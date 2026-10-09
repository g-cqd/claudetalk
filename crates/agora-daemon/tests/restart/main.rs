//! Restart recovery of the job layer (D8, O-22), through the daemon: a job's supervisor outlives the daemon, and the
//! daemon that starts again adopts it, sweeps it, or finalizes it from its status.json. Real child processes: a
//! `sleep` (or `sh`) command under the `agora-supervise` supervisor. Every wait is bounded by `WAIT`. A Drop guard
//! kills any supervisor or command the test started that is still alive, so a failing test leaves nothing behind.

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agora_daemon::{Daemon, DaemonConfig, Injected};
use agora_proto::{Reply, Req};
use agora_run::jobdir::{JobDir, SupervisorInfo, work_root};
use agora_run::machine::{FakeMachine, Machine};
use agora_run::proc::{Sig, alive, signal_group, signal_pid};
use agora_sched::model::{TaskRecord, TaskState, WaitSpec};
use common::{TestClient, TestDaemon, connect, ident, world};
use tempfile::TempDir;

mod adopt;
mod finalize;
mod sweep;

/// Bound for every wait in this file: a daemon or supervisor bug fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(20);

/// Interval of the file and process probes. Only used where no event is available to await.
const POLL: Duration = Duration::from_millis(25);

/// The gate of the adoption test: the supervisor is held here until the test opens the gate, and then it runs the
/// command. `$1` is the gate, `$2` the supervisor binary; the daemon's `--job-dir D` is what remains. The wrapper
/// writes `<gate>.held` first, so the test knows the supervisor was spawned and is held before it stops the daemon.
const GATE_WRAPPER: &str = r#"gate="$1"; sup="$2"; shift 2; echo held > "$gate.held"; while [ ! -e "$gate" ]; do /bin/sleep 0.05; done; exec "$sup" "$@""#;

/// A fresh home under /tmp (short path: socket paths are limited to 104 bytes on macOS).
fn new_home() -> TempDir {
    tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("a temp home under /tmp")
}

/// Binds a daemon on `home` with a fake machine (admission never depends on host load) and the test supervisor.
/// `f` may override the config, as in the common harness.
async fn start_on(home: TempDir, f: impl FnOnce(&mut DaemonConfig)) -> TestDaemon {
    let mut cfg = DaemonConfig::load(home.path()).expect("load the test home's config");
    cfg.handle_signals = false;
    cfg.supervisor_argv = Some(vec![env!("CARGO_BIN_EXE_agora-supervise").to_owned()]);
    f(&mut cfg);
    let machine: Arc<dyn Machine> = FakeMachine::new();
    let injected = Injected {
        clock: None,
        machine: Some(machine),
    };
    let daemon = Daemon::bind_with(cfg, injected)
        .await
        .expect("bind the daemon on the test home");
    let socket = daemon.socket_path().to_path_buf();
    let token = daemon.shutdown_token();
    let task = tokio::spawn(daemon.run());
    TestDaemon {
        home,
        socket,
        token,
        task,
    }
}

/// Stops the daemon and returns its home, which stays alive for the next start. The common `TestDaemon::stop`
/// consumes the home, so it cannot be used here.
async fn stop(d: TestDaemon) -> TempDir {
    let TestDaemon {
        home, token, task, ..
    } = d;
    token.cancel();
    match tokio::time::timeout(Duration::from_secs(15), task).await {
        Err(_) => panic!("daemon did not stop within 15 s of cancellation"),
        Ok(Err(join)) => panic!("daemon run task panicked: {join}"),
        Ok(Ok(Err(e))) => panic!("daemon run returned an error: {e}"),
        Ok(Ok(Ok(()))) => {}
    }
    home
}

/// Runs `argv` as an inline job named `id` in `home` (`work.run` with a command), with only PATH in its environment.
/// The cwd is the canonical home: the supervisor refuses a cwd that is not its own canonical path, and on macOS
/// /tmp is a symlink to /private/tmp. The restart scenarios do not test that check.
async fn run_inline(client: &mut TestClient, home: &Path, id: &str, argv: &[&str]) {
    let cwd = std::fs::canonicalize(home).expect("canonicalize the test home");
    let arg = serde_json::json!({
        "add": {"id": id, "title": id, "command": argv, "cwd": cwd.display().to_string()}
    })
    .to_string();
    let op = client.op("work.run", "", &arg);
    let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    match client.ok(Req::WorkRun { op, env }).await {
        Reply::Task { task } => assert_eq!(task.id, id, "the run names the task it created"),
        other => panic!("work run: expected Task, got {other:?}"),
    }
}

