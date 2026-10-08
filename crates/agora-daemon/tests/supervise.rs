//! The supervisor binary (D8, D9) run directly on prepared job directories: real child processes, real signals, a real
//! session. Each test starts `agora-supervise --job-dir <dir>` and waits for it with a bounded loop. A Drop guard kills
//! the job's process group and the supervisor, so a failing test leaves nothing behind.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agora_run::jobdir::{JobDir, JobSpec, JobStatus, create_log, log_path, work_root};
use agora_run::proc::{Sig, signal_group, signal_pid};
use agora_sched::settings::Qos;
use rustix::process::{Pid, getsid};
use tempfile::TempDir;

/// Interval between checks of a child, a file or a log.
const POLL: Duration = Duration::from_millis(20);
/// Bound for a job to end, for the tests that do not ask for a tighter one.
const ENDS_WITHIN: Duration = Duration::from_secs(15);
/// Bound for the supervisor to publish supervisor.json, which it does once the command is running.
const STARTS_WITHIN: Duration = Duration::from_secs(10);
/// The log cap of the log-cap test, as the spec sets it.
const SMALL_LOG_LIMIT: u64 = 65_536;
/// The log cap of every other test: far above anything they print.
const LOG_LIMIT: u64 = 1 << 20;
/// The PATH every job gets, so its shell finds `sleep`, `head` and `yes`.
const PATH: &str = "/usr/bin:/bin";
/// Set in the supervisor's own environment, so the supervisor inherits it; its jobs must not.
const LEAK: &str = "AGORA_TEST_LEAK";

/// One prepared job directory under a fresh home in /tmp, and the supervisor once it is started.
struct Job {
    _home: TempDir,
    cwd: PathBuf,
    dir: JobDir,
    log: PathBuf,
    supervisor: Option<Child>,
}

impl Job {
    /// A job that runs `argv` in its home directory, with the default log cap and only PATH in its environment.
    fn new(argv: &[&str]) -> Job {
        Job::with(argv, |_| {})
    }

    /// The same job, with `tweak` applied to its spec before the spec is written.
    fn with(argv: &[&str], tweak: impl FnOnce(&mut JobSpec)) -> Job {
        let home = tempfile::Builder::new()
            .prefix("agora-supervise-")
            .tempdir_in("/tmp")
            .expect("a temp home under /tmp");
        // The supervisor refuses a cwd that is not its own canonical path; on macOS /tmp is a symlink to /private/tmp.
        let cwd = fs::canonicalize(home.path()).expect("canonicalize the temp home");
        let root = work_root(home.path());
        let dir = JobDir::new(&root, "t1").expect("a job dir for t1");
        dir.create().expect("create the job dir");
        let log = log_path(&root, "t1");
        create_log(&log).expect("create the log the daemon would create");
        let mut spec = JobSpec {
            task: "t1".to_owned(),
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: cwd.display().to_string(),
            env: BTreeMap::from([("PATH".to_owned(), PATH.to_owned())]),
            log: log.display().to_string(),
            log_limit: LOG_LIMIT,
            qos: Qos::Default,
            exclusive: false,
            context_start: None,
        };
        tweak(&mut spec);
        dir.write_spec(&spec).expect("write spec.json");
        Job {
            _home: home,
            cwd: PathBuf::from(&spec.cwd),
            dir,
            log,
            supervisor: None,
        }
    }

    /// Starts the supervisor binary on the job directory, with stdin, stdout and stderr closed or discarded.
    fn start(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_agora-supervise"))
            .arg("--job-dir")
            .arg(&self.dir.dir)
            .env(LEAK, "leaked")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the supervisor binary");
        self.supervisor = Some(child);
    }

    fn supervisor(&self) -> &Child {
        self.supervisor
            .as_ref()
            .expect("the supervisor was started")
    }

    fn supervisor_pid(&self) -> u32 {
        self.supervisor().id()
    }

    /// Waits until the supervisor has published supervisor.json, which it does once the command is running.
    fn wait_until_started(&self) {
        poll("the command to start", STARTS_WITHIN, || {
            self.dir
                .read_info()
                .expect("read supervisor.json")
                .map(|_| ())
        });
    }

    /// Waits until the log contains `text`. The command prints it only after the step that precedes it.
    fn wait_for_log(&self, text: &str) {
        poll(&format!("{text:?} in the log"), STARTS_WITHIN, || {
            self.log_text().contains(text).then_some(())
        });
    }

