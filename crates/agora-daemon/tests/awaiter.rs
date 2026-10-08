//! `Req::WorkWait` through the daemon (D10): each condition's status and wake-up, the timeout and cursor rules, the
//! server cap, the target bound, and the Worker gate of the filesystem and process waits (R4-1). Real files and real
//! child processes; every wait is bounded, and admission uses `FakeMachine` so host load never holds a job.

mod common;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agora_proto::{ErrorBody, ErrorCode, Reply, Req};
use agora_run::jobdir;
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::model::{
    AddArgs, CancelArgs, RunArgs, TaskRecord, TaskState, WaitResult, WaitSpec,
};
use common::*;
use tokio::task::JoinHandle;

/// Bound on every await in this file, so a daemon bug fails here instead of hanging.
const BOUND: Duration = Duration::from_secs(20);
/// The timeout of a wait that must end on its condition; the server caps it at 55 s.
const WAIT_MS: u64 = 30_000;
/// How long a producer waits before it acts: the external change the wait is for, which lands while the wait is
/// parked in practice. The assertions hold whichever comes first.
const LATER: Duration = Duration::from_millis(200);
/// Bound for a running job to end after `work.cancel`: TERM to the group, KILL after 5 s (D9).
const CANCEL_WITHIN_MS: u64 = 7_000;

/// A daemon whose admission machine is the fake (idle and roomy, whatever the host is doing).
async fn daemon() -> TestDaemon {
    let machine: Arc<dyn Machine> = FakeMachine::new();
    start_injected(None, Some(machine), |_| {}).await
}

/// `client.call` bounded by `BOUND`.
async fn call(client: &mut TestClient, req: Req) -> Result<Reply, ErrorBody> {
    tokio::time::timeout(BOUND, client.call(req))
        .await
        .unwrap_or_else(|_| panic!("the request did not return within {BOUND:?}"))
}

/// A WorkWait that may be refused.
async fn wait_result(
    client: &mut TestClient,
    spec: WaitSpec,
    timeout_ms: u64,
    cursor: Option<String>,
) -> Result<WaitResult, ErrorBody> {
    match call(
        client,
        Req::WorkWait {
            spec,
            timeout_ms,
            cursor,
        },
    )
    .await
    {
        Ok(Reply::Wait { result }) => Ok(result),
        Ok(other) => panic!("WorkWait: expected Wait, got {other:?}"),
        Err(error) => Err(error),
    }
}

/// A WorkWait that must be accepted.
async fn wait_on(
    client: &mut TestClient,
    spec: WaitSpec,
    timeout_ms: u64,
    cursor: Option<String>,
) -> WaitResult {
    match wait_result(client, spec, timeout_ms, cursor).await {
        Ok(result) => result,
        Err(error) => panic!("WorkWait failed: {error:?}"),
    }
}

/// A WorkWait on a client moved into a task, which hands the client back with the result.
fn spawn_wait(
    mut client: TestClient,
    spec: WaitSpec,
    timeout_ms: u64,
    cursor: Option<String>,
) -> JoinHandle<(TestClient, Result<WaitResult, ErrorBody>)> {
    tokio::spawn(async move {
        let result = wait_result(&mut client, spec, timeout_ms, cursor).await;
        (client, result)
    })
}

/// Awaits a spawned task within `BOUND`.
async fn join<T: Send + 'static>(task: JoinHandle<T>) -> T {
    tokio::time::timeout(BOUND, task)
        .await
        .unwrap_or_else(|_| panic!("the task did not finish within {BOUND:?}"))
        .expect("the task did not panic")
}

fn tasks_spec(ids: Vec<String>, any: bool) -> WaitSpec {
    WaitSpec::Tasks { ids, any }
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

/// Runs `act` after `LATER` on its own task.
fn after_later(act: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        act();
    })
}

fn create_later(path: PathBuf, bytes: &'static [u8]) -> JoinHandle<()> {
    after_later(move || fs::write(&path, bytes).expect("create the file"))
}

