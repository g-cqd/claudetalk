//! Card and job (D7): a job linked to a card moves the card when the job ends, in the same transaction as the job's
//! finish. A finished job moves its card to review (through doing, from claimed); a failed job blocks the card and
//! names the failure for its owner; a cancelled job returns the card to ready with no owner. A card that is no longer
//! in a state the job's outcome can move, or that has changed hands since the job was added, stays where it is and
//! its current owner is told why.
//!
//! Not asserted here: the evidence reference `job:<id> status=… log=…` and the actor of the card's own events. The
//! daemon has no read path for either (no reply carries evidence refs, and the work log returns only `work.*` and
//! `job.*` events). The store tests check the reference string.
//!
//! Real child processes under the `agora-supervise` binary; every wait is bounded, and admission uses `FakeMachine`,
//! so host load never holds a job.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agora_proto::{CardView, Reply, Req};
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::model::{
    AcceptArgs, AddArgs, CancelArgs, CardExt, NoticeRecord, RunArgs, TaskRecord, TaskState,
    WaitResult, WaitSpec,
};
use common::*;
use serde_json::json;

/// Bound for a job that runs to its own end.
const JOB_WITHIN: Duration = Duration::from_secs(20);
/// Bound for a job to be reported running after it was requested.
const RUNNING_WITHIN: Duration = Duration::from_secs(15);
/// Bound for a running job to end after `work.cancel`: TERM to the group, KILL after 5 s (D9).
const CANCEL_WITHIN: Duration = Duration::from_secs(7);
/// The longest one WorkWait asks for; the server clamps larger requests to the same bound.
const WAIT_MS: u64 = 55_000;
/// The timeout of the WorkWait that re-reads a task which is not running yet.
const PACE_MS: u64 = 20;
/// The command of a job that keeps running until it is cancelled.
const SLEEPS: &[&str] = &["sh", "-c", "sleep 30"];

/// A daemon whose admission machine is the fake (idle, roomy, and the same whatever the host is doing).
async fn daemon() -> TestDaemon {
    let machine: Arc<dyn Machine> = FakeMachine::new();
    start_injected(None, Some(machine), |_| {}).await
}

/// The world, with carol granted the supervisor role by the owner: carol plans the board (creates, readies,
/// reclaims and cancels cards); alice and bob are workers.
async fn board_world(d: &TestDaemon) -> World {
    let mut w = world(d).await;
    let op = w.owner.op("role.grant", &w.carol.pk_hex(), "supervisor");
    match w.owner.ok(Req::RoleGrant { op }).await {
        Reply::Unit => {}
        other => panic!("RoleGrant: expected Unit, got {other:?}"),
    }
    w
}

/// Carol creates `card` in lane "l" and readies it; alice claims it and starts it, so it is in doing and owned by
/// alice.
async fn card_in_doing(w: &mut World, card: &str) {
    create_card(&mut w.carol, card).await;
    move_card(&mut w.carol, card, "ready").await;
    move_card(&mut w.alice, card, "claimed").await;
    move_card(&mut w.alice, card, "doing").await;
}

async fn create_card(who: &mut TestClient, card: &str) {
    let arg = json!({"title": card, "lane": "l"}).to_string();
    let op = who.op("card.create", card, &arg);
    expect_unit(who.ok(Req::CardCreate { op }).await, "card.create");
}

/// A card.move; the arg is the target state.
async fn move_card(who: &mut TestClient, card: &str, to: &str) {
    let op = who.op("card.move", card, to);
    expect_unit(who.ok(Req::CardMove { op }).await, "card.move");
}

async fn reclaim_card(who: &mut TestClient, card: &str) {
    let op = who.op("card.reclaim", card, "");
    expect_unit(who.ok(Req::CardReclaim { op }).await, "card.reclaim");
}

fn expect_unit(reply: Reply, what: &str) {
    match reply {
        Reply::Unit => {}
        other => panic!("{what}: expected Unit, got {other:?}"),
    }
}

/// The card `id` as the board lists it now.
async fn card(who: &mut TestClient, id: &str) -> CardView {
    let req = Req::CardList {
        state: None,
        lane: None,
        limit: 500,
    };
    match who.ok(req).await {
        Reply::Cards { cards } => cards
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("card {id} is listed")),
        other => panic!("CardList: expected Cards, got {other:?}"),
    }
}

/// The card's scheduling extension (its linked jobs, needs_owner and owner_action).
async fn card_ext(who: &mut TestClient, id: &str) -> CardExt {
    match who
        .ok(Req::CardExt {
            ids: vec![id.to_string()],
        })
        .await
    {
        Reply::CardExts { cards } => cards
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("card {id} has an extension")),
        other => panic!("CardExt: expected CardExts, got {other:?}"),
    }
}

/// Every notice addressed to the caller, oldest first.
async fn notices(who: &mut TestClient) -> Vec<NoticeRecord> {
    match who
        .ok(Req::Notices {
            after: 0,
            limit: 100,
        })
        .await
    {
        Reply::Notices { notices } => notices,
        other => panic!("Notices: expected Notices, got {other:?}"),
    }
}