    /// Waits for the supervisor to exit within `within`, asserts that it exited 0, and returns the status it recorded.
    fn finish(&mut self, within: Duration) -> JobStatus {
        let child = self
            .supervisor
            .as_mut()
            .expect("the supervisor was started");
        let exit: ExitStatus = poll("the supervisor to exit", within, || {
            child.try_wait().expect("poll the supervisor")
        });
        assert!(
            exit.success(),
            "the supervisor exits 0 once it has recorded the status, got {exit}"
        );
        self.status()
    }

    fn request_cancel(&self) {
        self.dir.request_cancel().expect("write the cancel file");
    }

    fn status(&self) -> JobStatus {
        self.dir
            .read_status()
            .expect("read status.json")
            .expect("status.json is written")
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).expect("read the log")
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // The command's group first: killing the supervisor alone would leave the command running.
        if let Ok(Some(info)) = self.dir.read_info()
            && let Some(pid) = info.pid
        {
            let _ = signal_group(pid, Sig::Kill);
        }
        if let Some(child) = self.supervisor.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Starts the job and returns the status it recorded once the supervisor has exited.
fn run(job: &mut Job) -> JobStatus {
    job.start();
    job.finish(ENDS_WITHIN)
}

/// Polls `probe` every POLL until it yields a value; panics with `what` once `within` has passed.
fn poll<T>(what: &str, within: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?} waiting for {what}"
        );
        thread::sleep(POLL);
    }
}

#[test]
fn a_command_that_exits_zero_is_recorded_as_done_with_its_cpu_and_memory() {
    let mut job = Job::new(&["/bin/sh", "-c", "exit 0"]);
    let status = run(&mut job);
    assert_eq!(status.status, Some(0));
    assert!(status.started);
    assert!(status.cpu_seconds.is_some_and(|seconds| seconds >= 0.0));
    assert!(status.rss_peak_kib.is_some_and(|kib| kib > 0));
    assert!(!status.cancelled && !status.over_limit);
}

#[test]
fn a_nonzero_exit_status_of_the_command_is_recorded_as_it_is() {
    let mut job = Job::new(&["/bin/sh", "-c", "exit 3"]);
    let status = run(&mut job);
    assert_eq!(status.status, Some(3));
    assert!(status.started);
}

#[test]
fn a_missing_program_is_recorded_as_127_with_a_note_and_a_cannot_run_line() {
    let mut job = Job::new(&["agora-no-such-program"]);
    let status = run(&mut job);
    assert_eq!(status.status, Some(127));
    assert!(!status.started, "a command that never ran did not start");
    assert!(
        status.note.starts_with("could not start: ")
            && status.note.contains("No such file or directory"),
        "note was {:?}",
        status.note
    );
    assert!(
        job.log_text().lines().any(|line| {
            line.starts_with("work: cannot run agora-no-such-program: ")
                && line.contains("No such file or directory")
        }),
        "the log has no 'work: cannot run' line:\n{}",
        job.log_text()
    );
}

#[test]
fn a_file_that_is_not_executable_is_recorded_as_126() {
    let mut job = Job::with(&["placeholder"], |spec| {
        spec.argv = vec![format!("{}/not-run", spec.cwd)];
    });
    let program = job.cwd.join("not-run");
    fs::write(&program, "#!/bin/sh\nexit 0\n").expect("write the program");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o644)).expect("drop the x bits");
    let status = run(&mut job);
    assert_eq!(status.status, Some(126));
    assert!(!status.started);
    assert!(
        status.note.starts_with("could not start: "),
        "note was {:?}",
        status.note
    );
}

#[test]
fn a_command_that_kills_itself_with_sigterm_is_recorded_as_143() {
    let mut job = Job::new(&["/bin/sh", "-c", "kill -TERM $$"]);
    let status = run(&mut job);
    assert_eq!(status.status, Some(143));
    assert!(status.started);
    assert!(!status.cancelled && !status.over_limit);
}

#[test]
fn a_job_whose_log_passes_the_cap_is_stopped_and_recorded_as_over_limit() {
    let mut job = Job::with(
        &["/bin/sh", "-c", "yes | head -c 2000000; sleep 5"],
        |spec| spec.log_limit = SMALL_LOG_LIMIT,
    );
    let status = run(&mut job);
    assert!(status.over_limit);
    assert_eq!(status.status, Some(143));
    assert!(
        status
            .note
            .starts_with("stopped: its log passed the limit of 65536 bytes"),
        "note was {:?}",
        status.note
    );
    let size = fs::metadata(&job.log).expect("stat the log").len();
    assert!(size < 2 * 1024 * 1024, "the log grew to {size} bytes");
}

#[test]
fn a_cancel_file_stops_a_running_job_with_status_143_within_eight_seconds() {
    let mut job = Job::new(&["/bin/sh", "-c", "sleep 30"]);
    job.start();
    job.wait_until_started();
    job.request_cancel();
    let status = job.finish(Duration::from_secs(8));
    assert_eq!(status.status, Some(143));
    assert!(status.cancelled);
    assert!(status.started);
}