fn append_later(path: PathBuf, bytes: &'static [u8]) -> JoinHandle<()> {
    after_later(move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(bytes))
            .expect("append to the file")
    })
}

fn remove_later(path: PathBuf) -> JoinHandle<()> {
    after_later(move || fs::remove_file(&path).expect("remove the file"))
}

/// A producer acts `LATER` after it is spawned, so a wait that ended on its change ended at least that long after
/// `started`, taken before the producer was spawned. A wait that returned early fails this (tokio never sleeps short).
fn assert_waited_for_the_change(started: Instant) {
    assert!(
        started.elapsed() >= LATER,
        "the wait returned before the change it waits for"
    );
}

/// `count` distinct paths under `dir`, none of which exists.
fn absent_paths(dir: &Path, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| path_str(&dir.join(format!("absent-{i}"))))
        .collect()
}

/// Runs the inline command `argv` in `home` through `work.run`, and returns the task as the daemon reports it. The job
/// is given `home` in its canonical form: on macOS `/tmp` is a symlink, and the supervisor refuses a cwd that is not
/// its own canonical path, so a symlinked cwd would fail every job before it runs.
async fn run_inline(client: &mut TestClient, home: &Path, argv: &[&str]) -> TaskRecord {
    let cwd = home.canonicalize().expect("the home directory resolves");
    let add = AddArgs {
        title: "awaiter test".to_string(),
        command: Some(argv.iter().map(|arg| arg.to_string()).collect()),
        cwd: Some(path_str(&cwd)),
        ..Default::default()
    };
    let arg = serde_json::to_string(&RunArgs {
        task: None,
        add: Some(add),
    })
    .expect("a RunArgs serializes");
    let op = client.op("work.run", "", &arg);
    match call(
        client,
        Req::WorkRun {
            op,
            env: BTreeMap::new(),
        },
    )
    .await
    {
        Ok(Reply::Task { task }) => task,
        other => panic!("work.run: expected Task, got {other:?}"),
    }
}

/// `work.cancel` of the running task `id`, then a wait until it has ended.
async fn cancel_and_end(client: &mut TestClient, id: &str) {
    let arg = serde_json::to_string(&CancelArgs {
        note: "test over".to_string(),
    })
    .expect("a CancelArgs serializes");
    let op = client.op("work.cancel", id, &arg);
    call(client, Req::WorkCancel { op })
        .await
        .expect("work.cancel is accepted");
    let ended = wait_on(
        client,
        tasks_spec(vec![id.to_string()], false),
        CANCEL_WITHIN_MS,
        None,
    )
    .await;
    assert!(
        !ended.timed_out,
        "the cancelled job {id} ended within {CANCEL_WITHIN_MS} ms"
    );
}

/// Grants `role` to the identity with public key `who` (hex), as the owner.
async fn grant(owner: &mut TestClient, who: String, role: &str) {
    let op = owner.op("role.grant", &who, role);
    match call(owner, Req::RoleGrant { op }).await {
        Ok(Reply::Unit) => {}
        other => panic!("role.grant: expected Unit, got {other:?}"),
    }
}

async fn create_card(client: &mut TestClient, card: &str) {
    let arg = serde_json::json!({"title": "T", "lane": "l"}).to_string();
    let op = client.op("card.create", card, &arg);
    call(client, Req::CardCreate { op })
        .await
        .expect("card.create is accepted");
}

/// `card.escalate` of `card` with `action` to the keys `to`; returns the identities notified.
async fn escalate(
    client: &mut TestClient,
    card: &str,
    action: &str,
    to: Vec<String>,
) -> Vec<String> {
    let arg = serde_json::json!({"owner_action": action, "to": to}).to_string();
    let op = client.op("card.escalate", card, &arg);
    match call(client, Req::CardEscalate { op }).await {
        Ok(Reply::Escalated { notified }) => notified,
        other => panic!("card.escalate: expected Escalated, got {other:?}"),
    }
}

