use super::*;

// ---------------------------------------------------------------------------------------------------------------------
// jobdir: names, layout, records, cancel, remove, logs and tail

fn temp_work() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("tempdir");
    let work = work_root(home.path());
    (home, work)
}

/// A job directory created the way the daemon creates it.
fn job_in(work: &Path, task: &str) -> JobDir {
    let job = JobDir::new(work, task).expect("valid task id");
    job.create().expect("create the job dir");
    job
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// The sorted names directly inside `dir`.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn spec() -> JobSpec {
    JobSpec {
        task: "job-1".into(),
        argv: vec!["make".into(), "-j4".into()],
        cwd: "/tmp".into(),
        env: BTreeMap::from([("LANG".to_owned(), "C".to_owned())]),
        log: "/home/work/logs/job-1.log".into(),
        log_limit: 1024,
        qos: Qos::Background,
        exclusive: true,
        context_start: Some("work: context at start: job-1".into()),
    }
}

fn info() -> SupervisorInfo {
    SupervisorInfo {
        supervisor_pid: 4242,
        supervisor_start_s: Some(1_700_000_000),
        pid: Some(4243),
        pid_start_s: None,
        started_ms: 1_700_000_000_500,
        qos: "utility".into(),
    }
}

fn sample() -> Sample {
    Sample {
        cpu_percent: 12.5,
        rss_kib: 2048,
        rss_peak_kib: 4096,
        sampled_ms: 1_700_000_001_000,
    }
}

fn status() -> JobStatus {
    JobStatus {
        status: Some(143),
        note: "stopped: its log passed the limit".into(),
        cpu_seconds: Some(1.25),
        rss_peak_kib: Some(4096),
        started_ms: Some(1_700_000_000_500),
        ended_ms: 1_700_000_002_000,
        started: true,
        cancelled: false,
        over_limit: true,
    }
}

#[test]
fn job_dir_rejects_task_ids_that_are_not_plain_names() {
    let (_home, work) = temp_work();
    let long = "x".repeat(129);
    for bad in [
        "",
        "../escape",
        "a/b",
        ".hidden",
        "-flag",
        "has space",
        long.as_str(),
    ] {
        let err = JobDir::new(&work, bad).expect_err(bad);
        assert!(
            matches!(&err, RunError::Invalid(msg) if msg.starts_with("task id")),
            "{bad:?} gave {err:?}"
        );
    }
}

#[test]
fn job_dir_accepts_names_up_to_128_characters_and_places_them_under_jobs() {
    let (_home, work) = temp_work();
    let longest = "a".repeat(128);
    for name in ["job-1", "job.1:a@b-c_d", longest.as_str()] {
        let job = JobDir::new(&work, name).expect("valid name");
        assert_eq!(job.dir, work.join("jobs").join(name));
    }
}

#[test]
fn job_dir_new_creates_nothing_until_create_is_called() {
    let (_home, work) = temp_work();
    let job = JobDir::new(&work, "job-1").expect("valid name");
    assert!(!job.dir.exists());
    assert!(!work.exists());
}

#[test]
fn create_makes_the_work_jobs_and_job_directories_owner_only_and_is_repeatable() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.create().expect("a second create succeeds");
    let jobs = work.join("jobs");
    for dir in [&work, &jobs, &job.dir] {
        assert_eq!(mode_of(dir), 0o700, "{}", dir.display());
    }
}

#[test]
fn job_spec_is_read_back_exactly_as_written() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_spec(&spec()).expect("write");
    assert_eq!(job.read_spec().expect("read"), spec());
}

#[test]
fn supervisor_info_is_read_back_exactly_as_written() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_info(&info()).expect("write");
    assert_eq!(job.read_info().expect("read"), Some(info()));
}

#[test]
fn sample_is_read_back_exactly_as_written() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_sample(&sample()).expect("write");
    assert_eq!(job.read_sample().expect("read"), Some(sample()));
}

#[test]
fn job_status_is_read_back_exactly_as_written() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_status(&status()).expect("write");
    assert_eq!(job.read_status().expect("read"), Some(status()));
}

