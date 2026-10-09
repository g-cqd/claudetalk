//! The reconciler through the daemon (D12, SPEC section 6): the stale-card ladder (a notice to the owner at 30 min idle,
//! escalation to the supervisors at 45, reclaim to ready at 60), path lease expiry at its deadline, the disk floor (a
//! blocker card and paused admission, then recovery), the WIP limit and the sprint-end notice.
//!
//! The daemon runs a pass on its own timer, and `reconciler::pass` needs the daemon's internal state, so the tests move
//! the fake clock and wait for the pass it makes due. `SpyClock` wraps the fake clock and records each `sleep_until`
//! deadline: after a pass at time `t` the reconciler sleeps until `t + 30 s`, so that deadline being recorded is the
//! signal that the pass is over. No test sleeps; every wait is bounded by `WAIT`.
//!
//! Client ops are signed with the wall clock and checked against the fake clock (±5 min), so every client op is made
//! before the clock moves. The tests after that only read.
//!
//! Not covered: the reconcile events' actor (the system pk). No daemon request reads the events table, and `WorkLog`
//! reads only `work.*` and `job.*` events, so there is no accessor to assert through.

#[path = "../common/mod.rs"]
mod common;

use agora_daemon::clock::{Clock, FakeClock};
use agora_daemon::now_ms;
use agora_proto::{CardView, Reply, Req};
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::model::{
    BoardExtras, LeaseRecord, NeedsOwner, TaskRecord, TaskState, WaitResult, WaitSpec,
};
use agora_sched::model_ops::{AddArgs, RunArgs};
use common::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;

mod disk;
mod lease;
mod limits;
mod stale;

/// Bound on every wait, so a daemon bug fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(30);
const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;
/// The default `reconcile_interval_s` (SPEC D12): the reconciler sleeps this long after each pass.
const INTERVAL_MS: i64 = 30 * SECOND;
const GIB_KIB: u64 = 1024 * 1024;
/// The disk reading that breaches the default floor of 5 GiB, and the text the daemon derives from it.
const LOW_DISK_KIB: u64 = 3 * GIB_KIB;
const DISK_BREACH: &str = "disk free 3.0 GiB below the floor of 5.0 GiB";
const BLOCKER_TITLE: &str = "Machine floor breached: disk free 3.0 GiB below the floor of 5.0 GiB";
const BLOCKER_ACTION: &str =
    "free disk space (3.0 GiB < floor 5.0 GiB) — admission is paused until then";

/// Wraps a `FakeClock` and records the deadline of every `sleep_until` call. A spy rather than a poll: the test
/// waits on the watch channel for the reconciler's post-pass sleep.
struct SpyClock {
    clock: Arc<FakeClock>,
    deadlines: watch::Sender<Vec<i64>>,
}

impl SpyClock {
    fn new(start_ms: i64) -> Arc<SpyClock> {
        Arc::new(SpyClock {
            clock: FakeClock::new(start_ms),
            deadlines: watch::channel(Vec::new()).0,
        })
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Resolves once the reconciler has finished its pass at `pass_ms` and gone back to sleep.
    async fn pass_done_at(&self, pass_ms: i64) {
        let want = pass_ms + INTERVAL_MS;
        let mut seen = self.deadlines.subscribe();
        match timeout(WAIT, seen.wait_for(|d| d.contains(&want))).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => panic!("the clock's record channel closed"),
            Err(_) => panic!("no reconcile pass at {pass_ms} within {WAIT:?}"),
        }
    }

    /// Moves the clock to `at_ms` and waits for the pass that the move makes due.
    async fn pass_at(&self, at_ms: i64) {
        self.clock.set(at_ms);
        self.pass_done_at(at_ms).await;
    }
}

impl Clock for SpyClock {
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.deadlines.send_modify(|d| d.push(deadline_ms));
        self.clock.sleep_until(deadline_ms)
    }
}

