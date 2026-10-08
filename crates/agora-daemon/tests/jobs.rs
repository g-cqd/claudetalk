//! Jobs through the daemon (D8, D9, D10, D14): an inline `work.run` ends with its command's status, `work.cancel`
//! stops a running job with status 143, the waiter times out while the job runs, the log cap stops a job, and the
//! command gets the caller's environment after scrubbing. Real child processes under the `agora-supervise` binary;
//! every wait is bounded, and admission uses `FakeMachine`, so host load never holds a job.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agora_proto::{Reply, Req};
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::model::{
    AddArgs, CancelArgs, RunArgs, SettingsArgs, TaskRecord, TaskState, WaitResult, WaitSpec,
};
use agora_sched::settings::SettingsPatch;
use common::*;

/// Bound for a job that runs to its own end (the longest of these is a 5 s sleep after a log overflow).
const JOB_WITHIN: Duration = Duration::from_secs(20);
/// Bound for a job to be reported running after `work.run`.
const RUNNING_WITHIN: Duration = Duration::from_secs(15);
/// Bound for a running job to end after `work.cancel`: TERM to the group, KILL after 5 s (D9).
const CANCEL_WITHIN: Duration = Duration::from_secs(7);
/// The longest one WorkWait asks for; the server clamps larger requests to the same bound.
const WAIT_MS: u64 = 55_000;
/// The timeout of the WorkWait that re-reads a task which is not running yet.
const PACE_MS: u64 = 20;

/// A daemon whose admission machine is the fake (idle, roomy, and the same whatever the host is doing).
async fn daemon() -> TestDaemon {
    let machine: Arc<dyn Machine> = FakeMachine::new();
    start_injected(None, Some(machine), |_| {}).await
}

/// The `work.run` argument for the inline command `argv` run in `cwd`.
fn run_arg(argv: &[&str], cwd: &Path) -> String {
    let add = AddArgs {
        title: "jobs test".to_string(),
        command: Some(argv.iter().map(|arg| arg.to_string()).collect()),
        cwd: Some(cwd.display().to_string()),
        ..Default::default()
    };
    serde_json::to_string(&RunArgs {
        task: None,
        add: Some(add),
    })
    .expect("a RunArgs serializes")
}

/// Requests the inline command `argv` in `cwd` with `env`, and returns the task as the daemon reports it.
async fn run_inline(
    client: &mut TestClient,
    cwd: &Path,
    argv: &[&str],
    env: BTreeMap<String, String>,
) -> TaskRecord {
    let op = client.op("work.run", "", &run_arg(argv, cwd));
    match client.ok(Req::WorkRun { op, env }).await {
        Reply::Task { task } => task,
        other => panic!("work.run: expected Task, got {other:?}"),
    }
}

/// The task `id` as the daemon reports it now.
async fn show(client: &mut TestClient, id: &str) -> TaskRecord {
    match client
        .ok(Req::WorkShow {
            task: id.to_string(),
        })
        .await
    {
        Reply::Task { task } => task,
        other => panic!("work.show {id}: expected Task, got {other:?}"),
    }
}

/// One WorkWait on the task `id`, returning after `timeout_ms` or when the task has ended.
async fn wait_once(
    client: &mut TestClient,
    id: &str,
    timeout_ms: u64,
    cursor: Option<String>,
) -> WaitResult {
    let spec = WaitSpec::Tasks {
        ids: vec![id.to_string()],
        any: false,
    };
    match client
        .ok(Req::WorkWait {
            spec,
            timeout_ms,
            cursor,
        })
        .await
    {
        Reply::Wait { result } => result,
        other => panic!("work.wait {id}: expected Wait, got {other:?}"),
    }
}

