//! `Req::WorkWait` through the daemon (D10): each condition's status and wake-up, the timeout and cursor rules, the
//! server cap, the target bound, and the Worker gate of the filesystem and process waits (R4-1). Real files and real
//! child processes; every wait is bounded, and admission uses `FakeMachine` so host load never holds a job.

#[path = "../common/mod.rs"]
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
mod events;
mod files;
mod limits;
mod tasks;

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