/// Carol (supervisor) creates card `id` and promotes it to ready; alice claims it, starts it and attaches evidence,
/// so that moving it to review is alice's next step.
async fn card_in_doing(w: &mut World, id: &str) {
    grant(&mut w.owner, w.carol.pk_hex(), "supervisor").await;
    create_card(&mut w.carol, id).await;
    let op = w.carol.op("card.move", id, "ready");
    call(&mut w.carol, Req::CardMove { op })
        .await
        .expect("card to ready");
    for to in ["claimed", "doing"] {
        let op = w.alice.op("card.move", id, to);
        call(&mut w.alice, Req::CardMove { op })
            .await
            .unwrap_or_else(|e| panic!("card to {to}: {e:?}"));
    }
    let op = w.alice.op("card.evidence", id, "log://x");
    call(&mut w.alice, Req::CardEvidence { op })
        .await
        .expect("evidence attached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_ends_with_status_zero_and_the_final_task_once_its_job_is_done() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(&mut w.owner, d.home.path(), &["sleep", "0.5"]).await;

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id.clone()], false),
        WAIT_MS,
        None,
    )
    .await;

    assert_eq!(result.condition, "tasks");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0, "tasks: {:?}", result.tasks);
    assert_eq!(result.targets, vec![task.id.clone()]);
    assert!(result.pending.is_empty());
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].id, task.id);
    assert_eq!(result.tasks[0].state, TaskState::Done);
    assert_eq!(result.tasks[0].status, Some(0));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_any_tasks_wait_returns_when_the_first_of_two_jobs_ends() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let fast = run_inline(&mut w.owner, d.home.path(), &["sleep", "0.3"]).await;
    let slow = run_inline(&mut w.owner, d.home.path(), &["sleep", "30"]).await;

    let ids = vec![fast.id.clone(), slow.id.clone()];
    let result = wait_on(&mut w.owner, tasks_spec(ids, true), WAIT_MS, None).await;

    assert!(!result.timed_out, "the first job ended, so the wait ends");
    assert_eq!(result.status, 0);
    assert_eq!(result.tasks.len(), 2);
    assert_eq!(result.tasks[0].state, TaskState::Done);
    assert!(
        !result.tasks[1].state.is_final(),
        "the second job still runs when the wait returns"
    );
    cancel_and_end(&mut w.owner, &slow.id).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_tail_holds_the_last_lines_of_the_first_final_tasks_log() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo line-one; echo line-two"],
    )
    .await;

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id], false),
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(
        result.tail,
        vec!["line-one".to_string(), "line-two".to_string()]
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_tasks_wait_tail_holds_the_log_of_the_first_task_that_has_one() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo started; sleep 30"],
    )
    .await;
    // The log matches once the job has written its first line, so the timed-out wait below cannot race the echo.
    let log = jobdir::log_path(&jobdir::work_root(d.home.path()), &task.id);
    let started = wait_on(
        &mut w.owner,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^started$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert!(!started.timed_out, "the job wrote its first line");

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id.clone()], false),
        300,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.pending, vec![task.id.clone()]);
    assert_eq!(result.tail, vec!["started".to_string()]);
    cancel_and_end(&mut w.owner, &task.id).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_that_neither_authored_nor_claimed_a_task_gets_its_status_but_no_log_tail() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo owner-only"],
    )
    .await;

    let result = wait_on(&mut w.bob, tasks_spec(vec![task.id], false), WAIT_MS, None).await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert!(
        result.tail.is_empty(),
        "the tail is the author's and claimant's to read"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_all_wait_waits_for_the_jobs_that_were_running_when_it_was_called() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let short = run_inline(&mut w.owner, d.home.path(), &["sleep", "1"]).await;
    let long = run_inline(&mut w.owner, d.home.path(), &["sleep", "2"]).await;

    let result = wait_on(&mut w.owner, WaitSpec::All { scope: None }, WAIT_MS, None).await;

    assert_eq!(result.condition, "all");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    let mut targets = result.targets.clone();
    targets.sort();
    let mut expected = vec![short.id.clone(), long.id.clone()];
    expected.sort();
    assert_eq!(targets, expected);
    assert_eq!(result.tasks.len(), 2);
    assert!(result.tasks.iter().all(|t| t.state == TaskState::Done));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_all_wait_with_no_task_in_scope_is_done_at_once() {
    let d = daemon().await;
    let mut w = world(&d).await;

    let result = wait_on(&mut w.owner, WaitSpec::All { scope: None }, 10_000, None).await;

    assert_eq!(result.condition, "all");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert!(result.targets.is_empty());
    assert!(result.tasks.is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_on_an_unknown_task_is_not_found() {
    let d = daemon().await;
    let mut w = world(&d).await;

    let error = wait_result(
        &mut w.owner,
        tasks_spec(vec!["no-such-task".to_string()], false),
        1_000,
        None,
    )
    .await
    .expect_err("an unknown task is refused");

    assert_eq!(error.code, ErrorCode::NotFound);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cards_wait_wakes_when_the_claim_owner_moves_the_card_to_review() {
    let d = daemon().await;
    let mut w = world(&d).await;
    card_in_doing(&mut w, "c1").await;
    let mut alice = w.alice;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.bob,
        WaitSpec::Cards {
            ids: vec!["c1".to_string()],
            states: vec!["review".to_string()],
        },
        WAIT_MS,
        None,
    );
    let mover = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        let op = alice.op("card.move", "c1", "review");
        call(&mut alice, Req::CardMove { op })
            .await
            .expect("card moves to review");
    });

    let (_bob, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    join(mover).await;
    let result = result.expect("the cards wait is accepted");

    assert_eq!(result.condition, "cards");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec!["c1".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_messages_wait_wakes_when_a_message_is_posted_to_its_channel() {
    let d = daemon().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut alice = w.alice;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.bob,
        WaitSpec::Messages {
            channel: "g".to_string(),
            after: 0,
        },
        WAIT_MS,
        None,
    );
    let poster = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        alice.post("g", "wake up").await
    });

    join(poster).await;
    let (_bob, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    let result = result.expect("the messages wait is accepted");

    assert_eq!(result.condition, "messages");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec!["g".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_messages_wait_does_not_return_for_a_message_at_or_before_its_after_seq() {
    let d = daemon().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "already read").await;

    let result = wait_on(
        &mut w.bob,
        WaitSpec::Messages {
            channel: "g".to_string(),
            after: seq,
        },
        300,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.pending, vec!["g".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notices_wait_from_now_does_not_return_a_notice_posted_before_it_started() {
    let d = daemon().await;
    let mut w = world(&d).await;
    create_card(&mut w.owner, "n1").await;
    let alice_pk = w.alice.pk_hex();
    let notified = escalate(&mut w.owner, "n1", "first decision", vec![alice_pk]).await;
    assert_eq!(notified.len(), 1, "the escalation notified alice");

    let result = wait_on(&mut w.alice, WaitSpec::Notices { after: -1 }, 300, None).await;

    assert_eq!(result.condition, "notices");
    assert!(
        result.timed_out,
        "the notice posted before the wait is not new"
    );
    assert_eq!(result.status, 124);
    assert!(result.cursor.is_some());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_notices_wait_returns_on_a_notice_posted_while_it_waits() {
    let d = daemon().await;
    let mut w = world(&d).await;
    create_card(&mut w.owner, "n1").await;
    let alice_pk = w.alice.pk_hex();
    escalate(&mut w.owner, "n1", "first decision", vec![alice_pk.clone()]).await;
    let first = wait_on(&mut w.alice, WaitSpec::Notices { after: -1 }, 300, None).await;
    assert!(first.timed_out);

    let mut owner = w.owner;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.alice,
        WaitSpec::Notices { after: -1 },
        WAIT_MS,
        first.cursor,
    );
    let escalator = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        escalate(&mut owner, "n1", "second decision", vec![alice_pk]).await;
    });

    let (_alice, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    join(escalator).await;
    let result = result.expect("the resumed notices wait is accepted");

    assert_eq!(result.condition, "notices");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pid_wait_returns_status_zero_once_the_child_has_exited() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let mut child = Command::new("sleep")
        .arg("1")
        .spawn()
        .expect("start a child process");
    let pid = i64::from(child.id());

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Pid { pids: vec![pid] },
        WAIT_MS,
        None,
    )
    .await;

    assert_eq!(result.condition, "pid");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![pid.to_string()]);
    assert!(
        child.try_wait().expect("poll the child").is_some(),
        "the wait returned before the child exited"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_wait_returns_when_the_file_is_created() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("ready.txt");
    let started = Instant::now();
    let writer = create_later(file.clone(), b"done\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::File {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert_eq!(result.condition, "file");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0, "a file that names no status ends with 0");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_wait_with_one_path_ends_with_the_number_the_file_holds() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("status.txt");
    fs::write(&file, b"7\n").expect("write the status file");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::File {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 7);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gone_wait_returns_when_the_file_is_removed() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("lock.txt");
    fs::write(&file, b"held\n").expect("write the lock file");
    let started = Instant::now();
    let remover = remove_later(file.clone());

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Gone {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(remover).await;

    assert_eq!(result.condition, "gone");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_matches_a_line_written_after_the_wait_began() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("build.log");
    let started = Instant::now();
    let writer = append_later(log.clone(), b"booting\nready\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^ready$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert_eq!(result.condition, "log");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![path_str(&log)]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_line_split_across_two_writes_matches_once_the_line_is_complete() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("split.log");
    fs::write(&log, b"hello").expect("write the first half");
    let started = Instant::now();
    let writer = append_later(log.clone(), b" world\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^hello world$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert!(!result.timed_out, "the completed line matches");
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_line_already_in_the_file_matches_at_once() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("old.log");
    fs::write(&log, b"booting\nready\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^ready$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_pattern_with_non_ascii_text_matches_when_its_character_is_split_across_writes() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("unicode.log");
    // "é" is C3 A9; the first write ends inside it.
    fs::write(&log, b"caf\xC3").expect("write the first bytes");
    let started = Instant::now();
    let writer = append_later(log.clone(), b"\xA9\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^café$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert!(
        !result.timed_out,
        "the character is whole once its last byte lands"
    );
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_refuses_a_pattern_that_is_not_a_regular_expression() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("any.log");
    fs::write(&log, b"x\n").expect("write the log");

    let error = wait_result(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "(".to_string(),
        },
        1_000,
        None,
    )
    .await
    .expect_err("an invalid pattern is refused");

    assert_eq!(error.code, ErrorCode::Invalid);
    assert!(
        error.message.contains("not a valid regular expression"),
        "the message names the pattern error, got {:?}",
        error.message
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_that_times_out_reports_124_with_a_cursor_and_its_log_as_target() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("quiet.log");
    fs::write(&log, b"nothing here\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^never$".to_string(),
        },
        500,
        None,
    )
    .await;

    assert_eq!(result.condition, "log");
    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.targets, vec![path_str(&log)]);
    assert!(result.cursor.is_some(), "a timed-out wait returns a cursor");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_log_wait_lists_its_log_as_pending() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("pending.log");
    fs::write(&log, b"nothing here\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^never$".to_string(),
        },
        500,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.pending, vec![path_str(&log)]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resuming_a_timed_out_wait_with_its_cursor_adds_the_new_seconds_to_the_old() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("resume.log");
    fs::write(&log, b"nothing here\n").expect("write the log");
    let spec = || WaitSpec::Log {
        path: path_str(&log),
        pattern: "^never$".to_string(),
    };
    let first = wait_on(&mut w.alice, spec(), 500, None).await;
    assert!(first.timed_out);

    let second = wait_on(&mut w.alice, spec(), 500, first.cursor.clone()).await;

    assert!(second.timed_out);
    assert_eq!(second.status, 124);
    assert!(
        second.seconds >= first.seconds + 0.4,
        "the second wait adds its own ~0.5 s to the first: {} then {}",
        first.seconds,
        second.seconds
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_asking_longer_than_the_cap_still_returns_when_its_condition_is_met() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let mut child = Command::new("sleep")
        .arg("1")
        .spawn()
        .expect("start a child process");
    let pid = i64::from(child.id());

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Pid { pids: vec![pid] },
        120_000,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    child.wait().expect("reap the child");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_naming_more_than_256_targets_is_refused_as_invalid() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let paths = absent_paths(d.home.path(), 257);

    let error = wait_result(&mut w.alice, WaitSpec::Gone { paths }, 1_000, None)
        .await
        .expect_err("257 targets are refused");

    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.message, "name at most 256 targets");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_naming_exactly_256_targets_is_accepted() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let paths = absent_paths(d.home.path(), 256);

    let result = wait_on(&mut w.alice, WaitSpec::Gone { paths }, WAIT_MS, None).await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets.len(), 256);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_is_refused_every_pid_file_gone_and_log_wait() {
    let d = daemon().await;
    let mut w = world(&d).await;
    grant(&mut w.owner, w.carol.pk_hex(), "observer").await;
    let file = d.home.path().join("watched.txt");
    fs::write(&file, b"x\n").expect("write the file");
    let me = i64::from(std::process::id());
    let specs = [
        ("pid", WaitSpec::Pid { pids: vec![me] }),
        (
            "file",
            WaitSpec::File {
                paths: vec![path_str(&file)],
            },
        ),
        (
            "gone",
            WaitSpec::Gone {
                paths: vec![path_str(&file)],
            },
        ),
        (
            "log",
            WaitSpec::Log {
                path: path_str(&file),
                pattern: "x".to_string(),
            },
        ),
    ];

    for (name, spec) in specs {
        let error = wait_result(&mut w.carol, spec, 1_000, None)
            .await
            .expect_err(name);
        assert_eq!(
            error.code,
            ErrorCode::Forbidden,
            "a {name} wait by an observer"
        );
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_may_wait_on_a_task() {
    let d = daemon().await;
    let mut w = world(&d).await;
    grant(&mut w.owner, w.carol.pk_hex(), "observer").await;
    let task = run_inline(&mut w.owner, d.home.path(), &["true"]).await;

    let result = wait_on(
        &mut w.carol,
        tasks_spec(vec![task.id], false),
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_may_not_wait_on_a_job_log_by_path_but_a_supervisor_may() {
    let d = daemon().await;
    let mut w = world(&d).await;
    // A job's log where the daemon keeps it; the pattern already matches, so a permitted wait returns at once.
    let log = jobdir::log_path(&jobdir::work_root(d.home.path()), "gate-job");
    fs::create_dir_all(log.parent().expect("a logs directory")).expect("create the logs directory");
    fs::write(&log, b"ready\n").expect("write the job log");
    let spec = WaitSpec::Log {
        path: path_str(&log),
        pattern: "^ready$".to_string(),
    };

    // Bob registered as a worker: a job's log is read through its task wait, which applies S-2, not by path.
    let error = wait_result(&mut w.bob, spec.clone(), 1_000, None)
        .await
        .expect_err("a worker's log wait under the work root");
    assert_eq!(error.code, ErrorCode::Forbidden);

    grant(&mut w.owner, w.carol.pk_hex(), "supervisor").await;
    let result = wait_on(&mut w.carol, spec, WAIT_MS, None).await;
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![path_str(&log)]);
    d.stop().await;
}