/// A daemon on a fake clock and a fake machine, with the world's clients and the startup pass done.
struct Rig {
    d: TestDaemon,
    clock: Arc<SpyClock>,
    machine: Arc<FakeMachine>,
    /// The clock at start; every scripted instant is relative to it.
    start: i64,
    /// The instant the stale-card timeline is measured from: the card's last update. The store stamps card moves
    /// from the wall clock (M1-M4 code keeps `now_ms()`), not from the injected clock, so it can sit a little after
    /// `start`. Defaults to `start`.
    anchor: i64,
    w: World,
}

async fn rig() -> Rig {
    let clock = SpyClock::new(now_ms());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let machine = FakeMachine::new();
    let machine_dyn: Arc<dyn Machine> = machine.clone();
    let d = start_injected(Some(clock_dyn), Some(machine_dyn), |_| {}).await;
    let start = clock.now();
    // The startup pass runs at the start instant. Wait for it before any state changes.
    clock.pass_done_at(start).await;
    let w = world(&d).await;
    Rig {
        d,
        clock,
        machine,
        start,
        anchor: start,
        w,
    }
}

async fn grant_supervisor(owner: &mut TestClient, who: &TestClient) {
    let op = owner.op("role.grant", &who.pk_hex(), "supervisor");
    match owner.ok(Req::RoleGrant { op }).await {
        Reply::Unit => {}
        other => panic!("role.grant: expected Unit, got {other:?}"),
    }
}

async fn create_card(c: &mut TestClient, card: &str, arg: serde_json::Value) {
    let op = c.op("card.create", card, &arg.to_string());
    c.ok(Req::CardCreate { op }).await;
}

async fn move_card(c: &mut TestClient, card: &str, to: &str) {
    let op = c.op("card.move", card, to);
    c.ok(Req::CardMove { op }).await;
}

/// Cards in `state`, every lane, sorted by id.
async fn cards_in(c: &mut TestClient, state: &str) -> Vec<CardView> {
    let req = Req::CardList {
        state: Some(state.to_string()),
        lane: None,
        limit: 100,
    };
    let mut cards = match c.ok(req).await {
        Reply::Cards { cards } => cards,
        other => panic!("card.list: expected Cards, got {other:?}"),
    };
    cards.sort_by(|a, b| a.id.cmp(&b.id));
    cards
}

/// The notices for `c` as (kind, subject, text), in the order the store holds them.
async fn notices(c: &mut TestClient) -> Vec<(String, String, String)> {
    match c
        .ok(Req::Notices {
            after: 0,
            limit: 100,
        })
        .await
    {
        Reply::Notices { notices } => notices
            .into_iter()
            .map(|n| (n.kind, n.subject, n.text))
            .collect(),
        other => panic!("notices: expected Notices, got {other:?}"),
    }
}

fn notice(kind: &str, subject: &str, text: &str) -> (String, String, String) {
    (kind.to_string(), subject.to_string(), text.to_string())
}

async fn board_extras(c: &mut TestClient) -> BoardExtras {
    match c.ok(Req::BoardExtras).await {
        Reply::BoardExtras { extras } => extras,
        other => panic!("board extras: expected BoardExtras, got {other:?}"),
    }
}

/// The admission pause the machine view reports.
async fn status_paused(c: &mut TestClient) -> Option<String> {
    match c.ok(Req::WorkStatus { scope: None }).await {
        Reply::WorkStatus { report } => report.machine.paused,
        other => panic!("work.status: expected WorkStatus, got {other:?}"),
    }
}

async fn show(c: &mut TestClient, task: &str) -> TaskRecord {
    match c
        .ok(Req::WorkShow {
            task: task.to_string(),
        })
        .await
    {
        Reply::Task { task } => task,
        other => panic!("work.show {task}: expected Task, got {other:?}"),
    }
}

