//! Admission scenarios from ref-admission section 15, encoded through the public `admit` and `quiet` API: table A
//! (admission, S01-S11 here and S12-S22 in `gates`), B (queue order, `queue_order`), C (backfill, `backfill`), D (quiet
//! windows, `drain`) and E (load wait, `load_wait`). The submodules live under `admit_scenarios/` to keep each file
//! under the repo's 500-line budget.
//!
//! Fixture F8 unless a test says otherwise: 8 CPUs, budget 8, no capacity, no memory budget, idle 90%, memory normal,
//! no load gate, and default settings (quiet drain 60 s, quiet max 180 s, small weight 1, backfill wait 300 s).
//! Reason strings are exact. Reference rows whose text was hand-derived were checked against work.py with a fake box
//! (`python3 -I`), and the comments say where the two differ.

#[path = "admit_scenarios/backfill.rs"]
mod backfill;
#[path = "admit_scenarios/drain.rs"]
mod drain;
#[path = "admit_scenarios/gates.rs"]
mod gates;
#[path = "admit_scenarios/load_wait.rs"]
mod load_wait;
#[path = "admit_scenarios/queue_order.rs"]
mod queue_order;

use agora_sched::admit::{
    self, Budgets, Cause, Hold, MachineSnapshot, MemLevel, RunningJob, Signals, WaitingJob,
};
use agora_sched::quiet::{self, AdmitInput};
use agora_sched::settings::{MemBudget, MemLimit, WorkSettings};

/// The clock every scenario runs at, in epoch seconds. Integral, so every "age" is exact in f64.
const NOW: f64 = 10_000.0;

fn calm() -> Signals {
    Signals {
        idle_pct: Some(90.0),
        memory_level: Some(MemLevel::Normal),
        load1: None,
        load_gate: false,
        memory_used_kib: None,
    }
}

/// Fixture F8: 8 CPUs, no memory total (so no memory budget), and calm signals.
fn machine() -> MachineSnapshot {
    MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: None,
        disk_free_kib: None,
        signals: Some(calm()),
    }
}

/// F8 with the signals replaced.
fn machine_with(signals: Signals) -> MachineSnapshot {
    MachineSnapshot {
        signals: Some(signals),
        ..machine()
    }
}

fn idle_machine() -> MachineSnapshot {
    machine_with(Signals {
        idle_pct: Some(5.0),
        ..calm()
    })
}

fn settings() -> WorkSettings {
    WorkSettings::default()
}

/// Default settings with the CPU budget set to `cpus`.
fn budget(cpus: f64) -> WorkSettings {
    WorkSettings {
        cpu_budget: Some(cpus),
        ..settings()
    }
}

/// A job running since NOW, declared at `weight`, with `pred` predicted cores.
fn run(id: &str, weight: f64, pred: Option<f64>) -> RunningJob {
    RunningJob {
        id: id.to_owned(),
        weight,
        pred_cores: pred,
        pred_rss_p90_kib: None,
        pred_duration_p90_s: None,
        rss_kib: None,
        started_s: Some(NOW),
        quiet_until_s: None,
    }
}

/// A waiting job of kind "work", declared at `weight` with `pred` predicted cores, requested `age` seconds before NOW.
fn wait(id: &str, weight: f64, pred: Option<f64>, age: f64) -> WaitingJob {
    WaitingJob {
        id: id.to_owned(),
        kind: "work".to_owned(),
        seq: 0,
        requested_s: NOW - age,
        weight,
        pred_cores: pred,
        pred_duration_s: None,
        pred_duration_p90_s: None,
        pred_rss_p90_kib: None,
        machine_held_since_s: None,
    }
}

/// A waiting job with no declared weight, a kind, and a predicted duration, for queue order.
fn queued(id: &str, requested_s: f64, dur: Option<f64>) -> WaitingJob {
    WaitingJob {
        id: id.to_owned(),
        kind: "work".to_owned(),
        seq: 0,
        requested_s,
        weight: 1.0,
        pred_cores: None,
        pred_duration_s: dur,
        pred_duration_p90_s: None,
        pred_rss_p90_kib: None,
        machine_held_since_s: None,
    }
}

