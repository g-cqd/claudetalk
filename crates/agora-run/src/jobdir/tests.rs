use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn root() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("tempdir");
    let work = work_root(home.path());
    (home, work)
}

fn created(work: &Path) -> JobDir {
    let job = JobDir::new(work, "job-1").expect("valid name");
    job.create().expect("create");
    job
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

fn spec() -> JobSpec {
    JobSpec {
        task: "job-1".into(),
        argv: vec!["make".into(), "-j4".into()],
        cwd: "/tmp".into(),
        env: BTreeMap::from([("PWD".to_string(), "/tmp".to_string())]),
        log: "/home/work/logs/job-1.log".into(),
        log_limit: 1024,
        qos: agora_sched::settings::Qos::Background,
        exclusive: true,
        context_start: Some("work: context at start: job-1".into()),
    }
}

fn info() -> SupervisorInfo {
    SupervisorInfo {
        supervisor_pid: 4242,
        supervisor_start_s: Some(1_700_000_000),
        pid: Some(4343),
        pid_start_s: None,
        started_ms: 1_700_000_001_000,
        qos: "background".into(),
    }
}

fn sample() -> Sample {
    Sample {
        cpu_percent: 12.5,
        rss_kib: 2048,
        rss_peak_kib: 4096,
        sampled_ms: 1_700_000_002_000,
    }
}

fn status() -> JobStatus {
    JobStatus {
        status: Some(143),
        note: "cancelled by someone".into(),
        cpu_seconds: Some(1.25),
        rss_peak_kib: Some(512),
        started_ms: Some(1),
        ended_ms: 2,
        started: true,
        cancelled: true,
        over_limit: false,
    }
}

fn write_log(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write log");
}

#[test]
fn new_rejects_a_task_name_that_escapes_the_jobs_dir() {
    let (_home, work) = root();
    match JobDir::new(&work, "../x") {
        Err(RunError::Invalid(msg)) => {
            assert!(msg.starts_with("task id '../x' is not valid"), "{msg}")
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn new_places_the_dir_under_jobs() {
    let (_home, work) = root();
    let job = JobDir::new(&work, "job-1").expect("valid name");
    assert_eq!(job.dir, work.join("jobs").join("job-1"));
}

#[test]
fn create_makes_owner_only_dirs_and_is_repeatable() {
    let (_home, work) = root();
    let job = created(&work);
    assert_eq!(mode(&work), 0o700);
    assert_eq!(mode(&work.join("jobs")), 0o700);
    assert_eq!(mode(&job.dir), 0o700);
    job.create().expect("second create");
}

#[test]
fn create_refuses_a_symlinked_job_dir() {
    let (home, work) = root();
    std::fs::create_dir_all(work.join("jobs")).expect("jobs");
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    symlink(&elsewhere, work.join("jobs").join("job-1")).expect("symlink");
    let job = JobDir::new(&work, "job-1").expect("valid name");
    assert!(matches!(job.create(), Err(RunError::Invalid(_))));
}

#[test]
fn create_refuses_a_file_in_place_of_the_job_dir() {
    let (_home, work) = root();
    std::fs::create_dir_all(work.join("jobs")).expect("jobs");
    std::fs::write(work.join("jobs").join("job-1"), b"x").expect("file");
    let job = JobDir::new(&work, "job-1").expect("valid name");
    assert!(matches!(job.create(), Err(RunError::Invalid(_))));
}

#[test]
fn spec_round_trips() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_spec(&spec()).expect("write");
    assert_eq!(job.read_spec().expect("read"), spec());
}

#[test]
fn supervisor_info_round_trips() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_info(&info()).expect("write");
    assert_eq!(job.read_info().expect("read"), Some(info()));
}

#[test]
fn sample_round_trips() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_sample(&sample()).expect("write");
    assert_eq!(job.read_sample().expect("read"), Some(sample()));
}

#[test]
fn status_round_trips_and_a_second_write_replaces_it() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_status(&JobStatus {
        status: None,
        ..status()
    })
    .expect("first");
    job.write_status(&status()).expect("second");
    assert_eq!(job.read_status().expect("read"), Some(status()));
}

#[test]
fn missing_files_read_as_none_and_missing_spec_is_an_error() {
    let (_home, work) = root();
    let job = created(&work);
    assert_eq!(job.read_info().expect("info"), None);
    assert_eq!(job.read_sample().expect("sample"), None);
    assert_eq!(job.read_status().expect("status"), None);
    assert!(matches!(job.read_spec(), Err(RunError::Io(_))));
}

#[test]
fn atomic_write_leaves_only_the_target_file() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_status(&status()).expect("write");
    job.write_status(&status()).expect("rewrite");
    let mut names: Vec<String> = std::fs::read_dir(&job.dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["status.json".to_string()]);
    assert_eq!(mode(&job.dir.join("status.json")), 0o600);
}