/// `WorkShow` of one task.
async fn show(client: &mut TestClient, task: &str) -> TaskRecord {
    match client
        .ok(Req::WorkShow {
            task: task.to_owned(),
        })
        .await
    {
        Reply::Task { task } => task,
        other => panic!("work show: expected Task, got {other:?}"),
    }
}

/// Waits until `task` satisfies `ready`, through WorkWait: each call returns as soon as the queue changes the task, or
/// after a 500 ms slice, and then re-reads it. Panics after `WAIT`.
async fn await_task(
    client: &mut TestClient,
    task: &str,
    what: &str,
    ready: impl Fn(&TaskRecord) -> bool,
) -> TaskRecord {
    let deadline = Instant::now() + WAIT;
    let mut last = String::from("no record");
    loop {
        let spec = WaitSpec::Tasks {
            ids: vec![task.to_owned()],
            any: false,
        };
        let result = match client
            .ok(Req::WorkWait {
                spec,
                timeout_ms: 500,
                cursor: None,
            })
            .await
        {
            Reply::Wait { result } => result,
            other => panic!("work wait: expected Wait, got {other:?}"),
        };
        if let Some(record) = result.tasks.into_iter().find(|t| t.id == task) {
            if ready(&record) {
                return record;
            }
            last = format!("state {}, note {:?}", record.state.as_str(), record.note);
        }
        assert!(
            Instant::now() < deadline,
            "task {task} did not {what} within {WAIT:?} (last seen: {last})"
        );
    }
}

/// Polls `check` at `POLL` intervals until it yields a value, panicking after `WAIT`. Used for files and processes,
/// which have no event to await.
async fn poll<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        tokio::time::sleep(POLL).await;
    }
}

/// The job directory of `task` under `home`.
fn job_dir(home: &Path, task: &str) -> JobDir {
    JobDir::new(&work_root(home), task).expect("a job dir for the task")
}

/// The supervisor.json of a running job; it exists once the job's command has started.
fn job_info(home: &Path, task: &str) -> SupervisorInfo {
    job_dir(home, task)
        .read_info()
        .expect("read supervisor.json")
        .expect("supervisor.json exists once the job runs")
}

/// Processes a test started, killed on drop if they are still the same processes (pid and start time match).
#[derive(Default)]
struct Cleanup {
    supervisors: Vec<(u32, Option<u64>)>,
    commands: Vec<(u32, Option<u64>)>,
}

impl Cleanup {
    fn track(&mut self, info: &SupervisorInfo) {
        self.supervisors
            .push((info.supervisor_pid, info.supervisor_start_s));
        if let Some(pid) = info.pid {
            self.commands.push((pid, info.pid_start_s));
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for (pid, start) in &self.supervisors {
            if alive(*pid, *start) {
                let _ = signal_pid(*pid, Sig::Kill);
            }
        }
        for (pgid, start) in &self.commands {
            if alive(*pgid, *start) {
                let _ = signal_group(*pgid, Sig::Kill);
            }
        }
    }
}

/// Waits until the process is gone (killed, and reaped or a zombie).
async fn until_gone(pid: u32, start: Option<u64>, what: &str) {
    poll(&format!("{what} is still alive"), || {
        (!alive(pid, start)).then_some(())
    })
    .await;
}

/// The note of a running job whose supervisor is gone while its command runs on (D8).
fn orphan_note(supervisor: u32, command: u32) -> String {
    format!(
        "its supervisor, process {supervisor}, is gone; the command runs on as process {command}, \
         and its status will not be recorded"
    )
}

/// The note of a job whose supervisor and command are both gone without a status (D8).
fn ended_without_status_note(supervisor: u32) -> String {
    format!(
        "the job ended without a recorded status: its supervisor, process {supervisor}, is gone"
    )
}