/// The notices the card sync wrote to the caller (kind `job_sync`), oldest first.
async fn job_sync_notices(who: &mut TestClient) -> Vec<NoticeRecord> {
    notices(who)
        .await
        .into_iter()
        .filter(|n| n.kind == "job_sync")
        .collect()
}

/// `work.run` of the inline command `argv` in `cwd`, linked to `card`.
async fn run_linked(client: &mut TestClient, card: &str, argv: &[&str], cwd: &Path) -> TaskRecord {
    let add = AddArgs {
        title: format!("job for {card}"),
        command: Some(argv.iter().map(|arg| arg.to_string()).collect()),
        cwd: Some(cwd.display().to_string()),
        card: Some(card.to_string()),
        ..Default::default()
    };
    let arg = serde_json::to_string(&RunArgs {
        task: None,
        add: Some(add),
    })
    .expect("a RunArgs serializes");
    let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    let op = client.op("work.run", "", &arg);
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
async fn wait_once(client: &mut TestClient, id: &str, timeout_ms: u64) -> WaitResult {
    let spec = WaitSpec::Tasks {
        ids: vec![id.to_string()],
        any: false,
    };
    match client
        .ok(Req::WorkWait {
            spec,
            timeout_ms,
            cursor: None,
        })
        .await
    {
        Reply::Wait { result } => result,
        other => panic!("work.wait {id}: expected Wait, got {other:?}"),
    }
}

/// Waits until the task `id` ends, resuming with the cursor after each timeout; panics past `within`.
async fn wait_done(client: &mut TestClient, id: &str, within: Duration) -> WaitResult {
    tokio::time::timeout(within, async {
        let mut cursor = None;
        loop {
            let spec = WaitSpec::Tasks {
                ids: vec![id.to_string()],
                any: false,
            };
            let req = Req::WorkWait {
                spec,
                timeout_ms: WAIT_MS,
                cursor: cursor.take(),
            };
            let result = match client.ok(req).await {
                Reply::Wait { result } => result,
                other => panic!("work.wait {id}: expected Wait, got {other:?}"),
            };
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
async fn await_running(client: &mut TestClient, id: &str) {
    tokio::time::timeout(RUNNING_WITHIN, async {
        loop {
            let task = show(client, id).await;
            match task.state {
                TaskState::Running => return,
                state if state.is_final() => {
                    panic!("task {id} ended ({}) before it ran", state.as_str())
                }
                _ => {}
            }
            wait_once(client, id, PACE_MS).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {id} did not start within {RUNNING_WITHIN:?}"))
}

/// `work.cancel` of the task `id`; the reply is not used, the caller waits for the end.
async fn cancel_job(client: &mut TestClient, id: &str, note: &str) {
    let arg = serde_json::to_string(&CancelArgs {
        note: note.to_string(),
    })
    .expect("a CancelArgs serializes");
    let op = client.op("work.cancel", id, &arg);
    match client.ok(Req::WorkCancel { op }).await {
        Reply::Task { .. } => {}
        other => panic!("work.cancel {id}: expected Task, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_exits_zero_moves_its_doing_card_to_review() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;
    let task = run_linked(&mut w.alice, "c1", &["true"], &cwd).await;

    let ended = wait_done(&mut w.alice, &task.id, JOB_WITHIN).await;
    assert_eq!(ended.status, 0);
    assert_eq!(show(&mut w.alice, &task.id).await.state, TaskState::Done);

    let after = card(&mut w.alice, "c1").await;
    assert_eq!(after.state, "review");
    assert_eq!(after.owner, Some(w.alice.pk_hex()));
    let ext = card_ext(&mut w.alice, "c1").await;
    assert_eq!(ext.jobs, [task.id.as_str()]);
    assert!(
        !ext.needs_owner,
        "a finished job asks its owner for nothing"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_exits_nonzero_blocks_its_card_and_names_the_failure_for_the_owner() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;
    let task = run_linked(&mut w.alice, "c1", &["sh", "-c", "exit 3"], &cwd).await;

    let ended = wait_done(&mut w.alice, &task.id, JOB_WITHIN).await;
    assert_eq!(ended.status, 3);

    assert_eq!(card(&mut w.alice, "c1").await.state, "blocked");
    let ext = card_ext(&mut w.alice, "c1").await;
    assert!(ext.needs_owner, "a failed job flags its card for its owner");
    assert!(
        ext.owner_action
            .contains(&format!("job {} failed with status 3", task.id)),
        "owner_action names the job and its status, got {:?}",
        ext.owner_action
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_running_job_returns_its_card_to_ready_with_no_owner() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;
    let task = run_linked(&mut w.alice, "c1", SLEEPS, &cwd).await;
    await_running(&mut w.alice, &task.id).await;

    cancel_job(&mut w.alice, &task.id, "no longer needed").await;
    wait_done(&mut w.alice, &task.id, CANCEL_WITHIN).await;
    assert_eq!(
        show(&mut w.alice, &task.id).await.state,
        TaskState::Cancelled
    );

    let after = card(&mut w.alice, "c1").await;
    assert_eq!(after.state, "ready");
    assert_eq!(after.owner, None, "a card returned to ready has no owner");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_exits_zero_on_a_claimed_card_moves_it_through_doing_to_review() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    create_card(&mut w.carol, "c1").await;
    move_card(&mut w.carol, "c1", "ready").await;
    move_card(&mut w.alice, "c1", "claimed").await;
    let task = run_linked(&mut w.alice, "c1", &["true"], &cwd).await;

    wait_done(&mut w.alice, &task.id, JOB_WITHIN).await;

    let after = card(&mut w.alice, "c1").await;
    assert_eq!(after.state, "review");
    assert_eq!(after.owner, Some(w.alice.pk_hex()));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_card_was_cancelled_meanwhile_leaves_the_card_and_tells_the_owner() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;
    let task = run_linked(&mut w.alice, "c1", SLEEPS, &cwd).await;
    await_running(&mut w.alice, &task.id).await;
    move_card(&mut w.carol, "c1", "cancelled").await;

    cancel_job(&mut w.alice, &task.id, "stop").await;
    wait_done(&mut w.alice, &task.id, CANCEL_WITHIN).await;

    assert_eq!(card(&mut w.alice, "c1").await.state, "cancelled");
    let told = job_sync_notices(&mut w.alice).await;
    assert_eq!(
        told.len(),
        1,
        "the owner hears once why the card did not move"
    );
    assert_eq!(told[0].subject, "c1");
    assert!(
        told[0].text.contains(&task.id),
        "the notice names the job, got {:?}",
        told[0].text
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_card_changed_hands_leaves_the_card_and_tells_the_new_owner() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;
    let task = run_linked(&mut w.alice, "c1", SLEEPS, &cwd).await;
    await_running(&mut w.alice, &task.id).await;
    reclaim_card(&mut w.carol, "c1").await;
    move_card(&mut w.bob, "c1", "claimed").await;

    cancel_job(&mut w.alice, &task.id, "stop").await;
    wait_done(&mut w.alice, &task.id, CANCEL_WITHIN).await;

    let after = card(&mut w.bob, "c1").await;
    assert_eq!(
        after.state, "claimed",
        "bob's claim is not moved by alice's job"
    );
    assert_eq!(after.owner, Some(w.bob.pk_hex()));
    assert_eq!(job_sync_notices(&mut w.bob).await.len(), 1);
    assert!(
        job_sync_notices(&mut w.alice).await.is_empty(),
        "the job's author no longer owns the card and is not told"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_job_run_by_another_identity_still_moves_the_card_its_author_owns() {
    let d = daemon().await;
    let mut w = board_world(&d).await;
    let cwd = std::fs::canonicalize(d.home.path()).expect("canonical home");
    card_in_doing(&mut w, "c1").await;

    let add = AddArgs {
        title: "queued for c1".to_string(),
        command: Some(vec!["true".to_string()]),
        cwd: Some(cwd.display().to_string()),
        card: Some("c1".to_string()),
        ..Default::default()
    };
    let arg = serde_json::to_string(&add).expect("an AddArgs serializes");
    let op = w.alice.op("work.add", "", &arg);
    let queued = match w.alice.ok(Req::WorkAdd { op }).await {
        Reply::Task { task } => task,
        other => panic!("work.add: expected Task, got {other:?}"),
    };

    let op = w.bob.op("work.claim", &queued.id, "");
    match w.bob.ok(Req::WorkClaim { op }).await {
        Reply::Task { .. } => {}
        other => panic!("work.claim: expected Task, got {other:?}"),
    }
    let digest = show(&mut w.bob, &queued.id)
        .await
        .digest
        .expect("a queued command has a digest");
    let arg = serde_json::to_string(&AcceptArgs { digest }).expect("an AcceptArgs serializes");
    let op = w.bob.op("work.accept", &queued.id, &arg);
    match w.bob.ok(Req::WorkAccept { op }).await {
        Reply::Accepted { .. } => {}
        other => panic!("work.accept: expected Accepted, got {other:?}"),
    }
    let arg = serde_json::to_string(&RunArgs {
        task: Some(queued.id.clone()),
        add: None,
    })
    .expect("a RunArgs serializes");
    let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    let op = w.bob.op("work.run", "", &arg);
    match w.bob.ok(Req::WorkRun { op, env }).await {
        Reply::Task { .. } => {}
        other => panic!("work.run: expected Task, got {other:?}"),
    }

    let ended = wait_done(&mut w.bob, &queued.id, JOB_WITHIN).await;
    assert_eq!(ended.status, 0);

    assert_eq!(card(&mut w.alice, "c1").await.state, "review");
    d.stop().await;
}
