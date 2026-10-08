//! Restart recovery of the job layer (D8, O-22), through the daemon: a job's supervisor outlives the daemon, and the
//! daemon that starts again adopts it, sweeps it, or finalizes it from its status.json. Real child processes: a
//! `sleep` (or `sh`) command under the `agora-supervise` supervisor. Every wait is bounded by `WAIT`. A Drop guard
//! kills any supervisor or command the test started that is still alive, so a failing test leaves nothing behind.

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

/// Starts a `sleep 8` job, stops the daemon, and starts a new one on the same home: the supervisor survived the
/// restart, the job is still running, and the adopted supervisor's status.json finalizes it as done with status 0.
/// The job is 8 s, not 4 s: under host load the test can observe the start late, and a 4 s job may already have ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_job_outlives_a_daemon_restart_and_is_finalized_from_its_status() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "adopt", &["/bin/sleep", "8"]).await;
    await_task(&mut alice, "adopt", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "adopt");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let during = show(&mut alice, "adopt").await;
    assert_eq!(
        during.state,
        TaskState::Running,
        "the job is still running after the restart"
    );
    assert!(
        alive(info.supervisor_pid, info.supervisor_start_s),
        "the supervisor {} survived the daemon's shutdown",
        info.supervisor_pid
    );

    let done = await_task(&mut alice, "adopt", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the adopted job ends as done");
    assert_eq!(done.status, Some(0), "the adopted job records its status 0");
}

/// The daemon stops while a job runs; the supervisor and its command are killed while the daemon is down. The
/// restarted daemon's startup sweep fails the task with no status, and names the dead supervisor in the note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_supervisor_and_command_died_while_the_daemon_was_down_fails_without_status() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "both", &["/bin/sleep", "30"]).await;
    await_task(&mut alice, "both", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "both");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);
    let command = info
        .pid
        .expect("the command's pid is recorded once it runs");

    signal_pid(info.supervisor_pid, Sig::Kill).expect("kill the supervisor");
    signal_group(command, Sig::Kill).expect("kill the command's process group");
    until_gone(
        info.supervisor_pid,
        info.supervisor_start_s,
        "the supervisor",
    )
    .await;
    until_gone(command, info.pid_start_s, "the command").await;

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let failed = await_task(&mut alice, "both", "end", |t| t.state.is_final()).await;
    assert_eq!(failed.state, TaskState::Failed, "the sweep fails the job");
    assert_eq!(failed.status, None, "no status was ever recorded");
    assert_eq!(
        failed.note,
        ended_without_status_note(info.supervisor_pid),
        "the note names the dead supervisor"
    );
}

/// The daemon stops, and only the supervisor is killed; the command lives on. The restarted daemon keeps the task
/// running with the orphan note. Killing the command and then a WorkStatus (which runs the sweep) fails the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_command_outlives_its_supervisor_stays_running_until_a_later_sweep() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "orphan", &["/bin/sleep", "30"]).await;
    await_task(&mut alice, "orphan", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "orphan");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);
    let command = info
        .pid
        .expect("the command's pid is recorded once it runs");

    signal_pid(info.supervisor_pid, Sig::Kill).expect("kill the supervisor only");
    until_gone(
        info.supervisor_pid,
        info.supervisor_start_s,
        "the supervisor",
    )
    .await;
    assert!(
        alive(command, info.pid_start_s),
        "the command {command} outlives its supervisor"
    );

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let orphaned = await_task(&mut alice, "orphan", "record the orphan note", |t| {
        t.note == orphan_note(info.supervisor_pid, command)
    })
    .await;
    assert_eq!(
        orphaned.state,
        TaskState::Running,
        "a job whose command lives on stays running"
    );
    assert_eq!(orphaned.status, None, "no status is recorded yet");

    signal_group(command, Sig::Kill).expect("kill the orphaned command");
    until_gone(command, info.pid_start_s, "the orphaned command").await;
    let still = show(&mut alice, "orphan").await;
    assert_eq!(
        still.state,
        TaskState::Running,
        "nothing has swept the job yet"
    );

    alice.ok(Req::WorkStatus { scope: None }).await;
    let failed = show(&mut alice, "orphan").await;
    assert_eq!(
        failed.state,
        TaskState::Failed,
        "a WorkStatus sweep fails the job"
    );
    assert_eq!(failed.status, None, "no status was ever recorded");
    assert_eq!(
        failed.note,
        ended_without_status_note(info.supervisor_pid),
        "the sweep names the dead supervisor"
    );
}

/// A job whose command ends while the daemon is down: its supervisor writes status.json during the downtime. The
/// restarted daemon finalizes it from that file as done with status 0.
///
/// The command waits for a gate file, not for `true`: the daemon must be stopped while the job is still running, and
/// a command that ends at once would finish before it could be stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_finished_while_the_daemon_was_down_is_recorded_done_on_restart() {
    let home = new_home();
    let root = home.path().to_path_buf();
    let gate = root.join("gate");
    let script = format!(
        "until [ -e '{}' ]; do /bin/sleep 0.05; done",
        gate.display()
    );
    let d = start_on(home, |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "finish",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    await_task(&mut alice, "finish", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "finish");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let status_path = job_dir(home.path(), "finish");
    poll("the supervisor writes status.json", || {
        status_path.read_status().ok().flatten()
    })
    .await;

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "finish", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the finished job is done");
    assert_eq!(
        done.status,
        Some(0),
        "the status from status.json is recorded"
    );
}