#[test]
fn records_never_written_read_as_none_and_a_missing_spec_is_an_error() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    assert_eq!(job.read_info().expect("read"), None);
    assert_eq!(job.read_sample().expect("read"), None);
    assert_eq!(job.read_status().expect("read"), None);
    assert!(matches!(job.read_spec(), Err(RunError::Io(_))));
}

#[test]
fn records_leave_only_their_named_files_in_the_job_dir() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_spec(&spec()).expect("spec");
    job.write_info(&info()).expect("info");
    job.write_sample(&sample()).expect("sample");
    job.write_status(&status()).expect("status");
    job.request_cancel().expect("cancel");
    assert_eq!(
        names_in(&job.dir),
        [
            "cancel",
            "sample.json",
            "spec.json",
            "status.json",
            "supervisor.json"
        ]
    );
}

#[test]
fn a_second_status_write_replaces_the_first_and_leaves_no_temp_file() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_status(&status()).expect("first write");
    let later = JobStatus {
        status: Some(0),
        note: String::new(),
        ..status()
    };
    job.write_status(&later).expect("second write");
    assert_eq!(job.read_status().expect("read"), Some(later));
    assert_eq!(names_in(&job.dir), ["status.json"]);
}

#[test]
fn failed_write_reports_an_error_and_leaves_no_temp_file() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    // A directory in the target's place makes the final rename fail.
    fs::create_dir(job.dir.join("status.json")).expect("blocker");
    let err = job
        .write_status(&status())
        .expect_err("rename over a directory fails");
    assert!(matches!(err, RunError::Io(_)), "{err:?}");
    assert_eq!(names_in(&job.dir), ["status.json"]);
}

#[test]
fn records_read_through_a_symlink_are_refused() {
    let (home, work) = temp_work();
    let job = job_in(&work, "job-1");
    let elsewhere = home.path().join("elsewhere.json");
    fs::write(&elsewhere, serde_json::to_vec(&status()).expect("json")).expect("target");
    symlink(&elsewhere, job.dir.join("status.json")).expect("symlink");
    assert!(matches!(job.read_status(), Err(RunError::Io(_))));
}

/// A valid spec record padded with trailing spaces to exactly `len` bytes.
fn spec_padded_to(len: usize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&spec()).expect("json");
    bytes.resize(len, b' ');
    bytes
}

#[test]
fn record_of_exactly_one_mebibyte_is_read() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    fs::write(job.dir.join("spec.json"), spec_padded_to(MIB)).expect("plant");
    assert_eq!(job.read_spec().expect("read"), spec());
}

#[test]
fn record_one_byte_over_one_mebibyte_is_rejected() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    fs::write(job.dir.join("spec.json"), spec_padded_to(MIB + 1)).expect("plant");
    let err = job.read_spec().expect_err("over the cap");
    assert!(
        matches!(&err, RunError::Invalid(msg) if msg.contains("larger than 1 MiB")),
        "{err:?}"
    );
}

#[test]
fn record_that_is_a_fifo_is_refused_without_waiting_for_a_writer() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    let fifo = job.dir.join("status.json");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo made the fifo");
    let reader = job.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(reader.read_status());
    });
    let result = rx
        .recv_timeout(WAIT)
        .expect("the read returns without a writer");
    assert!(
        matches!(&result, Err(RunError::Invalid(_))),
        "a fifo is not a record: {result:?}"
    );
}

#[test]
fn cancel_flag_is_clear_until_requested_and_stays_set_after() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    assert!(!job.cancel_requested());
    job.request_cancel().expect("request");
    assert!(job.cancel_requested());
    job.request_cancel().expect("a second request");
    assert!(job.cancel_requested());
}

#[test]
fn remove_deletes_the_job_directory_and_everything_in_it() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.write_spec(&spec()).expect("spec");
    job.request_cancel().expect("cancel");
    job.remove().expect("remove");
    assert!(!job.dir.exists());
}

#[test]
fn remove_leaves_sibling_job_directories_alone() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    let sibling = job_in(&work, "job-2");
    sibling.write_spec(&spec()).expect("sibling spec");
    job.remove().expect("remove");
    assert_eq!(sibling.read_spec().expect("sibling read"), spec());
}