/// Runs `/bin/sh -c 'exit 0'` inline as `c`, in the daemon's home, and returns the task.
async fn run_inline(c: &mut TestClient, cwd: &std::path::Path) -> TaskRecord {
    let add = AddArgs {
        title: "reconciler test".to_string(),
        command: Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ]),
        cwd: Some(cwd.display().to_string()),
        ..Default::default()
    };
    let arg = serde_json::to_string(&RunArgs {
        task: None,
        add: Some(add),
    })
    .expect("a RunArgs serializes");
    let op = c.op("work.run", "", &arg);
    match c
        .ok(Req::WorkRun {
            op,
            env: BTreeMap::new(),
        })
        .await
    {
        Reply::Task { task } => task,
        other => panic!("work.run: expected Task, got {other:?}"),
    }
}

/// Re-reads the task until its note says the admission hold. The note is set by the admission loop with no bus
/// event, so the re-read is the only way to observe it; the loop is bounded by `WAIT`.
async fn await_hold_note(c: &mut TestClient, task: &str, note: &str) -> TaskRecord {
    timeout(WAIT, async {
        loop {
            let record = show(c, task).await;
            if record.note == note {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task {task} never got the note {note:?}"))
}

/// One WorkWait on the task, returning when it ends. The awaiter's own timeout runs on the fake clock, so the
/// wait returns only when the task ends.
async fn wait_for_end(c: &mut TestClient, task: &str) -> WaitResult {
    let spec = WaitSpec::Tasks {
        ids: vec![task.to_string()],
        any: false,
    };
    match timeout(
        WAIT,
        c.ok(Req::WorkWait {
            spec,
            timeout_ms: 55_000,
            cursor: None,
        }),
    )
    .await
    {
        Ok(Reply::Wait { result }) => result,
        Ok(other) => panic!("work.wait: expected Wait, got {other:?}"),
        Err(_) => panic!("task {task} did not end within {WAIT:?}"),
    }
}

/// Creates `card` in `team`, lane "l", and has alice claim it and move it to doing. Everything is signed at the start
/// instant, before the clock moves.
async fn doing_card(w: &mut World, card: &str, team: &str) {
    let arg = json!({"title": "stale card", "lane": "l", "team": team});
    create_card(&mut w.carol, card, arg).await;
    move_card(&mut w.carol, card, "ready").await;
    move_card(&mut w.alice, card, "claimed").await;
    move_card(&mut w.alice, card, "doing").await;
}

/// The rig with carol as a supervisor and alice holding `c1` in doing. The timeline is measured from `c1`'s last
/// update, which the card list reports.
async fn stale_rig() -> Rig {
    let mut r = rig().await;
    grant_supervisor(&mut r.w.owner, &r.w.carol).await;
    doing_card(&mut r.w, "c1", "").await;
    let updated = cards_in(&mut r.w.owner, "doing")
        .await
        .into_iter()
        .find(|c| c.id == "c1")
        .expect("c1 is in doing")
        .updated;
    r.anchor = updated;
    r
}

/// Runs the passes at the given minutes after the card's last update, in order. A test that checks one stage runs
/// the earlier ones first, as the daemon does in production: the first stale notice's text depends on the pass time.
async fn passes_at_minutes(r: &Rig, minutes: &[i64]) {
    for m in minutes {
        r.clock.pass_at(r.anchor + m * MINUTE).await;
    }
}

/// Notice texts for the stale card `c1` (titled "stale card") at the scripted idle times: 31 min (stale), 46 min
/// (escalation, owner named by its pseudonym: the first eight characters of its public key), 61 min (reclaimed).
const STALE_31: &str = "card c1 (stale card) has been doing for 31 min 0 s without activity; move it, add evidence, or release it";
const RECLAIMED_61: &str = "card c1 (stale card) was reclaimed after 1 h 1 min without activity; it is ready again and no longer yours";

fn escalation_46(owner: &TestClient) -> String {
    format!(
        "card c1 (stale card) owned by {} has been doing for 46 min 0 s; it will be reclaimed after 1 h 0 min",
        &owner.pk_hex()[..8]
    )
}
