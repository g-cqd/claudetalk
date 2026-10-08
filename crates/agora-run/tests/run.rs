//! Integration tests for agora-run through its public API: program lookup, QoS wrapping, job directories, log tails,
//! process probes and machine probes. Every child process runs under a guard that kills it on drop, and every wait is
//! bounded by `WAIT`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agora_run::RunError;
use agora_run::jobdir::{
    JobDir, JobSpec, JobStatus, Sample, SupervisorInfo, create_log, log_path, tail, work_root,
};
use agora_run::locate::locate;
use agora_run::machine::{FakeMachine, Machine, RealMachine, effective_cpus};
use agora_run::proc::{Sampler, Sig, alive, signal_group, signal_pid, start_time_s};
use agora_run::qos::wrap;
use agora_sched::admit::{MachineSnapshot, MemLevel, Signals};
use agora_sched::settings::Qos;

/// Longest any single wait may block before the test fails.
const WAIT: Duration = Duration::from_secs(5);
/// Interval between probes while waiting.
const STEP: Duration = Duration::from_millis(10);
const MIB: usize = 1024 * 1024;

/// A child that is killed and reaped on drop. A group leader also has its process group killed, so a background
/// member it started does not outlive a failing test.
struct Guard {
    child: Child,
    group: bool,
}

impl Guard {
    fn spawn(cmd: &mut Command) -> Guard {
        Guard {
            child: cmd.spawn().expect("spawn child"),
            group: false,
        }
    }

    fn group_leader(cmd: &mut Command) -> Guard {
        cmd.process_group(0);
        Guard {
            child: cmd.spawn().expect("spawn group leader"),
            group: true,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.group {
            let _ = signal_group(self.pid(), Sig::Kill);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `probe` every `STEP` until it yields a value; None once `WAIT` has passed.
fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(STEP);
    }
}

fn wait_exit(child: &mut Child) -> Option<ExitStatus> {
    wait_for(|| child.try_wait().ok().flatten())
}

fn holds(mut cond: impl FnMut() -> bool) -> bool {
    wait_for(|| cond().then_some(())).is_some()
}

/// A `sleep 30` child, killed on drop.
fn sleeper() -> Guard {
    Guard::spawn(Command::new("sleep").arg("30"))
}

/// `sh -c 'sleep 30 & echo $!; wait'` as a group leader. Returns the guard and the pid of the background sleep,
/// which the script echoes first, so the background member exists before the caller goes on.
fn group_with_background_sleep() -> (Guard, u32) {
    let mut guard = Guard::group_leader(
        Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped()),
    );
    let stdout = guard.child.stdout.take().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the background pid");
    let background: u32 = line.trim().parse().expect("background pid");
    (guard, background)
}

/// The pid of a child that has run to completion and been reaped.
fn reaped_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    assert!(wait_exit(&mut child).is_some_and(|status| status.success()));
    pid
}

// ---------------------------------------------------------------------------------------------------------------------
// locate: execvp emulation

const SCRIPT: &[u8] = b"#!/bin/sh\nexit 0\n";

fn file_with_mode(path: &Path, body: &[u8], mode: u32) {
    fs::write(path, body).expect("write file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
}

fn on_path(dir: &Path) -> String {
    dir.display().to_string()
}

#[test]
fn locate_reports_enoent_for_a_program_missing_from_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("no-such-tool", dir.path(), Some(&on_path(dir.path()))),
        Err(2)
    );
}

#[test]
fn locate_reports_enoent_for_an_absolute_path_that_does_not_exist() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("/no/such/agora-tool", dir.path(), Some("/bin")),
        Err(2)
    );
}

#[test]
fn locate_reports_eacces_for_a_file_without_an_execute_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), SCRIPT, 0o644);
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(13)
    );
}

#[test]
fn locate_reports_eacces_for_a_directory_named_like_the_program() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("tool")).expect("mkdir");
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(13)
    );
}

#[test]
fn locate_reports_enoexec_for_an_executable_with_neither_shebang_nor_binary_magic() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), b"echo hi\n", 0o755);
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(8)
    );
}

#[test]
fn locate_finds_sh_through_the_given_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("sh", dir.path(), Some("/bin:/usr/bin")),
        Ok(PathBuf::from("/bin/sh"))
    );
}

#[test]
fn locate_resolves_a_dot_slash_program_against_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("x"), SCRIPT, 0o755);
    assert_eq!(
        locate("./x", dir.path(), Some("/no-such-dir")),
        Ok(dir.path().join("x"))
    );
}

#[test]
fn locate_treats_an_empty_path_entry_as_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), SCRIPT, 0o755);
    assert_eq!(
        locate("tool", dir.path(), Some("")),
        Ok(dir.path().join("tool"))
    );
    assert_eq!(
        locate("tool", dir.path(), Some(":/no-such-dir")),
        Ok(dir.path().join("tool"))
    );
}

#[test]
fn locate_without_a_path_searches_the_system_dirs_not_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("agora-cwd-only-tool"), SCRIPT, 0o755);
    assert_eq!(locate("agora-cwd-only-tool", dir.path(), None), Err(2));
}

