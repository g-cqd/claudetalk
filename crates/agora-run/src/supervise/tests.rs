//! Tests of the supervisor: real child processes, run in-process through the core without setsid.

use std::collections::BTreeMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use agora_sched::settings::Qos;
use rustix::process::getuid;
use tempfile::TempDir;

use super::*;
use crate::jobdir::{create_log, log_path, work_root};
use crate::proc::{alive, signal_pid};

const LIMIT: u64 = 1 << 20;
const INLINE: Opts = Opts { detach: false };

struct Fixture {
    _tmp: TempDir,
    dir: JobDir,
    log: PathBuf,
    spec: JobSpec,
}

impl Fixture {
    fn new(argv: &[&str]) -> Fixture {
        let tmp = TempDir::new().expect("tempdir");
        let root = work_root(tmp.path());
        let dir = JobDir::new(&root, "t1").expect("job dir");
        dir.create().expect("create job dir");
        let log = log_path(&root, "t1");
        create_log(&log).expect("create log");
        let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
        // The daemon records canonical directories, and on macOS the temporary directory is under a symlink.
        let cwd = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let spec = JobSpec {
            task: "t1".to_owned(),
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: cwd.display().to_string(),
            env: BTreeMap::from([("PATH".to_owned(), path)]),
            log: log.display().to_string(),
            log_limit: LIMIT,
            qos: Qos::Default,
            exclusive: false,
            context_start: None,
        };
        let fixture = Fixture {
            _tmp: tmp,
            dir,
            log,
            spec,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        self.dir.write_spec(&self.spec).expect("write spec");
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).expect("read log")
    }
}

/// Runs the core in-process and checks that the returned status is the one on disk.
fn run(fixture: &Fixture) -> JobStatus {
    let status = run_job(&fixture.dir, INLINE).expect("supervise");
    assert_eq!(
        fixture.dir.read_status().expect("read status"),
        Some(status.clone())
    );
    status
}

/// Requests a cancel after 300 ms, from another thread, and reports when it was written.
fn cancel_later(dir: &JobDir) -> thread::JoinHandle<Instant> {
    let dir = dir.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        dir.request_cancel().expect("request cancel");
        Instant::now()
    })
}

#[test]
fn usage_errors_exit_two_without_supervising() {
    let owned = |parts: &[&str]| {
        parts
            .iter()
            .map(|p| (*p).to_owned())
            .collect::<Vec<String>>()
    };
    assert_eq!(main_from_args(&[]), 2);
    assert_eq!(main_from_args(&owned(&["--job-dir"])), 2);
    assert_eq!(main_from_args(&owned(&["--job-dir", "/x", "extra"])), 2);
    assert_eq!(main_from_args(&owned(&["--other", "/x"])), 2);
}

