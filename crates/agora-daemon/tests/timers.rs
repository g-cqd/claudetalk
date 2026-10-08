//! Timers through the daemon (D11, SPEC section 8): `at`, `every` and `cron` timers fire as `timer.fire` system ops when
//! the fake clock reaches their instant. A fire enqueues the timer's template as a queued task authored by the timer's
//! owner, with origin `timer:<id>` and id `<timer>-<n>`. Missed periods coalesce into one fire, an `at` timer disables
//! itself after it fires, the per-identity quota refuses creates past `timer_quota` enabled timers, and only the owner or
//! a Supervisor may delete a timer.
//!
//! Signed client ops carry the wall clock and the daemon checks them against the fake clock (±5 min). So a client op is
//! accepted only while the fake clock is near the wall time: every timer and every sentinel (see `sentinel`) is created
//! before the clock moves, and the tests after that only read.
//!
//! The loop fires on its own task, so every wait polls the work queue within `WAIT`. A "nothing else fired" claim is
//! trusted only after `settle`: the loop fires the timers due at an instant in `next_fire` order within one pass, so a
//! sentinel due at that instant fires after all of them.

mod common;

use agora_daemon::clock::{Clock, FakeClock};
use agora_daemon::now_ms;
use agora_proto::{ErrorBody, ErrorCode, Reply, Req};
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::model::{TaskFilter, TaskRecord, TaskState, TimerRecord};
use agora_sched::model_ops::{AddArgs, TimerArgs};
use agora_sched::timer::TimerSpec;
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Bound on every wait, so a daemon bug fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(30);
const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
/// The `every` period and the `cron` step used throughout.
const FIVE_MINUTES: i64 = 5 * MINUTE;

/// A daemon whose clock starts at the wall time and whose machine is fake.
struct Rig {
    d: TestDaemon,
    clock: Arc<FakeClock>,
}

async fn rig() -> Rig {
    let clock = FakeClock::new(now_ms());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let machine: Arc<dyn Machine> = FakeMachine::new();
    let d = start_injected(Some(clock_dyn), Some(machine), |_| {}).await;
    Rig { d, clock }
}

/// Creates a timer owned by `c` with a one-line template; `Err` is the daemon's refusal.
async fn try_create(
    c: &mut TestClient,
    id: Option<&str>,
    spec: TimerSpec,
    title: &str,
) -> Result<TimerRecord, ErrorBody> {
    let args = TimerArgs {
        id: id.map(str::to_owned),
        spec,
        template: AddArgs {
            title: title.to_owned(),
            ..AddArgs::default()
        },
    };
    let arg = serde_json::to_string(&args).expect("timer args serialize");
    let op = c.op("timer.create", id.unwrap_or(""), &arg);
    match c.call(Req::TimerCreate { op }).await? {
        Reply::Timer { timer } => Ok(timer),
        other => panic!("timer.create: expected Timer, got {other:?}"),
    }
}

async fn create(c: &mut TestClient, id: Option<&str>, spec: TimerSpec, title: &str) -> TimerRecord {
    try_create(c, id, spec, title)
        .await
        .unwrap_or_else(|e| panic!("timer.create refused: {e}"))
}

/// Deletes timer `id` as `c`; `Err` is the daemon's refusal.
async fn try_delete(c: &mut TestClient, id: &str) -> Result<(), ErrorBody> {
    let op = c.op("timer.delete", id, "{}");
    match c.call(Req::TimerDelete { op }).await? {
        Reply::Unit => Ok(()),
        other => panic!("timer.delete: expected Unit, got {other:?}"),
    }
}

/// Every timer on the daemon.
async fn listed(c: &mut TestClient) -> Vec<TimerRecord> {
    match c.ok(Req::TimerList { owner: None }).await {
        Reply::Timers { timers } => timers,
        other => panic!("timer.list: expected Timers, got {other:?}"),
    }
}

/// The listed timer `id`; panics when it is not listed.
fn timer_named(timers: &[TimerRecord], id: &str) -> TimerRecord {
    timers
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("timer {id} is not listed"))
}

