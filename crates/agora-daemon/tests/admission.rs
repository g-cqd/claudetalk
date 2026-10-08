//! Admission through the daemon (ref-admission §3-§9): the memory, pressure, idle and load gates, the CPU budget,
//! backfill past a blocked head, the exclusive drain and quiet window, manual quiet windows, paused admission, and
//! the load-wait failure. Jobs are real child processes; the daemon's time is a `FakeClock` and the machine a
//! `FakeMachine`, so no test depends on host load or on wall time.
//!
//! Signed client ops carry the wall clock and the daemon checks them against the fake clock (±5 min). So each test
//! signs its ops before it advances the clock far, and it advances no more than the scenario needs.

mod common;

use agora_daemon::clock::{Clock, FakeClock};
use agora_daemon::now_ms;
use agora_proto::{Reply, Req};
use agora_run::machine::{FakeMachine, Machine};
use agora_sched::admit::{MemLevel, Signals};
use agora_sched::model::{LeaseRecord, TaskRecord, TaskState, WaitSpec};
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Bound on every wait, so a daemon bug fails the test instead of hanging it.
const WAIT_MS: u64 = 30_000;
const WAIT: Duration = Duration::from_millis(WAIT_MS);
/// One GiB in KiB, the unit of the machine's memory figures.
const GIB_KIB: u64 = 1024 * 1024;
/// The fake machine's memory budget under the default setting: 16 GiB total minus 2 GiB.
const DEFAULT_MEMORY_BUDGET_GIB: &str = "14.0 GiB";
/// The machine's used memory with the default probe (4 GiB), kept below the budget.
const LIGHT_MEMORY_GIB: u64 = 4;
/// Used memory above the default budget of 14 GiB.
const HEAVY_MEMORY_GIB: u64 = 15;

/// A daemon on a fresh home, with a fake clock started at the wall time and a fake machine.
struct Rig {
    d: TestDaemon,
    clock: Arc<FakeClock>,
    machine: Arc<FakeMachine>,
}

async fn rig() -> Rig {
    let clock = FakeClock::new(now_ms());
    let machine = FakeMachine::new();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let machine_dyn: Arc<dyn Machine> = machine.clone();
    let d = start_injected(Some(clock_dyn), Some(machine_dyn), |_| {}).await;
    Rig { d, clock, machine }
}

/// Edits the fake machine's signals, creating them when absent.
fn signals(machine: &FakeMachine, f: impl FnOnce(&mut Signals)) {
    machine.update(|m| f(m.signals.get_or_insert_with(Signals::default)));
}

fn set_used_memory(machine: &FakeMachine, gib: u64) {
    signals(machine, |s| s.memory_used_kib = Some(gib * GIB_KIB));
}

/// Submits an inline run of `command` at `weight` and returns the job as the daemon recorded it.
async fn run_inline(c: &mut TestClient, command: &[&str], weight: f64) -> TaskRecord {
    let arg = json!({"add": {"command": command, "cwd": "/", "weight": weight}}).to_string();
    let op = c.op("work.run", "", &arg);
    let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    match c.ok(Req::WorkRun { op, env }).await {
        Reply::Task { task } => task,
        other => panic!("work.run: expected Task, got {other:?}"),
    }
}

async fn show(c: &mut TestClient, id: &str) -> TaskRecord {
    match c
        .ok(Req::WorkShow {
            task: id.to_owned(),
        })
        .await
    {
        Reply::Task { task } => task,
        other => panic!("work.show {id}: expected Task, got {other:?}"),
    }
}

/// Polls the job until `pred` holds, bounded by `WAIT`. A hold note is written by the admission pass without a bus
/// event, so there is nothing to await: each read is a round trip to the daemon, which yields between reads.
async fn wait_until(
    c: &mut TestClient,
    id: &str,
    what: &str,
    pred: impl Fn(&TaskRecord) -> bool,
) -> TaskRecord {
    let mut last = None;
    let found = timeout(WAIT, async {
        loop {
            let task = show(c, id).await;
            if pred(&task) {
                return task;
            }
            last = Some((task.state, task.note.clone(), task.started_ms));
            tokio::task::yield_now().await;
        }
    })
    .await;
    found.unwrap_or_else(|_| {
        panic!("job {id} did not reach {what} within {WAIT:?}; last seen {last:?}")
    })
}