#[test]
fn clean_exit_is_recorded_as_started() {
    let fixture = Fixture::new(&["true"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    assert!(status.started && !status.cancelled && !status.over_limit);
    assert_eq!(status.note, "");
    let started = status.started_ms.expect("started_ms");
    assert!(status.ended_ms >= started);
    assert!(
        status.ended_ms - started < 200,
        "true ended {} ms after it started",
        status.ended_ms - started
    );
}

#[test]
fn exit_code_is_kept() {
    let fixture = Fixture::new(&["/bin/sh", "-c", "exit 3"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(3));
    assert!(status.started);
}

#[test]
fn death_by_signal_is_128_plus_signal() {
    let fixture = Fixture::new(&["/bin/sh", "-c", "kill -TERM $$"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(143));
    assert!(status.started && !status.cancelled);
}

#[test]
fn missing_program_is_127_with_note_and_log_line() {
    let fixture = Fixture::new(&["agora-no-such-program-m5"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(127));
    assert!(!status.started);
    assert!(
        status.note.starts_with("could not start: "),
        "{}",
        status.note
    );
    assert!(
        fixture
            .log_text()
            .starts_with("work: cannot run agora-no-such-program-m5: ")
    );
}

#[test]
fn empty_argv_is_127() {
    let fixture = Fixture::new(&[]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(127));
    assert!(!status.started);
}

#[test]
fn non_executable_file_is_126() {
    let mut fixture = Fixture::new(&["true"]);
    let program = Path::new(&fixture.spec.cwd).join("plain.txt");
    std::fs::write(&program, "not a program\n").expect("write file");
    fixture.spec.argv = vec![program.display().to_string()];
    fixture.save();
    let status = run(&fixture);
    assert_eq!(status.status, Some(126));
    assert!(!status.started);
    assert!(
        status.note.starts_with("could not start: "),
        "{}",
        status.note
    );
}

#[test]
fn gone_directory_is_recorded_as_unstarted() {
    let mut fixture = Fixture::new(&["true"]);
    let gone = format!("{}/gone", fixture.spec.cwd);
    fixture.spec.cwd = gone.clone();
    fixture.save();
    let status = run(&fixture);
    assert_eq!(status.status, None);
    assert!(!status.started);
    assert_eq!(status.note, format!("its directory {gone} is gone"));
    assert!(
        fixture
            .log_text()
            .contains("work: cannot run true: its directory ")
    );
}

#[test]
fn a_cwd_that_is_a_symlink_is_refused_as_unstarted() {
    let mut fixture = Fixture::new(&["true"]);
    let real = Path::new(&fixture.spec.cwd).join("real");
    std::fs::create_dir(&real).expect("create the real directory");
    let link = Path::new(&fixture.spec.cwd).join("link");
    std::os::unix::fs::symlink(&real, &link).expect("create the symlink");
    let (link, real) = (link.display().to_string(), real.display().to_string());
    fixture.spec.cwd = link.clone();
    fixture.save();
    let status = run(&fixture);
    assert_eq!(status.status, None);
    assert!(!status.started);
    assert_eq!(
        status.note,
        format!("its directory {link} now resolves to {real}")
    );
    assert!(
        fixture.log_text().contains(&format!(
            "work: cannot run true: its directory {link} now resolves to {real}\n"
        )),
        "{}",
        fixture.log_text()
    );
}

#[test]
fn a_canonical_cwd_runs_in_that_directory() {
    let fixture = Fixture::new(&["/bin/sh", "-c", "pwd -P"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    assert!(status.started);
    assert_eq!(status.note, "");
    assert_eq!(fixture.log_text().trim_end(), fixture.spec.cwd);
}

#[test]
fn exclusive_run_brackets_the_log_with_context_lines() {
    let mut fixture = Fixture::new(&["true"]);
    let start = "work: context at start: load 1.00, CPU 50% idle; running: no other job";
    fixture.spec.exclusive = true;
    fixture.spec.context_start = Some(start.to_owned());
    fixture.save();
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    let log = fixture.log_text();
    assert!(log.starts_with(&format!("{start}\n")), "{log}");
    assert!(log.contains("work: context at end: load "), "{log}");
    assert!(log.contains(" over the run\n"), "{log}");
}

#[test]
fn log_cap_stops_the_job_and_says_so() {
    let mut fixture = Fixture::new(&["/bin/sh", "-c", "yes | head -c 200000; sleep 5"]);
    fixture.spec.log_limit = 4096;
    fixture.save();
    let status = run(&fixture);
    assert!(status.over_limit);
    assert_eq!(status.status, Some(143));
    assert_eq!(
        status.note,
        "stopped: its log passed the limit of 4096 bytes (agora work settings --log-limit)"
    );
    assert!(!status.cancelled);
}

#[test]
fn a_writer_that_ignores_term_is_killed_once_the_log_overruns_the_cap() {
    // `yes` writes far faster than the cap grows by 16 MiB, so the KILL must come at once after the TERM, not after
    // the 5 s grace period. The cap is checked every 25 ms while the job runs, so a fast writer can add tens of MB
    // before the TERM; the property is a bounded overrun (cap + 128 MiB), not an exact one.
    let mut fixture = Fixture::new(&["/bin/sh", "-c", "trap '' TERM; yes"]);
    fixture.spec.log_limit = 65536;
    fixture.save();
    let started = Instant::now();
    let status = run(&fixture);
    let elapsed = started.elapsed();
    assert!(status.over_limit);
    assert!(!status.cancelled);
    assert_eq!(status.status, Some(137));
    assert!(elapsed < Duration::from_secs(3), "ended after {elapsed:?}");
    let size = std::fs::metadata(&fixture.log).expect("log metadata").len();
    let bound = 65536 + (128 << 20);
    assert!(
        size < bound,
        "log is {size} bytes, over the bound of {bound}"
    );
}

#[test]
fn cancel_file_stops_a_running_job_within_four_seconds() {
    let fixture = Fixture::new(&["/bin/sleep", "30"]);
    let canceller = cancel_later(&fixture.dir);
    let status = run(&fixture);
    let done = Instant::now();
    let requested = canceller.join().expect("canceller thread");
    assert!(status.cancelled);
    assert_eq!(status.status, Some(143));
    assert!(!status.over_limit);
    assert!(done.duration_since(requested) < Duration::from_secs(4));
}

#[test]
fn term_ignoring_job_is_killed_after_the_grace_period() {
    let fixture = Fixture::new(&["/bin/sh", "-c", "trap '' TERM; sleep 30"]);
    let canceller = cancel_later(&fixture.dir);
    let started = Instant::now();
    let status = run(&fixture);
    canceller.join().expect("canceller thread");
    assert!(status.cancelled);
    assert_eq!(status.status, Some(137));
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn supervisor_info_and_samples_are_written_for_a_running_job() {
    let fixture = Fixture::new(&["/bin/sleep", "2.5"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    let info = fixture
        .dir
        .read_info()
        .expect("read info")
        .expect("supervisor.json");
    assert_eq!(info.supervisor_pid, std::process::id());
    assert!(info.pid.is_some());
    assert_eq!(info.qos, "default");
    let sample = fixture
        .dir
        .read_sample()
        .expect("read sample")
        .expect("sample.json");
    assert!(sample.sampled_ms > 0);
}

#[test]
fn a_job_shorter_than_the_first_sample_still_reports_cpu_and_memory() {
    // It ends before the first sample (~1.5 s), so only getrusage can report its CPU time and peak RSS.
    let fixture = Fixture::new(&["/bin/sleep", "0.1"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    assert!(status.cpu_seconds.is_some(), "cpu_seconds from getrusage");
    assert!(
        status.rss_peak_kib.is_some_and(|kib| kib > 0),
        "rss_peak_kib from getrusage"
    );
}

#[test]
fn an_unreadable_spec_is_recorded_as_could_not_supervise() {
    let fixture = Fixture::new(&["true"]);
    std::fs::write(fixture.dir.dir.join("spec.json"), "{ not a spec").expect("corrupt spec");
    let error = run_job(&fixture.dir, INLINE).expect_err("a corrupt spec is an error");
    let status = fixture
        .dir
        .read_status()
        .expect("read status")
        .expect("status.json is written");
    assert_eq!(status.status, None);
    assert!(!status.started);
    assert!(
        status.note.starts_with("could not supervise: json: "),
        "{}",
        status.note
    );
    assert_eq!(status.note, format!("could not supervise: {error}"));
}

/// Restores a directory's permissions on drop, so the temporary directory can be removed whatever the test did.
struct Restore(PathBuf);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, Permissions::from_mode(0o755));
    }
}

#[test]
fn a_directory_that_cannot_be_entered_is_recorded_as_unstarted() {
    if getuid().as_raw() == 0 {
        eprintln!("skipped: root may enter any directory");
        return;
    }
    let mut fixture = Fixture::new(&["true"]);
    let locked = Path::new(&fixture.spec.cwd).join("locked");
    std::fs::create_dir(&locked).expect("create the directory");
    let _restore = Restore(locked.clone());
    std::fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("lock it");
    fixture.spec.cwd = locked.display().to_string();
    fixture.save();
    let status = run(&fixture);
    assert_eq!(status.status, None);
    assert!(!status.started);
    assert!(
        status.note.starts_with("cannot enter its directory "),
        "{}",
        status.note
    );
    assert!(
        fixture
            .log_text()
            .contains("work: cannot run true: cannot enter its directory ")
    );
}

/// Reads a pid written by a script, polling for at most 5 s; None if no pid appears.
fn read_pid_file(path: &Path) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pid = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        if pid.is_some() || Instant::now() >= deadline {
            return pid;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_grandchild_that_ignores_term_is_killed_with_the_group() {
    // The leader exits on TERM, but its background subshell ignores TERM and keeps sleeping: only the group KILL
    // after the leader's exit ends it.
    let script = "trap 'exit 0' TERM; (trap '' TERM; sleep 1000) & echo $! > grandchild.pid; wait";
    let fixture = Fixture::new(&["/bin/sh", "-c", script]);
    let pid_file = Path::new(&fixture.spec.cwd).join("grandchild.pid");
    let canceller = {
        let dir = fixture.dir.clone();
        thread::spawn(move || {
            // The pid is written after the trap is set, so the cancel's TERM reaches a leader that is ready.
            let pid = read_pid_file(&pid_file);
            dir.request_cancel().expect("request cancel");
            pid
        })
    };
    let status = run(&fixture);
    let grandchild = canceller
        .join()
        .expect("canceller thread")
        .expect("the script wrote the grandchild's pid");
    assert!(status.cancelled);
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(grandchild, None) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let still_alive = alive(grandchild, None);
    if still_alive {
        let _ = signal_pid(grandchild, Sig::Kill);
    }
    assert!(!still_alive, "grandchild {grandchild} survived the cancel");
}

#[test]
fn the_child_sees_only_the_spec_environment_and_pwd_is_its_directory() {
    // The test process has variables of its own; none may reach the child, whose environment is exactly the spec's
    // plus PWD set to its directory.
    assert!(
        std::env::vars_os().any(|(key, _)| key != "PATH"),
        "the test process has variables of its own"
    );
    let fixture = Fixture::new(&["/usr/bin/env"]);
    let status = run(&fixture);
    assert_eq!(status.status, Some(0));
    let mut seen: Vec<String> = fixture.log_text().lines().map(str::to_owned).collect();
    seen.sort();
    let mut expected = vec![
        format!("PATH={}", fixture.spec.env["PATH"]),
        format!("PWD={}", fixture.spec.cwd),
    ];
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn the_poll_is_25_ms_while_running_and_1_ms_once_term_is_sent() {
    assert_eq!(poll_period(false), Duration::from_millis(25));
    assert_eq!(poll_period(true), Duration::from_millis(1));
}

#[test]
fn end_context_line_has_its_exact_text() {
    assert_eq!(
        end_context(Some(1.5), Some(41.6)),
        "work: context at end: load 1.50, CPU 42% idle over the run"
    );
    assert_eq!(
        end_context(None, None),
        "work: context at end: load unknown, CPU idle unknown over the run"
    );
}