fn hold(cause: Cause, reason: &str) -> Option<Hold> {
    Some(Hold {
        cause,
        reason: reason.to_owned(),
    })
}

/// The admission verdict for `job` beside `running` at `now`.
fn admit_at(
    job: &WaitingJob,
    running: &[RunningJob],
    s: &WorkSettings,
    m: &MachineSnapshot,
    now: f64,
) -> Option<Hold> {
    let b = admit::budgets(s, m);
    admit::admission(job, running, s, &b, m.signals.as_ref(), now)
}

fn admit_now(
    job: &WaitingJob,
    running: &[RunningJob],
    s: &WorkSettings,
    m: &MachineSnapshot,
) -> Option<Hold> {
    admit_at(job, running, s, m, NOW)
}

/// The backfill verdict for `job` behind `ahead` (ahead[0] is the head) at NOW.
fn back(
    job: &WaitingJob,
    ahead: &[&WaitingJob],
    running: &[RunningJob],
    s: &WorkSettings,
    m: &MachineSnapshot,
) -> Option<Hold> {
    let b: Budgets = admit::budgets(s, m);
    admit::backfill(job, ahead, running, s, &b, m.signals.as_ref(), NOW)
}

/// Four jobs declared at 2 and predicted at one core each: the F8 "4 x (w2, p1.0)" running set.
fn four_one_core() -> Vec<RunningJob> {
    (0..4)
        .map(|i| run(&format!("r{i}"), 2.0, Some(1.0)))
        .collect()
}

/// The ids of `jobs` in queue order under `aging`.
fn order(jobs: &[WaitingJob], aging: f64) -> Vec<String> {
    let mut sorted = jobs.to_vec();
    sorted.sort_by(|x, y| admit::waiting_order_cmp(x, y, aging));
    sorted.into_iter().map(|job| job.id).collect()
}

/// The admission input at NOW for the given machine, settings, running set and waiting line.
fn input(
    s: WorkSettings,
    m: MachineSnapshot,
    running: Vec<RunningJob>,
    waiting: Vec<WaitingJob>,
) -> AdmitInput {
    AdmitInput {
        now_s: NOW,
        settings: s,
        machine: m,
        running,
        waiting,
        window_closes: Vec::new(),
        paused: None,
        manual_quiet: Vec::new(),
    }
}

/// Supervisor.blocker for the waiting job `id` of `inp`, against the running set and the line that `inp` holds.
fn blocked(inp: &AdmitInput, id: &str) -> Option<Hold> {
    let job = inp
        .waiting
        .iter()
        .find(|w| w.id == id)
        .expect("the job is waiting");
    quiet::blocker(job, inp, &inp.running, &inp.waiting)
}

// ---- A. Admission (S01-S22) ----

#[test]
fn s01_four_one_core_jobs_leave_room() {
    let cand = wait("c", 2.0, Some(1.0), 10.0);
    assert_eq!(
        admit_now(&cand, &four_one_core(), &settings(), &machine()),
        None
    );
}

#[test]
fn s02_cores_hold_names_the_predicted_cores_taken() {
    let mut running = four_one_core();
    running.push(run("big", 4.0, Some(3.4)));
    assert_eq!(
        admit_now(
            &wait("c", 2.0, None, 10.0),
            &running,
            &settings(),
            &machine()
        ),
        hold(
            Cause::Cores,
            "predicted 7.4 of 8 cores are taken and this job needs 2"
        ),
    );
}

#[test]
fn s03_a_job_larger_than_the_budget_runs_alone() {
    let cand = wait("c", 6.0, Some(9.0), 10.0);
    assert_eq!(admit_now(&cand, &[], &settings(), &machine()), None);
}

#[test]
fn s04_tiny_job_passes_the_budget_only_up_to_half_a_core() {
    let busy = [run("r", 7.9, None)];
    let tiny = wait("t", 1.0, Some(0.2), 10.0);
    assert_eq!(admit_now(&tiny, &busy, &settings(), &machine()), None);
    let over = wait("t", 1.0, Some(0.6), 10.0);
    assert_eq!(
        admit_now(&over, &busy, &settings(), &machine()),
        hold(
            Cause::Cores,
            "predicted 7.9 of 8 cores are taken and this job needs 0.6"
        ),
    );
}