/// Waits until the task `id` ends, resuming with the cursor after each timeout; panics past `within`.
async fn wait_done(
    client: &mut TestClient,
    id: &str,
    cursor: Option<String>,
    within: Duration,
) -> WaitResult {
    tokio::time::timeout(within, async {
        let mut cursor = cursor;
        loop {
            let result = wait_once(client, id, WAIT_MS, cursor.take()).await;
            if !result.timed_out {
                return result;
            }
            cursor = result.cursor;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {id} did not end within {within:?}"))
}

/// Re-reads the task `id` until the daemon reports it running. The daemon does not push work changes to clients,
/// so the re-read is paced by a short WorkWait, which returns after its timeout while the task is not done.
async fn await_running(client: &mut TestClient, id: &str) -> TaskRecord {
    tokio::time::timeout(RUNNING_WITHIN, async {
        loop {
            let task = show(client, id).await;
            match task.state {
                TaskState::Running => return task,
                state if state.is_final() => {
                    panic!("task {id} ended ({}) before it ran", state.as_str())
                }
                _ => {}
            }
            wait_once(client, id, PACE_MS, None).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {id} did not start within {RUNNING_WITHIN:?}"))
}

/// `work.cancel` of the task `id` with `note`; returns the task as the daemon reports it at the reply.
async fn cancel(client: &mut TestClient, id: &str, note: &str) -> TaskRecord {
    let arg = serde_json::to_string(&CancelArgs {
        note: note.to_string(),
    })
    .expect("a CancelArgs serializes");
    let op = client.op("work.cancel", id, &arg);
    match client.ok(Req::WorkCancel { op }).await {
        Reply::Task { task } => task,
        other => panic!("work.cancel {id}: expected Task, got {other:?}"),
    }
}

/// Sets `log_limit` through a signed `work.settings` by the owner (Controller+).
async fn set_log_limit(owner: &mut TestClient, bytes: &str) {
    let patch = SettingsPatch {
        log_limit: Some(bytes.to_string()),
        ..Default::default()
    };
    let arg = serde_json::to_string(&SettingsArgs { patch }).expect("a SettingsArgs serializes");
    let op = owner.op("work.settings", "", &arg);
    match owner.ok(Req::WorkSettings { op: Some(op) }).await {
        Reply::WorkSettings { .. } => {}
        other => panic!("work.settings: expected WorkSettings, got {other:?}"),
    }
}

/// The job's log, read after the job has ended.
fn read_log(task: &TaskRecord) -> String {
    let path = task.log.as_deref().expect("the job records its log path");
    fs::read_to_string(path).expect("the job's log is readable")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_that_exits_zero_ends_done_with_status_zero_and_a_log() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(&mut w.owner, d.home.path(), &["true"], BTreeMap::new()).await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert!(!ended.timed_out);
    assert_eq!(ended.status, 0);
    let done = show(&mut w.owner, &task.id).await;
    assert_eq!(done.state, TaskState::Done);
    assert_eq!(done.status, Some(0));
    let log = done.log.as_deref().expect("the job records its log path");
    assert!(Path::new(log).is_file(), "the job's log {log} exists");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_that_exits_nonzero_ends_failed_with_its_status_and_output_in_the_log() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo hi; exit 3"],
        BTreeMap::new(),
    )
    .await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 3);
    let failed = show(&mut w.owner, &task.id).await;
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.status, Some(3));
    assert!(
        read_log(&failed).contains("hi"),
        "the log keeps the command's output"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_program_ends_failed_with_status_127() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["agora-m5-no-such-program"],
        BTreeMap::new(),
    )
    .await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 127);
    let failed = show(&mut w.owner, &task.id).await;
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.status, Some(127));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_killed_by_sigterm_ends_failed_with_status_143() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "kill -TERM $$"],
        BTreeMap::new(),
    )
    .await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 143);
    let failed = show(&mut w.owner, &task.id).await;
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.status, Some(143));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_killed_by_sigkill_ends_failed_with_status_137() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "kill -KILL $$"],
        BTreeMap::new(),
    )
    .await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 137);
    let failed = show(&mut w.owner, &task.id).await;
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.status, Some(137));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_running_job_ends_it_cancelled_with_status_143_within_seven_seconds() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sleep", "30"],
        BTreeMap::new(),
    )
    .await;
    await_running(&mut w.owner, &task.id).await;

    cancel(&mut w.owner, &task.id, "enough").await;
    let ended = wait_done(&mut w.owner, &task.id, None, CANCEL_WITHIN).await;

    assert_eq!(ended.status, 143);
    let cancelled = show(&mut w.owner, &task.id).await;
    assert_eq!(cancelled.state, TaskState::Cancelled);
    assert_eq!(cancelled.status, Some(143));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_running_job_keeps_a_note_saying_who_cancelled_it_and_why() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sleep", "30"],
        BTreeMap::new(),
    )
    .await;
    await_running(&mut w.owner, &task.id).await;

    cancel(&mut w.owner, &task.id, "enough").await;
    wait_done(&mut w.owner, &task.id, None, CANCEL_WITHIN).await;

    let cancelled = show(&mut w.owner, &task.id).await;
    assert!(
        cancelled.note.starts_with("cancelled by ") && cancelled.note.ends_with(": enough"),
        "the note says who cancelled it and why, got {:?}",
        cancelled.note
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_that_times_out_reports_124_while_the_job_runs_and_a_later_wait_gets_its_status() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sleep", "20"],
        BTreeMap::new(),
    )
    .await;
    await_running(&mut w.owner, &task.id).await;

    let early = wait_once(&mut w.owner, &task.id, 500, None).await;
    assert!(early.timed_out, "a 500 ms wait on a 20 s job times out");
    assert_eq!(early.status, 124);
    assert_eq!(early.pending, vec![task.id.clone()]);
    assert!(
        early.cursor.is_some(),
        "a timed-out wait returns a cursor to resume with"
    );
    let still = show(&mut w.owner, &task.id).await;
    assert_eq!(still.state, TaskState::Running);

    cancel(&mut w.owner, &task.id, "").await;
    let later = wait_done(&mut w.owner, &task.id, early.cursor, CANCEL_WITHIN).await;
    assert!(!later.timed_out);
    assert_eq!(later.status, 143, "the cancel ends the job with 143");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_that_passes_the_limit_stops_the_job_with_status_143_and_a_note() {
    let d = daemon().await;
    let mut w = world(&d).await;
    set_log_limit(&mut w.owner, "65536").await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "yes | head -c 2000000; sleep 5"],
        BTreeMap::new(),
    )
    .await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 143);
    let stopped = show(&mut w.owner, &task.id).await;
    assert_eq!(stopped.state, TaskState::Failed);
    assert_eq!(stopped.status, Some(143));
    assert!(
        stopped
            .note
            .starts_with("stopped: its log passed the limit of 65536 bytes"),
        "the note names the log limit, got {:?}",
        stopped.note
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_gets_the_caller_environment_without_secret_names() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let env = BTreeMap::from([
        ("MY_TOKEN".to_string(), "s3cret".to_string()),
        ("KEEP".to_string(), "ok".to_string()),
    ]);
    let task = run_inline(&mut w.owner, d.home.path(), &["sh", "-c", "env"], env).await;

    let ended = wait_done(&mut w.owner, &task.id, None, JOB_WITHIN).await;
    assert_eq!(ended.status, 0);
    let log = read_log(&show(&mut w.owner, &task.id).await);
    assert!(
        log.lines().any(|line| line == "KEEP=ok"),
        "the command sees KEEP, log:\n{log}"
    );
    assert!(
        !log.contains("MY_TOKEN"),
        "the scrubbed name is absent, log:\n{log}"
    );
    assert!(
        !log.contains("s3cret"),
        "the scrubbed value is absent, log:\n{log}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_status_lists_a_running_job_with_its_pid() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sleep", "30"],
        BTreeMap::new(),
    )
    .await;
    await_running(&mut w.owner, &task.id).await;

    let report = match w.owner.ok(Req::WorkStatus { scope: None }).await {
        Reply::WorkStatus { report } => report,
        other => panic!("work.status: expected WorkStatus, got {other:?}"),
    };
    let listed = report
        .running
        .iter()
        .find(|running| running.id == task.id)
        .expect("the running job is listed as running");
    assert_eq!(listed.state, TaskState::Running);
    assert!(
        listed.pid.is_some_and(|pid| pid > 0),
        "the listed job has its command's pid"
    );

    cancel(&mut w.owner, &task.id, "").await;
    wait_done(&mut w.owner, &task.id, None, CANCEL_WITHIN).await;
    d.stop().await;
}