// ---------------------------------------------------------------------------------------------------------------------
// qos::wrap

const ARGS: [&str; 3] = ["-v", "two words", ""];

fn args() -> Vec<String> {
    ARGS.iter().map(|arg| (*arg).to_owned()).collect()
}

fn strs(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

/// The argv of a job that is not wrapped: the program, then its arguments.
fn bare_argv() -> Vec<&'static str> {
    std::iter::once("/bin/echo").chain(ARGS).collect()
}

/// `prefix` followed by the bare argv.
fn prefixed(prefix: &[&'static str]) -> Vec<&'static str> {
    prefix.iter().copied().chain(bare_argv()).collect()
}

#[test]
fn exclusive_jobs_run_unwrapped_at_every_qos() {
    for qos in [Qos::Utility, Qos::Default, Qos::Background] {
        let (argv, recorded) = wrap(qos, true, Path::new("/bin/echo"), &args());
        assert_eq!(recorded, "default", "exclusive job at {qos:?}");
        assert_eq!(strs(&argv), bare_argv(), "exclusive job at {qos:?}");
    }
}

#[test]
fn default_qos_runs_unwrapped_and_records_default() {
    let (argv, recorded) = wrap(Qos::Default, false, Path::new("/bin/echo"), &args());
    assert_eq!(recorded, "default");
    assert_eq!(strs(&argv), bare_argv());
}

#[test]
fn a_wrapped_job_keeps_its_program_and_arguments_as_the_final_argv_entries() {
    for qos in [Qos::Utility, Qos::Background] {
        let (argv, recorded) = wrap(qos, false, Path::new("/bin/echo"), &args());
        let argv = strs(&argv);
        if recorded == "default" {
            assert_eq!(argv, bare_argv(), "{qos:?} ran unwrapped");
        } else {
            assert_eq!(recorded, qos.as_str());
            assert!(argv.len() > bare_argv().len(), "{qos:?} has a wrapper");
            assert_eq!(
                &argv[argv.len() - bare_argv().len()..],
                bare_argv().as_slice()
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_utility_job_runs_under_taskpolicy_dash_c_utility_when_installed() {
    let (argv, recorded) = wrap(Qos::Utility, false, Path::new("/bin/echo"), &args());
    if Path::new("/usr/sbin/taskpolicy").is_file() {
        assert_eq!(recorded, "utility");
        assert_eq!(
            strs(&argv),
            prefixed(&["/usr/sbin/taskpolicy", "-c", "utility"])
        );
    } else {
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), bare_argv());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_background_job_runs_under_taskpolicy_dash_c_background_when_installed() {
    let (argv, recorded) = wrap(Qos::Background, false, Path::new("/bin/echo"), &args());
    if Path::new("/usr/sbin/taskpolicy").is_file() {
        assert_eq!(recorded, "background");
        assert_eq!(
            strs(&argv),
            prefixed(&["/usr/sbin/taskpolicy", "-c", "background"])
        );
    } else {
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), bare_argv());
    }
}

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

// ---------------------------------------------------------------------------------------------------------------------
// proc: liveness, signalling, sampling

#[test]
fn alive_is_true_for_a_running_child_with_or_without_its_start_time() {
    let child = sleeper();
    let start = start_time_s(child.pid()).expect("a running child has a start time");
    assert!(alive(child.pid(), None));
    assert!(alive(child.pid(), Some(start)));
}

#[test]
fn alive_is_false_when_the_recorded_start_time_does_not_match() {
    let child = sleeper();
    let start = start_time_s(child.pid()).expect("start time");
    assert!(!alive(child.pid(), Some(start - 100)));
}

#[test]
fn alive_is_false_once_the_child_is_killed_and_reaped() {
    let mut child = sleeper();
    let pid = child.pid();
    let start = start_time_s(pid).expect("start time");
    child.child.kill().expect("kill the child");
    assert!(wait_exit(&mut child.child).is_some(), "the child is reaped");
    assert!(!alive(pid, None));
    assert!(!alive(pid, Some(start)));
}

#[test]
fn signal_to_a_reaped_pid_fails_with_no_such_process() {
    let err = signal_pid(reaped_pid(), Sig::Term).expect_err("no process has that pid");
    assert_eq!(
        err.raw_os_error(),
        Some(rustix::io::Errno::SRCH.raw_os_error())
    );
}

#[test]
fn signal_group_ends_the_leader_and_the_background_member_it_started() {
    let (mut leader, background) = group_with_background_sleep();
    assert!(alive(background, None), "background sleep runs first");
    signal_group(leader.pid(), Sig::Term).expect("signal the group");
    let status = wait_exit(&mut leader.child).expect("group leader exits");
    assert_eq!(
        status.signal(),
        Some(rustix::process::Signal::TERM.as_raw())
    );
    assert!(
        holds(|| !alive(background, None)),
        "the background member gets the group signal"
    );
}

#[test]
fn sampler_reports_resident_memory_for_a_busy_child() {
    let busy = Guard::spawn(Command::new("sh").args(["-c", "while :; do :; done"]));
    let sample = Sampler::new()
        .sample_tree(busy.pid())
        .expect("a live child is sampled");
    assert!(sample.rss_kib > 0, "rss {} KiB", sample.rss_kib);
}

#[test]
fn sampler_reports_cpu_for_a_busy_child_once_two_samples_span_an_interval() {
    let busy = Guard::spawn(Command::new("sh").args(["-c", "while :; do :; done"]));
    let mut sampler = Sampler::new();
    let cpu = wait_for(|| {
        sampler
            .sample_tree(busy.pid())
            .filter(|sample| sample.cpu_percent > 0.0)
    });
    assert!(
        cpu.is_some(),
        "a busy child reports cpu usage within {WAIT:?}"
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// machine: the fake double and the real probes

#[test]
fn fake_machine_reports_its_documented_defaults() {
    let snapshot = FakeMachine::new().snapshot();
    assert_eq!(
        snapshot,
        MachineSnapshot {
            cpus: 8.0,
            total_memory_kib: Some(16 * 1024 * 1024),
            disk_free_kib: Some(100 * 1024 * 1024),
            signals: Some(Signals {
                idle_pct: Some(90.0),
                memory_level: Some(MemLevel::Normal),
                load1: None,
                load_gate: false,
                memory_used_kib: Some(4 * 1024 * 1024),
            }),
        }
    );
}

#[test]
fn fake_machine_set_replaces_every_field_of_the_snapshot() {
    let machine = FakeMachine::new();
    let custom = MachineSnapshot {
        cpus: 2.0,
        total_memory_kib: None,
        disk_free_kib: None,
        signals: None,
    };
    machine.set(custom.clone());
    assert_eq!(machine.snapshot(), custom);
}

#[test]
fn fake_machine_update_changes_only_the_field_it_names() {
    let machine = FakeMachine::new();
    machine.update(|snap| snap.cpus = 2.0);
    let mut expected = FakeMachine::new().snapshot();
    expected.cpus = 2.0;
    assert_eq!(machine.snapshot(), expected);
}

#[test]
fn fake_machine_update_of_signals_is_visible_to_the_next_snapshot() {
    let machine = FakeMachine::new();
    machine.update(|snap| {
        if let Some(signals) = snap.signals.as_mut() {
            signals.memory_level = Some(MemLevel::Critical);
            signals.load1 = Some(3.5);
        }
    });
    let signals = machine.snapshot().signals.expect("signals stay set");
    assert_eq!(signals.memory_level, Some(MemLevel::Critical));
    assert_eq!(signals.load1, Some(3.5));
    assert_eq!(
        signals.idle_pct,
        Some(90.0),
        "untouched fields keep their value"
    );
}

#[test]
fn real_machine_has_no_idle_reading_until_a_baseline_exists() {
    let home = tempfile::tempdir().expect("tempdir");
    let machine = RealMachine::new(home.path().to_path_buf());
    let first = machine.snapshot().signals.expect("signals");
    assert_eq!(first.idle_pct, None);
    // sysinfo measures CPU usage only across a span of at least MINIMUM_CPU_UPDATE_INTERVAL (200 ms).
    std::thread::sleep(Duration::from_millis(250));
    let second = machine.snapshot().signals.expect("signals");
    let idle = second.idle_pct.expect("the second snapshot reports idle");
    assert!((0.0..=100.0).contains(&idle), "idle {idle}");
}

#[test]
fn real_machine_reports_free_disk_and_memory_figures_for_its_home() {
    let home = tempfile::tempdir().expect("tempdir");
    let snapshot = RealMachine::new(home.path().to_path_buf()).snapshot();
    assert!(snapshot.disk_free_kib.is_some_and(|kib| kib > 0));
    assert!(snapshot.total_memory_kib.is_some_and(|kib| kib > 0));
    let signals = snapshot.signals.expect("signals");
    assert!(signals.memory_used_kib.is_some_and(|kib| kib > 0));
}

#[test]
fn real_machine_reports_a_positive_cpu_count() {
    let home = tempfile::tempdir().expect("tempdir");
    let snapshot = RealMachine::new(home.path().to_path_buf()).snapshot();
    assert!(snapshot.cpus > 0.0, "cpus {}", snapshot.cpus);
}

#[test]
fn real_machine_has_no_disk_figure_for_a_home_that_does_not_exist() {
    let home = tempfile::tempdir().expect("tempdir");
    let missing = RealMachine::new(home.path().join("no-such-home")).snapshot();
    assert_eq!(missing.disk_free_kib, None);
}

#[test]
fn real_machine_enables_the_load_gate_only_on_linux() {
    let home = tempfile::tempdir().expect("tempdir");
    let signals = RealMachine::new(home.path().to_path_buf())
        .snapshot()
        .signals
        .expect("signals");
    assert_eq!(signals.load_gate, cfg!(target_os = "linux"));
}

#[test]
fn effective_cpus_is_positive_and_no_more_than_the_host_parallelism() {
    // A fractional cgroup quota can bring the count below one, so only positivity is certain.
    let host = std::thread::available_parallelism().map_or(1, NonZeroUsize::get) as f64;
    let cpus = effective_cpus();
    assert!(cpus > 0.0 && cpus <= host, "{cpus} of a host with {host}");
}