#[test]
fn s05_the_budget_tolerates_one_nanocore() {
    let busy = [run("r", 7.0, None)];
    assert_eq!(
        admit_now(&wait("c", 1.0, None, 10.0), &busy, &settings(), &machine()),
        None
    );
    let cand = wait("c", 2.0, Some(1.000_000_001_1), 10.0);
    assert_eq!(
        admit_now(&cand, &busy, &settings(), &machine()),
        hold(
            Cause::Cores,
            "predicted 7 of 8 cores are taken and this job needs 1"
        ),
    );
}

#[test]
fn s06_the_tiny_bound_is_half_a_core_plus_an_epsilon() {
    let busy = [run("r", 7.9, None)];
    let at_bound = wait("t", 1.0, Some(0.5), 10.0);
    assert_eq!(admit_now(&at_bound, &busy, &settings(), &machine()), None);
    let past_bound = wait("t", 1.0, Some(0.500_000_001_1), 10.0);
    assert_eq!(
        admit_now(&past_bound, &busy, &settings(), &machine()),
        hold(
            Cause::Cores,
            "predicted 7.9 of 8 cores are taken and this job needs 0.5"
        ),
    );
}

#[test]
fn s07_an_exclusive_job_gets_no_tiny_bypass() {
    let busy = [run("r", 8.0, Some(7.9))];
    let cand = wait("c", 8.0, Some(0.2), 10.0);
    assert_eq!(
        admit_now(&cand, &busy, &settings(), &machine()),
        hold(
            Cause::Cores,
            "predicted 7.9 of 8 cores are taken and this job needs 0.2"
        ),
    );
}

#[test]
fn s08_capacity_caps_the_declared_weights() {
    let capped = WorkSettings {
        capacity: Some(4.0),
        ..settings()
    };
    let busy = [run("r", 3.0, Some(0.5))];
    let cand = wait("c", 2.0, Some(0.5), 10.0);
    assert_eq!(
        admit_now(&cand, &busy, &capped, &machine()),
        hold(
            Cause::Capacity,
            "declared weights of 3 of the capacity of 4 cores are taken and this job declares 2"
        ),
    );
    assert_eq!(admit_now(&cand, &busy, &settings(), &machine()), None);
}

#[test]
fn s09_a_tiny_job_is_still_held_by_capacity() {
    let capped = WorkSettings {
        capacity: Some(4.0),
        ..settings()
    };
    let busy = [run("r", 3.0, Some(0.5))];
    let tiny = wait("c", 2.0, Some(0.3), 10.0);
    assert_eq!(
        admit_now(&tiny, &busy, &capped, &machine()),
        hold(
            Cause::Capacity,
            "declared weights of 3 of the capacity of 4 cores are taken and this job declares 2"
        ),
    );
}

#[test]
fn s10_capacity_never_holds_an_empty_machine() {
    let capped = WorkSettings {
        capacity: Some(1.0),
        ..settings()
    };
    assert_eq!(
        admit_now(&wait("c", 8.0, None, 10.0), &[], &capped, &machine()),
        None
    );
}

#[test]
fn s11_memory_pressure_at_the_limit_holds_jobs() {
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    let warn = Signals {
        memory_level: Some(MemLevel::Warning),
        ..calm()
    };
    let critical = Signals {
        memory_level: Some(MemLevel::Critical),
        ..calm()
    };
    assert_eq!(
        admit_now(&cand, &[], &settings(), &machine_with(warn.clone())),
        hold(
            Cause::Memory,
            "memory pressure is warning; jobs start below warning"
        ),
    );
    let at_critical = WorkSettings {
        memory_limit: MemLimit::Critical,
        ..settings()
    };
    assert_eq!(
        admit_now(&cand, &[], &at_critical, &machine_with(warn)),
        None
    );
    assert_eq!(
        admit_now(&cand, &[], &at_critical, &machine_with(critical.clone())),
        hold(
            Cause::Memory,
            "memory pressure is critical; jobs start below critical"
        ),
    );
    let off = WorkSettings {
        memory_limit: MemLimit::Off,
        ..settings()
    };
    assert_eq!(admit_now(&cand, &[], &off, &machine_with(critical)), None);
}