#[test]
fn remove_of_an_already_removed_job_dir_succeeds() {
    let (_home, work) = temp_work();
    let job = job_in(&work, "job-1");
    job.remove().expect("first remove");
    job.remove().expect("second remove");
}

#[test]
fn log_path_is_named_after_the_task_under_logs() {
    let work = Path::new("/home/agora/work");
    assert_eq!(
        log_path(work, "job-1"),
        Path::new("/home/agora/work/logs/job-1.log")
    );
}

#[test]
fn create_log_is_owner_only_in_an_owner_only_logs_dir() {
    let (_home, work) = temp_work();
    job_in(&work, "job-1");
    let path = log_path(&work, "job-1");
    create_log(&path).expect("create log");
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(mode_of(&work.join("logs")), 0o700);
}

#[test]
fn create_log_truncates_what_an_earlier_run_wrote() {
    let (_home, work) = temp_work();
    job_in(&work, "job-1");
    let path = log_path(&work, "job-1");
    let mut first = create_log(&path).expect("first create");
    first.write_all(b"old output\n").expect("write");
    drop(first);
    let mut second = create_log(&path).expect("second create");
    assert_eq!(fs::metadata(&path).expect("metadata").len(), 0);
    second.write_all(b"new\n").expect("write");
    drop(second);
    assert_eq!(fs::read_to_string(&path).expect("read"), "new\n");
}

/// A log file holding `text` in a fresh temp dir.
fn log_with(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    fs::write(&path, text).expect("write log");
    (dir, path)
}

#[test]
fn tail_splits_lines_on_crlf_cr_and_lf_like_python_splitlines() {
    let (_dir, path) = log_with("a\r\n\r\nb\rc\nd\r\n");
    assert_eq!(tail(&path, 10), vec!["a", "", "b", "c", "d"]);
}

#[test]
fn tail_returns_only_the_last_requested_lines() {
    let (_dir, path) = log_with("1\n2\n3\n4\n5\n");
    assert_eq!(tail(&path, 2), vec!["4", "5"]);
}

#[test]
fn tail_of_a_missing_log_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(tail(&dir.path().join("absent.log"), 5).is_empty());
}

#[test]
fn tail_of_zero_lines_is_empty() {
    let (_dir, path) = log_with("1\n2\n");
    assert!(tail(&path, 0).is_empty());
}

#[test]
fn tail_considers_only_the_final_64_kib_of_a_long_log() {
    // 20 000 lines of 13 bytes is about 260 KiB; the final 64 KiB holds about 5 040 of them.
    let mut text = String::from("EARLY\n");
    for i in 0..20_000 {
        text.push_str(&format!("filler-{i:05}\n"));
    }
    text.push_str("last-line\n");
    let (_dir, path) = log_with(&text);
    let lines = tail(&path, 1_000_000);
    assert!(!lines.iter().any(|line| line == "EARLY"));
    assert!(
        (5_000..=5_100).contains(&lines.len()),
        "{} lines in the final 64 KiB",
        lines.len()
    );
    let last_three: Vec<&str> = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(last_three, ["filler-19998", "filler-19999", "last-line"]);
}

#[test]
fn tail_escapes_control_characters_and_keeps_printable_unicode() {
    let (_dir, path) = log_with("ok\u{1b}[31m\tbel\u{7} caf\u{e9}\n");
    assert_eq!(tail(&path, 1), vec!["ok\\x1b[31m\\tbel\\x07 caf\u{e9}"]);
}

#[test]
fn a_child_output_written_through_create_log_tails_as_its_lines() {
    let (_home, work) = temp_work();
    job_in(&work, "job-1");
    let path = log_path(&work, "job-1");
    let out = create_log(&path).expect("create log");
    let mut child = Guard::spawn(
        Command::new("sh")
            .args(["-c", "printf 'one\\r\\ntwo\\n'"])
            .stdout(out),
    );
    let status = wait_exit(&mut child.child).expect("child exits");
    assert!(status.success());
    assert_eq!(tail(&path, 10), vec!["one", "two"]);
}