#[test]
fn a_job_that_ignores_term_is_killed_after_the_grace_period_and_recorded_as_137() {
    let mut job = Job::new(&["/bin/sh", "-c", "trap '' TERM; echo ready; sleep 30"]);
    job.start();
    // The trap is set before the shell prints this line, so the TERM that follows cannot end the shell.
    job.wait_for_log("ready");
    job.request_cancel();
    let status = job.finish(Duration::from_secs(9));
    assert_eq!(status.status, Some(137));
    assert!(status.cancelled);
}

#[test]
fn a_job_whose_directory_is_gone_is_recorded_as_unstarted_and_names_the_directory() {
    let mut job = Job::with(&["/bin/sh", "-c", "echo should not run"], |spec| {
        spec.cwd = format!("{}/gone", spec.cwd);
    });
    let status = run(&mut job);
    assert_eq!(status.status, None);
    assert!(!status.started);
    assert!(!status.cancelled);
    assert_eq!(
        status.note,
        format!("its directory {} is gone", job.cwd.display())
    );
}

#[test]
fn sigterm_to_the_supervisor_stops_its_job_and_records_it_as_cancelled() {
    let mut job = Job::new(&["/bin/sh", "-c", "sleep 30"]);
    job.start();
    job.wait_until_started();
    signal_pid(job.supervisor_pid(), Sig::Term).expect("send TERM to the supervisor");
    let status = job.finish(ENDS_WITHIN);
    assert_eq!(status.status, Some(143));
    assert!(status.cancelled);
    assert!(status.started);
}

#[test]
fn a_job_that_outlives_its_first_sample_leaves_a_sample_file() {
    let mut job = Job::new(&["/bin/sh", "-c", "sleep 3"]);
    run(&mut job);
    let sample = job
        .dir
        .read_sample()
        .expect("read sample.json")
        .expect("sample.json is written for a 3 s job");
    assert!(sample.rss_kib > 0, "the sample saw no memory: {sample:?}");
}

#[test]
fn supervisor_info_names_the_supervisor_and_the_command_process() {
    let mut job = Job::new(&["/bin/sh", "-c", "echo pid=$$"]);
    job.start();
    let supervisor = job.supervisor_pid();
    job.finish(ENDS_WITHIN);
    let info = job
        .dir
        .read_info()
        .expect("read supervisor.json")
        .expect("supervisor.json is written");
    let command = job
        .log_text()
        .lines()
        .find_map(|line| line.strip_prefix("pid=").map(str::to_owned))
        .and_then(|pid| pid.parse::<u32>().ok())
        .expect("the command printed its own pid");
    assert_eq!(info.supervisor_pid, supervisor);
    assert_eq!(info.pid, Some(command));
}

#[test]
fn an_exclusive_job_brackets_its_output_with_the_context_lines() {
    let mut job = Job::with(&["/bin/sh", "-c", "echo middle"], |spec| {
        spec.exclusive = true;
        spec.context_start = Some("work: context at start: 1 running job".to_owned());
    });
    run(&mut job);
    let lines: Vec<String> = job.log_text().lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 3, "log lines: {lines:?}");
    assert_eq!(lines[0], "work: context at start: 1 running job");
    assert_eq!(lines[1], "middle");
    assert!(
        lines[2].starts_with("work: context at end: "),
        "last line was {:?}",
        lines[2]
    );
}

#[test]
fn the_command_sees_only_its_spec_environment_and_a_pwd_of_its_directory() {
    let mut job = Job::with(
        &[
            "/bin/sh",
            "-c",
            r#"echo "$FOO|$PWD|${AGORA_TEST_LEAK-absent}""#,
        ],
        |spec| {
            spec.env.insert("FOO".to_owned(), "bar".to_owned());
        },
    );
    run(&mut job);
    assert_eq!(
        job.log_text().trim_end(),
        format!("bar|{}|absent", job.cwd.display())
    );
}

#[test]
fn the_supervisor_leads_a_session_of_its_own() {
    let mut job = Job::new(&["/bin/sh", "-c", "sleep 30"]);
    job.start();
    job.wait_until_started();
    let leader = Pid::from_child(job.supervisor());
    let session = getsid(Some(leader)).expect("the supervisor's session");
    let own = getsid(None).expect("the test's session");
    assert_eq!(
        session, leader,
        "the supervisor calls setsid, so it leads its session"
    );
    assert_ne!(session, own, "the supervisor is not in the test's session");
    job.request_cancel();
    job.finish(ENDS_WITHIN);
}