/// Sets the work settings through a signed `work.settings` by the owner (Controller+).
async fn set_settings(owner: &mut TestClient, patch: Value) {
    let op = owner.op("work.settings", "", &json!({"patch": patch}).to_string());
    match owner.ok(Req::WorkSettings { op: Some(op) }).await {
        Reply::WorkSettings { .. } => {}
        other => panic!("work.settings: expected WorkSettings, got {other:?}"),
    }
}

async fn all_tasks(c: &mut TestClient) -> Vec<TaskRecord> {
    match c
        .ok(Req::WorkList {
            filter: TaskFilter::default(),
        })
        .await
    {
        Reply::Tasks { tasks } => tasks,
        other => panic!("work.list: expected Tasks, got {other:?}"),
    }
}

/// The tasks `timer` enqueued (origin `timer:<id>`), in id order.
async fn fired_by(c: &mut TestClient, timer: &str) -> Vec<TaskRecord> {
    let origin = format!("timer:{timer}");
    let mut fired: Vec<TaskRecord> = all_tasks(c)
        .await
        .into_iter()
        .filter(|t| t.origin.as_deref() == Some(origin.as_str()))
        .collect();
    fired.sort_by(|a, b| a.id.cmp(&b.id));
    fired
}

/// Polls until the tasks `timer` enqueued have exactly the ids `expected`, within `WAIT`. The final read is compared
/// whether or not the wait ended in time, so a mismatch names the ids that did appear.
async fn await_fired(
    c: &mut TestClient,
    timer: &str,
    expected: &[impl AsRef<str>],
) -> Vec<TaskRecord> {
    let mut want: Vec<String> = expected.iter().map(|id| id.as_ref().to_owned()).collect();
    want.sort();
    let _ = timeout(WAIT, async {
        loop {
            let ids: Vec<String> = fired_by(c, timer).await.into_iter().map(|t| t.id).collect();
            if ids == want {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let fired = fired_by(c, timer).await;
    let got: Vec<String> = fired.iter().map(|t| t.id.clone()).collect();
    assert_eq!(got, want, "tasks fired by timer {timer}");
    fired
}

/// Creates the sentinel `settle-<n>`: an `at` timer due at `at_ms`, the instant the test advances to. It must be created
/// before the clock moves, because a client op is refused once the fake clock is more than 5 min from the wall time.
async fn sentinel(c: &mut TestClient, n: u32, at_ms: i64) {
    let id = format!("settle-{n}");
    create(c, Some(id.as_str()), TimerSpec::At { at_ms }, "settle").await;
}

/// Waits for the sentinel `settle-<n>` to fire. The loop fires the timers due at or before its instant in `next_fire`
/// order, so by then every one of them has fired and nothing more is due at that instant.
async fn settle(c: &mut TestClient, n: u32) {
    let id = format!("settle-{n}");
    let task = format!("{id}-1");
    await_fired(c, &id, &[task]).await;
}

/// An `every 300` timer created now, with a sentinel at the instant `catch_up` advances to (three hours after the
/// second period). Advances through two periods, then one large advance, then settles; returns the timer as created.
async fn catch_up(r: &Rig, w: &mut World) -> TimerRecord {
    let timer = create(
        &mut w.alice,
        Some("pulse"),
        TimerSpec::Every {
            every_s: 300,
            start_ms: None,
        },
        "pulse",
    )
    .await;
    let landing = r.clock.now_ms() + 601 * SECOND + 3 * HOUR;
    sentinel(&mut w.alice, 1, landing).await;

    r.clock.advance(301 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1"]).await;
    r.clock.advance(300 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1", "pulse-2"]).await;

    r.clock.advance(3 * HOUR);
    assert_eq!(r.clock.now_ms(), landing);
    settle(&mut w.alice, 1).await;
    timer
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_timer_fires_as_a_queued_task_authored_by_its_owner_with_the_timer_origin() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let due = r.clock.now_ms() + MINUTE;
    let timer = create(
        &mut w.alice,
        Some("nudge"),
        TimerSpec::At { at_ms: due },
        "nudge the reviewer",
    )
    .await;
    assert_eq!(
        timer.next_fire_ms,
        Some(due),
        "the first fire is the at instant"
    );

    r.clock.advance(61 * SECOND);
    let fired = await_fired(&mut w.alice, "nudge", &["nudge-1"]).await;
    let task = &fired[0];
    assert_eq!(task.title, "nudge the reviewer");
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(
        task.author,
        w.alice.pk_hex(),
        "the task is authored by the timer's owner"
    );
    assert_eq!(task.origin.as_deref(), Some("timer:nudge"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_timer_fires_at_its_instant_and_not_before() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let due = r.clock.now_ms() + MINUTE;
    create(
        &mut w.alice,
        Some("nudge"),
        TimerSpec::At { at_ms: due },
        "nudge",
    )
    .await;
    sentinel(&mut w.alice, 1, due - SECOND).await;

    r.clock.advance(MINUTE - SECOND);
    settle(&mut w.alice, 1).await;
    await_fired(&mut w.alice, "nudge", &[] as &[&str]).await;
    assert_eq!(
        timer_named(&listed(&mut w.alice).await, "nudge").next_fire_ms,
        Some(due),
        "still scheduled for its instant one second later"
    );

    r.clock.advance(SECOND);
    await_fired(&mut w.alice, "nudge", &["nudge-1"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_timer_disables_itself_after_its_one_fire_and_never_fires_again() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let due = r.clock.now_ms() + MINUTE;
    create(
        &mut w.alice,
        Some("nudge"),
        TimerSpec::At { at_ms: due },
        "nudge",
    )
    .await;
    sentinel(&mut w.alice, 1, r.clock.now_ms() + 61 * SECOND + HOUR).await;

    r.clock.advance(61 * SECOND);
    await_fired(&mut w.alice, "nudge", &["nudge-1"]).await;
    let timer = timer_named(&listed(&mut w.alice).await, "nudge");
    assert!(!timer.enabled, "an at timer is disabled once it has fired");
    assert_eq!(timer.next_fire_ms, None);
    assert_eq!(timer.last_fire_ms, Some(due));
    assert_eq!(timer.fired, 1);

    r.clock.advance(HOUR);
    settle(&mut w.alice, 1).await;
    await_fired(&mut w.alice, "nudge", &["nudge-1"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_every_timer_fires_once_per_period() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let timer = create(
        &mut w.alice,
        Some("pulse"),
        TimerSpec::Every {
            every_s: 300,
            start_ms: None,
        },
        "pulse",
    )
    .await;
    assert_eq!(
        timer.next_fire_ms,
        Some(timer.created_ms + FIVE_MINUTES),
        "the first fire is one period after creation"
    );

    r.clock.advance(301 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1"]).await;
    r.clock.advance(300 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1", "pulse-2"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_advance_fires_one_catch_up_task_not_one_per_missed_period() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    catch_up(&r, &mut w).await;

    await_fired(&mut w.alice, "pulse", &["pulse-1", "pulse-2", "pulse-3"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_catch_up_fire_the_next_fire_is_the_first_grid_point_after_now() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let timer = catch_up(&r, &mut w).await;

    let now = r.clock.now_ms();
    let next = timer_named(&listed(&mut w.alice).await, "pulse")
        .next_fire_ms
        .expect("an every timer stays scheduled");
    assert!(
        next > now,
        "the next fire {next} is strictly after now {now}"
    );
    let grid_origin = timer.created_ms + FIVE_MINUTES;
    assert_eq!(
        (next - grid_origin).rem_euclid(FIVE_MINUTES),
        0,
        "the next fire is on the every-300 grid"
    );
    assert!(
        next - now <= FIVE_MINUTES,
        "the next fire is the first grid point after now"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cron_timer_fires_once_per_matching_minute_across_an_hour() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let timer = create(
        &mut w.alice,
        Some("cronjob"),
        TimerSpec::Cron {
            expr: "*/5 * * * *".to_owned(),
        },
        "cron job",
    )
    .await;
    let first = timer.next_fire_ms.expect("a cron timer has a first fire");
    assert_eq!(
        first.rem_euclid(FIVE_MINUTES),
        0,
        "the first fire is a five-minute boundary"
    );
    // One minute after the twelfth boundary: no boundary is due there, so the sentinel shows the loop is quiet.
    sentinel(&mut w.alice, 1, first + 55 * MINUTE + MINUTE).await;

    // Step to the first boundary, then one boundary per step. Each step is awaited before the next, so no two
    // boundaries can collapse into one catch-up fire.
    r.clock.advance(first - r.clock.now_ms());
    let mut expected: Vec<String> = Vec::new();
    for k in 1..=12 {
        if k > 1 {
            r.clock.advance(FIVE_MINUTES);
        }
        expected.push(format!("cronjob-{k}"));
        await_fired(&mut w.alice, "cronjob", &expected).await;
    }
    r.clock.advance(MINUTE);
    settle(&mut w.alice, 1).await;
    await_fired(&mut w.alice, "cronjob", &expected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_quota_refuses_a_create_past_timer_quota_enabled_timers() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_settings(&mut w.owner, json!({"timer_quota": "2"})).await;
    let hourly = TimerSpec::Every {
        every_s: 3600,
        start_ms: None,
    };
    create(&mut w.alice, Some("first"), hourly.clone(), "first").await;
    create(&mut w.alice, Some("second"), hourly.clone(), "second").await;

    let refused = try_create(&mut w.alice, Some("third"), hourly, "third")
        .await
        .expect_err("the third enabled timer is refused");
    assert_eq!(refused.code, ErrorCode::Invalid);
    assert!(
        refused.message.starts_with("timer quota reached"),
        "refusal names the quota: {}",
        refused.message
    );
    let ids: Vec<String> = listed(&mut w.alice)
        .await
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, ["first", "second"], "the refused timer is not stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_timer_quota_is_counted_per_identity() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_settings(&mut w.owner, json!({"timer_quota": "1"})).await;
    let hourly = TimerSpec::Every {
        every_s: 3600,
        start_ms: None,
    };
    create(&mut w.alice, Some("mine"), hourly.clone(), "mine").await;
    try_create(&mut w.alice, Some("second"), hourly.clone(), "second")
        .await
        .expect_err("alice is at her quota");

    let his = create(&mut w.bob, Some("his"), hourly, "his").await;
    assert_eq!(
        his.owner,
        w.bob.pk_hex(),
        "bob's timer is accepted while alice is at her quota"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_timer_stops_its_fires() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    create(
        &mut w.alice,
        Some("pulse"),
        TimerSpec::Every {
            every_s: 60,
            start_ms: None,
        },
        "pulse",
    )
    .await;
    r.clock.advance(61 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1"]).await;

    try_delete(&mut w.alice, "pulse")
        .await
        .expect("the owner deletes its timer");
    assert!(
        listed(&mut w.alice).await.iter().all(|t| t.id != "pulse"),
        "the deleted timer is no longer listed"
    );

    sentinel(&mut w.alice, 1, r.clock.now_ms() + 10 * MINUTE).await;
    r.clock.advance(10 * MINUTE);
    settle(&mut w.alice, 1).await;
    await_fired(&mut w.alice, "pulse", &["pulse-1"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_cannot_delete_another_workers_timer() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    create(
        &mut w.alice,
        Some("pulse"),
        TimerSpec::Every {
            every_s: 60,
            start_ms: None,
        },
        "pulse",
    )
    .await;

    let refused = try_delete(&mut w.bob, "pulse")
        .await
        .expect_err("bob may not delete alice's timer");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    assert!(
        timer_named(&listed(&mut w.alice).await, "pulse").enabled,
        "the refused delete leaves the timer enabled"
    );

    r.clock.advance(61 * SECOND);
    await_fired(&mut w.alice, "pulse", &["pulse-1"]).await;
}