/// O-22: the daemon stops after a job is claimed and requested but before its supervisor reports (job.start not yet
/// recorded). The supervisor then comes up during the downtime. The restarted daemon adopts that supervisor and
/// records its start, and the command runs once, not a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_job_whose_supervisor_came_up_during_downtime_is_adopted_not_run_again() {
    // The gate lives in its own directory, declared first, so it outlives the guard that opens it on any exit path.
    let gates = tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("a gate dir under /tmp");
    let gate = gates.path().join("gate");
    let _open_gate = GateOnDrop(gate.clone());
    let home = new_home();
    let root = home.path().to_path_buf();
    let mark = root.join("ran.txt");
    let supervisor = env!("CARGO_BIN_EXE_agora-supervise").to_owned();
    let script = format!("echo ran >> '{}'; /bin/sleep 3", mark.display());
    let gated = {
        let gate = gate.display().to_string();
        move |cfg: &mut DaemonConfig| {
            cfg.supervisor_argv = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                GATE_WRAPPER.into(),
                "sh".into(),
                gate.clone(),
                supervisor.clone(),
            ]);
        }
    };
    let held = gates.path().join("gate.held");
    let d = start_on(home, gated).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "gated",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    poll("the supervisor is spawned and held at the gate", || {
        held.exists().then_some(())
    })
    .await;
    let before = show(&mut alice, "gated").await;
    assert_eq!(
        before.state,
        TaskState::Claimed,
        "the job is requested but its supervisor has not reported, so it is not started"
    );

    let home = stop(d).await;
    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let dir = job_dir(home.path(), "gated");
    let info = poll("the supervisor reports its command", || {
        dir.read_info().ok().flatten()
    })
    .await;
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "gated", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the adopted job ends as done");
    assert_eq!(done.status, Some(0), "the adopted job records its status 0");
    assert_eq!(
        done.supervisor_pid,
        Some(i64::from(info.supervisor_pid)),
        "the record names the supervisor that came up during the downtime"
    );
    let runs = std::fs::read_to_string(&mark).expect("the command wrote its mark");
    assert_eq!(runs, "ran\n", "the command ran exactly once");
}

/// C1-M1: the daemon stops after a job is claimed and requested but before job.start is recorded. Its supervisor starts a
/// short command during the downtime, and the command finishes before the daemon returns, so status.json says started
/// while the task is still claimed. The restarted daemon records the start and finishes the job as done with status 0.
/// The command runs exactly once: nothing re-admits the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claimed_job_whose_command_finished_during_downtime_is_finished_once_not_run_again() {
    // The gate lives in its own directory, declared first, so it outlives the guard that opens it on any exit path.
    let gates = tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("a gate dir under /tmp");
    let gate = gates.path().join("gate");
    let _open_gate = GateOnDrop(gate.clone());
    let home = new_home();
    let root = home.path().to_path_buf();
    let mark = root.join("ran.txt");
    let supervisor = env!("CARGO_BIN_EXE_agora-supervise").to_owned();
    let script = format!("echo once >> '{}'", mark.display());
    let gated = {
        let gate = gate.display().to_string();
        move |cfg: &mut DaemonConfig| {
            cfg.supervisor_argv = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                GATE_WRAPPER.into(),
                "sh".into(),
                gate.clone(),
                supervisor.clone(),
            ]);
        }
    };
    let held = gates.path().join("gate.held");
    let d = start_on(home, gated).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "once",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    poll("the supervisor is spawned and held at the gate", || {
        held.exists().then_some(())
    })
    .await;
    let before = show(&mut alice, "once").await;
    assert_eq!(
        before.state,
        TaskState::Claimed,
        "the job is requested but its supervisor has not reported, so it is not started"
    );

    let home = stop(d).await;
    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let dir = job_dir(home.path(), "once");
    let status = poll("the command finishes and status.json is written", || {
        dir.read_status().ok().flatten()
    })
    .await;
    assert!(
        status.started,
        "the supervisor reports that the command started"
    );
    let info = job_info(home.path(), "once");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "once", "end", |t| t.state.is_final()).await;
    assert_eq!(
        done.state,
        TaskState::Done,
        "the job that finished during the downtime is done"
    );
    assert_eq!(
        done.status,
        Some(0),
        "the status from status.json is recorded"
    );
    assert_eq!(
        done.supervisor_pid,
        Some(i64::from(info.supervisor_pid)),
        "the recorded start names the supervisor that reported it"
    );

    // A re-admitted job would start within moments of the restart; a few seconds of quiet show that none does.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = show(&mut alice, "once").await;
    assert_eq!(
        later.state,
        TaskState::Done,
        "nothing started the job again"
    );
    let runs = std::fs::read_to_string(&mark).expect("the command wrote its mark");
    assert_eq!(runs, "once\n", "the command ran exactly once");
}

/// Opens a gate file when dropped, so a supervisor held at the gate never outlives its test.
struct GateOnDrop(std::path::PathBuf);

impl Drop for GateOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"");
    }
}