/// Awaits the job's final state through the daemon's event-driven wait, then reads it.
async fn wait_final(c: &mut TestClient, id: &str) -> TaskRecord {
    let spec = WaitSpec::Tasks {
        ids: vec![id.to_owned()],
        any: false,
    };
    let reply = timeout(
        WAIT,
        c.call(Req::WorkWait {
            spec,
            timeout_ms: WAIT_MS,
            cursor: None,
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("job {id} did not end within {WAIT:?}"))
    .unwrap_or_else(|e| panic!("work.wait {id} failed: {e}"));
    match reply {
        Reply::Wait { result } => assert!(!result.timed_out, "work.wait {id} timed out"),
        other => panic!("work.wait: expected Wait, got {other:?}"),
    }
    show(c, id).await
}

/// Any work op publishes `WorkChanged`, which wakes the admission loop; a queued task with no command changes
/// nothing else.
async fn kick(c: &mut TestClient) {
    let op = c.op("work.add", "", &json!({"title": "kick"}).to_string());
    match c.ok(Req::WorkAdd { op }).await {
        Reply::Task { .. } => {}
        other => panic!("work.add: expected Task, got {other:?}"),
    }
}

async fn set_settings(c: &mut TestClient, patch: Value) {
    let op = c.op("work.settings", "", &json!({"patch": patch}).to_string());
    match c.ok(Req::WorkSettings { op: Some(op) }).await {
        Reply::WorkSettings { .. } => {}
        other => panic!("work.settings: expected WorkSettings, got {other:?}"),
    }
}

async fn open_quiet(c: &mut TestClient, expected_end_ms: i64, note: &str) -> LeaseRecord {
    let arg = json!({"expected_end_ms": expected_end_ms, "note": note}).to_string();
    let op = c.op("quiet.open", "", &arg);
    match c.ok(Req::QuietOpen { op }).await {
        Reply::Lease { lease } => lease,
        other => panic!("quiet.open: expected Lease, got {other:?}"),
    }
}

async fn close_quiet(c: &mut TestClient, lease: &str) {
    let op = c.op("quiet.close", lease, "{}");
    match c.ok(Req::QuietClose { op }).await {
        Reply::Unit => {}
        other => panic!("quiet.close: expected Unit, got {other:?}"),
    }
}

/// Stops a long-running job the test no longer needs, so no child outlives the test.
async fn cancel(c: &mut TestClient, id: &str) {
    let op = c.op("work.cancel", id, &json!({"note": "test over"}).to_string());
    match c.ok(Req::WorkCancel { op }).await {
        Reply::Task { .. } => {}
        other => panic!("work.cancel {id}: expected Task, got {other:?}"),
    }
}

/// Starts a heavy job (weight 4, no history, so it counts 4 cores) and an exclusive job (weight 8 on 8 CPUs), then
/// advances the fake clock past the 60 s drain. Returns both jobs; the exclusive one is running with its window open.
async fn exclusive_job_in_its_window(r: &Rig, w: &mut World) -> (TaskRecord, TaskRecord) {
    let heavy = run_inline(&mut w.alice, &["sleep", "60"], 4.0).await;
    wait_until(&mut w.alice, &heavy.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let exclusive = run_inline(&mut w.alice, &["sleep", "60"], 8.0).await;
    let drain = format!(
        "it drains: job {}, predicted above 1 core, still runs; it starts in 1 min 0 s at the latest",
        heavy.id
    );
    wait_until(&mut w.alice, &exclusive.id, "the drain hold", |t| {
        t.note == drain
    })
    .await;
    r.clock.advance(61_000);
    let running = wait_until(&mut w.alice, &exclusive.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    (heavy, running)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn used_memory_above_the_budget_holds_a_job_until_a_kick_after_the_probe_drops() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_used_memory(&r.machine, HEAVY_MEMORY_GIB);
    let job = run_inline(&mut w.alice, &["true"], 1.0).await;

    let held = wait_until(&mut w.alice, &job.id, "its memory hold", |t| {
        t.note
            == format!(
                "used memory is 15.0 GiB, above the memory budget of {DEFAULT_MEMORY_BUDGET_GIB}"
            )
    })
    .await;
    assert_eq!(held.state, TaskState::Claimed);
    assert!(held.requested_ms.is_some(), "a held job has asked to run");
    assert_eq!(held.started_ms, None);

    set_used_memory(&r.machine, LIGHT_MEMORY_GIB);
    kick(&mut w.alice).await;
    let done = wait_final(&mut w.alice, &job.id).await;
    assert_eq!(done.state, TaskState::Done);
    assert_eq!(done.status, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_memory_pressure_warning_holds_a_job_until_the_level_clears() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    signals(&r.machine, |s| s.memory_level = Some(MemLevel::Warning));
    let job = run_inline(&mut w.alice, &["true"], 1.0).await;

    wait_until(&mut w.alice, &job.id, "the pressure hold", |t| {
        t.note == "memory pressure is warning; jobs start below warning"
    })
    .await;

    signals(&r.machine, |s| s.memory_level = Some(MemLevel::Normal));
    kick(&mut w.alice).await;
    let done = wait_final(&mut w.alice, &job.id).await;
    assert_eq!(done.state, TaskState::Done);
    assert_eq!(done.status, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_would_exceed_the_cpu_budget_starts_when_the_running_job_ends() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_settings(&mut w.owner, json!({"cpu_budget": "1"})).await;
    let first = run_inline(&mut w.alice, &["sleep", "5"], 1.0).await;
    wait_until(&mut w.alice, &first.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;

    let second = run_inline(&mut w.alice, &["true"], 1.0).await;
    wait_until(&mut w.alice, &second.id, "the cores hold", |t| {
        t.note == "predicted 1 of 1 cores are taken and this job needs 1"
    })
    .await;

    let first_done = wait_final(&mut w.alice, &first.id).await;
    assert_eq!(first_done.state, TaskState::Done);
    let second_done = wait_final(&mut w.alice, &second.id).await;
    assert_eq!(second_done.state, TaskState::Done);
    assert!(second_done.started_ms.is_some(), "the second job ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_job_starts_past_a_head_that_waits_for_cores() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_settings(&mut w.owner, json!({"cpu_budget": "3"})).await;
    let busy = run_inline(&mut w.alice, &["sleep", "60"], 1.0).await;
    wait_until(&mut w.alice, &busy.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let head = run_inline(&mut w.alice, &["true"], 3.0).await;
    wait_until(&mut w.alice, &head.id, "the head's cores hold", |t| {
        t.note == "predicted 1 of 3 cores are taken and this job needs 3"
    })
    .await;

    let small = run_inline(&mut w.alice, &["true"], 1.0).await;
    let small_done = wait_final(&mut w.alice, &small.id).await;
    assert_eq!(small_done.state, TaskState::Done);
    assert_eq!(small_done.status, Some(0));

    let head_now = show(&mut w.alice, &head.id).await;
    assert_eq!(head_now.state, TaskState::Claimed, "the head still waits");
    assert_eq!(head_now.started_ms, None);

    cancel(&mut w.alice, &busy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_job_is_held_once_the_head_has_waited_past_the_backfill_limit() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_settings(&mut w.owner, json!({"cpu_budget": "3"})).await;
    let busy = run_inline(&mut w.alice, &["sleep", "60"], 1.0).await;
    wait_until(&mut w.alice, &busy.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let head = run_inline(&mut w.alice, &["true"], 3.0).await;
    wait_until(&mut w.alice, &head.id, "the head's cores hold", |t| {
        t.note == "predicted 1 of 3 cores are taken and this job needs 3"
    })
    .await;

    // The backfill limit is 300 s, and the head asked at the fake clock's current time.
    r.clock.advance(300_000);
    let late = run_inline(&mut w.alice, &["true"], 1.0).await;
    let limit = format!(
        "job {} has waited 5 min 0 s, past the backfill limit of 5 min 0 s, so no job starts before it",
        head.id
    );
    wait_until(&mut w.alice, &late.id, "the backfill hold", |t| {
        t.note == limit
    })
    .await;
    let late_now = show(&mut w.alice, &late.id).await;
    assert_eq!(late_now.state, TaskState::Claimed);

    cancel(&mut w.alice, &busy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exclusive_job_waits_out_the_drain_while_a_heavy_job_runs_and_then_starts() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let heavy = run_inline(&mut w.alice, &["sleep", "60"], 4.0).await;
    wait_until(&mut w.alice, &heavy.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let exclusive = run_inline(&mut w.alice, &["sleep", "60"], 8.0).await;
    let drain = format!(
        "it drains: job {}, predicted above 1 core, still runs; it starts in 1 min 0 s at the latest",
        heavy.id
    );
    wait_until(&mut w.alice, &exclusive.id, "the drain hold", |t| {
        t.note == drain
    })
    .await;

    r.clock.advance(61_000);
    let started = wait_until(&mut w.alice, &exclusive.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    assert_eq!(started.started_ms, Some(r.clock.now_ms()));
    assert_eq!(started.quiet_until_ms, Some(r.clock.now_ms() + 180_000));

    cancel(&mut w.alice, &exclusive.id).await;
    cancel(&mut w.alice, &heavy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heavy_job_is_held_for_the_rest_of_a_running_exclusive_job_s_quiet_window() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let (heavy, exclusive) = exclusive_job_in_its_window(&r, &mut w).await;

    let late_heavy = run_inline(&mut w.alice, &["sleep", "60"], 4.0).await;
    let window = format!(
        "job {} runs in its quiet window for 3 min 0 s more; no job predicted above 1 core starts in it, and this job needs 4",
        exclusive.id
    );
    wait_until(&mut w.alice, &late_heavy.id, "the window hold", |t| {
        t.note == window
    })
    .await;

    cancel(&mut w.alice, &late_heavy.id).await;
    cancel(&mut w.alice, &exclusive.id).await;
    cancel(&mut w.alice, &heavy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_manual_quiet_window_holds_a_heavy_job_and_names_its_lease() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let end = r.clock.now_ms() + 600_000;
    let lease = open_quiet(&mut w.owner, end, "deploy freeze").await;

    let heavy = run_inline(&mut w.alice, &["sleep", "60"], 4.0).await;
    let hold = format!(
        "quiet window {} opened by {} is open for 10 min 0 s more: deploy freeze; no job predicted above 1 core starts in it, and this job needs 4",
        lease.id, lease.holder_name
    );
    wait_until(&mut w.alice, &heavy.id, "the manual window hold", |t| {
        t.note == hold
    })
    .await;
    let held = show(&mut w.alice, &heavy.id).await;
    assert_eq!(held.state, TaskState::Claimed);

    cancel(&mut w.alice, &heavy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_manual_quiet_window_starts_the_heavy_job_it_held() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    let end = r.clock.now_ms() + 600_000;
    let lease = open_quiet(&mut w.owner, end, "deploy freeze").await;
    let heavy = run_inline(&mut w.alice, &["sleep", "60"], 4.0).await;
    wait_until(&mut w.alice, &heavy.id, "the manual window hold", |t| {
        t.note.starts_with("quiet window ")
    })
    .await;

    close_quiet(&mut w.owner, &lease.id).await;
    let started = wait_until(&mut w.alice, &heavy.id, "running", |t| {
        t.state == TaskState::Running
    })
    .await;
    assert_eq!(started.started_ms, Some(r.clock.now_ms()));

    cancel(&mut w.alice, &heavy.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disk_floor_breach_pauses_admission_and_holds_the_waiting_job() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    set_used_memory(&r.machine, HEAVY_MEMORY_GIB);
    let job = run_inline(&mut w.alice, &["true"], 1.0).await;
    wait_until(&mut w.alice, &job.id, "its memory hold", |t| {
        t.note.starts_with("used memory is")
    })
    .await;

    // Free disk falls to 1 GiB, below the 5 GiB floor. The reconciler's next pass, 30 s on, pauses admission.
    r.machine.update(|m| m.disk_free_kib = Some(GIB_KIB));
    r.clock.advance(30_000);
    let paused = wait_until(&mut w.alice, &job.id, "the paused hold", |t| {
        t.note.starts_with("admission is paused: ")
    })
    .await;
    assert_eq!(
        paused.note,
        "admission is paused: disk free 1.0 GiB below the floor of 5.0 GiB"
    );
    assert_eq!(paused.state, TaskState::Claimed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_held_by_idle_cpu_past_the_load_wait_fails_with_the_pressure_note() {
    let r = rig().await;
    let mut w = world(&r.d).await;
    signals(&r.machine, |s| s.idle_pct = Some(5.0));
    let job = run_inline(&mut w.alice, &["true"], 1.0).await;
    wait_until(&mut w.alice, &job.id, "the idle hold", |t| {
        t.note == "the CPU is 5% idle; jobs start at 10% idle or more"
    })
    .await;

    // The default load wait is 1800 s. Only reads follow the advance, so the skew check does not apply.
    r.clock.advance(1_800_000);
    let failed = wait_final(&mut w.alice, &job.id).await;
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.status, None);
    assert_eq!(failed.started_ms, None, "the job never started");
    assert_eq!(failed.ended_ms, Some(r.clock.now_ms()));
    // The spec fixes the reason, the span and the flag; the port names the command `agora work settings`.
    assert!(
        failed.note.starts_with(
            "the CPU is 5% idle; jobs start at 10% idle or more for 30 min 0 s, the longest a job waits on the machine's pressure ("
        ),
        "note: {}",
        failed.note
    );
    assert!(
        failed.note.ends_with("--load-wait)"),
        "note: {}",
        failed.note
    );
}