#[test]
fn oversize_file_is_rejected() {
    let (_home, work) = root();
    let job = created(&work);
    std::fs::write(job.dir.join("status.json"), vec![b' '; (1 << 20) + 1]).expect("write");
    match job.read_status() {
        Err(RunError::Invalid(msg)) => assert_eq!(msg, "status.json is larger than 1 MiB"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn read_through_a_symlink_is_refused() {
    let (home, work) = root();
    let job = created(&work);
    let real = home.path().join("elsewhere.json");
    std::fs::write(&real, serde_json::to_vec(&status()).expect("json")).expect("write");
    symlink(&real, job.dir.join("status.json")).expect("symlink");
    assert!(job.read_status().is_err());
}

#[test]
fn a_directory_in_place_of_a_json_file_is_refused() {
    let (_home, work) = root();
    let job = created(&work);
    std::fs::create_dir(job.dir.join("sample.json")).expect("dir");
    match job.read_sample() {
        Err(RunError::Invalid(msg)) => assert_eq!(msg, "sample.json is not a regular file"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn cancel_flag_is_set_by_request_and_owner_only() {
    let (_home, work) = root();
    let job = created(&work);
    assert!(!job.cancel_requested());
    job.request_cancel().expect("request");
    assert!(job.cancel_requested());
    assert_eq!(mode(&job.dir.join("cancel")), 0o600);
}

#[test]
fn remove_deletes_the_dir_and_tolerates_a_missing_one() {
    let (_home, work) = root();
    let job = created(&work);
    job.write_spec(&spec()).expect("spec");
    job.remove().expect("remove");
    assert!(!job.dir.exists());
    job.remove().expect("remove again");
}

#[test]
fn remove_refuses_a_symlinked_job_dir() {
    let (home, work) = root();
    std::fs::create_dir_all(work.join("jobs")).expect("jobs");
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    symlink(&elsewhere, work.join("jobs").join("job-1")).expect("symlink");
    let job = JobDir::new(&work, "job-1").expect("valid name");
    assert!(matches!(job.remove(), Err(RunError::Invalid(_))));
    assert!(elsewhere.is_dir());
}

#[test]
fn paths_are_built_from_home_and_task() {
    let home = Path::new("/home/agora");
    assert_eq!(work_root(home), PathBuf::from("/home/agora/work"));
    assert_eq!(
        log_path(&work_root(home), "job-1"),
        PathBuf::from("/home/agora/work/logs/job-1.log")
    );
}

#[test]
fn create_log_is_owner_only_and_truncates_an_existing_log() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = log_path(&work, "job-1");
    let mut first = create_log(&path).expect("first open");
    first.write_all(b"old output\n").expect("write");
    drop(first);
    let _second = create_log(&path).expect("second open");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&work.join("logs")), 0o700);
}

#[test]
fn create_log_refuses_a_symlink() {
    let (home, work) = root();
    std::fs::create_dir_all(work.join("logs")).expect("logs");
    let real = home.path().join("victim.log");
    std::fs::write(&real, b"keep").expect("victim");
    let path = log_path(&work, "job-1");
    symlink(&real, &path).expect("symlink");
    assert!(create_log(&path).is_err());
    assert_eq!(std::fs::read(&real).expect("read"), b"keep");
}

#[test]
fn tail_splits_on_crlf_cr_and_lf() {
    let (_home, work) = root();
    let path = work.join("a.log");
    std::fs::create_dir_all(&work).expect("work");
    write_log(&path, "a\r\nb\rc\nd\n");
    assert_eq!(tail(&path, 10), vec!["a", "b", "c", "d"]);
    assert_eq!(tail(&path, 2), vec!["c", "d"]);
}

#[test]
fn tail_gives_all_lines_when_fewer_exist_and_keeps_blank_lines() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = work.join("a.log");
    write_log(&path, "x\ny\n");
    assert_eq!(tail(&path, 5), vec!["x", "y"]);
    write_log(&path, "a\n\n");
    assert_eq!(tail(&path, 5), vec!["a", ""]);
}

#[test]
fn tail_keeps_a_final_partial_line() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = work.join("a.log");
    write_log(&path, "first\nlast-no-newline");
    assert_eq!(tail(&path, 1), vec!["last-no-newline"]);
}

#[test]
fn tail_escapes_control_characters() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = work.join("a.log");
    write_log(&path, "ok\u{1b}[31m\n");
    assert_eq!(tail(&path, 1), vec!["ok\\x1b[31m"]);
}

#[test]
fn tail_of_a_large_log_returns_the_last_lines() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = work.join("big.log");
    let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    assert!(text.len() > 64 * 1024);
    write_log(&path, &text);
    assert_eq!(
        tail(&path, 3),
        vec!["line 19997", "line 19998", "line 19999"]
    );
}

#[test]
fn tail_of_a_missing_file_or_zero_lines_is_empty() {
    let (_home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let path = work.join("a.log");
    assert!(tail(&path, 5).is_empty());
    write_log(&path, "a\n");
    assert!(tail(&path, 0).is_empty());
}

#[test]
fn tail_refuses_a_symlinked_log() {
    let (home, work) = root();
    std::fs::create_dir_all(&work).expect("work");
    let real = home.path().join("secret.log");
    write_log(&real, "secret\n");
    let path = work.join("a.log");
    symlink(&real, &path).expect("symlink");
    assert!(tail(&path, 5).is_empty());
}

#[test]
fn splits_exactly_at_python_splitlines_breaks() {
    assert_eq!(
        split_py_lines("a\x0bb\x0cc\x1cd\x1de\x1ef\u{85}g\u{2028}h\u{2029}i"),
        vec!["a", "b", "c", "d", "e", "f", "g", "h", "i"]
    );
    assert!(split_py_lines("").is_empty());
    assert_eq!(split_py_lines("\n"), vec![""]);
    assert_eq!(split_py_lines("\r\n"), vec![""]);
    assert_eq!(split_py_lines("a\r\n"), vec!["a"]);
}
