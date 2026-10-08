//! Load-wait failure, ref-admission section 9 (table E), decided pass by pass as the daemon runs it.

use super::*;
use agora_sched::quiet::{Decision, Fail};

/// One admission pass at `now` over `running` and `waiting`, with the given machine and default settings.
fn pass(
    now: f64,
    m: MachineSnapshot,
    running: Vec<RunningJob>,
    waiting: Vec<WaitingJob>,
) -> Decision {
    let inp = AdmitInput {
        now_s: now,
        ..input(settings(), m, running, waiting)
    };
    quiet::decide(&inp)
}

/// The cause the pass held `id` for, if it held it.
fn held_cause(d: &Decision, id: &str) -> Option<Cause> {
    d.held.iter().find(|h| h.id == id).map(|h| h.cause)
}

/// Feeds the machine-hold clock that the pass reports for `job` back into it, as the daemon keeps it between polls.
fn carry(job: &mut WaitingJob, d: &Decision) {
    job.machine_held_since_s = d
        .machine_held_since
        .iter()
        .find(|(id, _)| id == &job.id)
        .and_then(|(_, since)| *since);
}

fn load_machine() -> MachineSnapshot {
    machine_with(Signals {
        load1: Some(92.8),
        load_gate: true,
        ..calm()
    })
}

fn memory_machine() -> MachineSnapshot {
    machine_with(Signals {
        memory_level: Some(MemLevel::Warning),
        ..calm()
    })
}

/// The load-wait note for an idle hold of `span` (work.py: "work settings --load-wait"; the port says "agora work
/// settings", as the log-limit note does in SPEC section 4).
fn idle_fail_note(span: &str) -> String {
    format!(
        "the CPU is 5% idle; jobs start at 10% idle or more for {span}, the longest a job waits on the machine's \
         pressure (agora work settings --load-wait)"
    )
}

// ---- E. Load wait (W1-W4), decided pass by pass as the daemon runs it ----

#[test]
fn w1_a_machine_hold_fails_once_it_has_lasted_the_load_wait() {
    let t0 = NOW;
    let mut job = wait("X", 1.0, None, 0.0);
    let d = pass(t0, idle_machine(), vec![], vec![job.clone()]);
    assert_eq!(held_cause(&d, "X"), Some(Cause::Idle));
    carry(&mut job, &d);
    let d = pass(t0 + 1799.0, idle_machine(), vec![], vec![job.clone()]);
    assert!(d.fail.is_empty());
    assert_eq!(held_cause(&d, "X"), Some(Cause::Idle));
    carry(&mut job, &d);
    let d = pass(t0 + 1800.0, idle_machine(), vec![], vec![job]);
    assert_eq!(
        d.fail,
        vec![Fail {
            id: "X".to_owned(),
            note: idle_fail_note("30 min 0 s"),
        }],
    );
    assert!(d.start.is_empty());
}

#[test]
fn w2_a_hold_by_another_cause_restarts_the_load_wait_clock() {
    let t0 = NOW;
    let mut job = wait("X", 1.0, None, 0.0);
    let d = pass(t0, idle_machine(), vec![], vec![job.clone()]);
    carry(&mut job, &d);
    // At 900 s the job is held by cores, which is not a machine cause: its clock clears.
    let busy = vec![run("R", 8.0, Some(8.0))];
    let d = pass(t0 + 900.0, machine(), busy, vec![job.clone()]);
    assert_eq!(held_cause(&d, "X"), Some(Cause::Cores));
    carry(&mut job, &d);
    assert_eq!(job.machine_held_since_s, None);
    // Idle again at 901 s: the clock starts there. The old clock would have failed the job at 1800 s.
    let d = pass(t0 + 901.0, idle_machine(), vec![], vec![job.clone()]);
    carry(&mut job, &d);
    assert_eq!(job.machine_held_since_s, Some(t0 + 901.0));
    let d = pass(t0 + 1800.0, idle_machine(), vec![], vec![job.clone()]);
    assert!(d.fail.is_empty());
    carry(&mut job, &d);
    let d = pass(t0 + 2701.0, idle_machine(), vec![], vec![job]);
    assert_eq!(
        d.fail,
        vec![Fail {
            id: "X".to_owned(),
            note: idle_fail_note("30 min 0 s"),
        }],
    );
}

#[test]
fn w3_memory_idle_and_load_holds_share_one_clock() {
    let t0 = NOW;
    let mut job = wait("X", 1.0, None, 0.0);
    let d = pass(t0, memory_machine(), vec![], vec![job.clone()]);
    assert_eq!(held_cause(&d, "X"), Some(Cause::Memory));
    carry(&mut job, &d);
    let d = pass(t0 + 600.0, idle_machine(), vec![], vec![job.clone()]);
    assert_eq!(held_cause(&d, "X"), Some(Cause::Idle));
    carry(&mut job, &d);
    assert_eq!(job.machine_held_since_s, Some(t0));
    let d = pass(t0 + 1200.0, load_machine(), vec![], vec![job.clone()]);
    assert_eq!(held_cause(&d, "X"), Some(Cause::Load));
    carry(&mut job, &d);
    assert_eq!(job.machine_held_since_s, Some(t0));
    let d = pass(t0 + 1799.0, load_machine(), vec![], vec![job.clone()]);
    assert!(d.fail.is_empty());
    carry(&mut job, &d);
    let d = pass(t0 + 1800.0, load_machine(), vec![], vec![job]);
    assert_eq!(
        d.fail,
        vec![Fail {
            id: "X".to_owned(),
            note: "the load average 92.80 is at or above 12.00 for 30 min 0 s, the longest a job waits on the machine's \
                   pressure (agora work settings --load-wait)"
                .to_owned(),
        }],
    );
}

#[test]
fn w4_holds_that_are_not_machine_pressure_never_fail() {
    // Every waiting job has been held by its cause for 10 000 s, far past the 1800 s load wait.
    let stuck = |id: &str, weight: f64, pred: Option<f64>, age: f64| WaitingJob {
        machine_held_since_s: Some(NOW - 10_000.0),
        ..wait(id, weight, pred, age)
    };
    let check = |d: &Decision, id: &str, cause: Cause| {
        assert!(d.fail.is_empty(), "no failure for {id}");
        assert_eq!(held_cause(d, id), Some(cause));
        assert!(
            d.machine_held_since
                .iter()
                .all(|(_, since)| since.is_none())
        );
    };

    let capped = WorkSettings {
        capacity: Some(4.0),
        ..settings()
    };
    let d = quiet::decide(&input(
        capped,
        machine(),
        vec![run("R", 3.0, None)],
        vec![stuck("X", 2.0, None, 10.0)],
    ));
    check(&d, "X", Cause::Capacity);

    let memory_budget = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let d = quiet::decide(&input(
        memory_budget,
        machine_with(Signals {
            memory_used_kib: Some(900_000),
            ..calm()
        }),
        vec![run("R", 1.0, Some(1.0))],
        vec![WaitingJob {
            pred_rss_p90_kib: Some(200_000),
            ..stuck("X", 1.0, Some(1.0), 10.0)
        }],
    ));
    check(&d, "X", Cause::Reserved);

    // Five cores: the 4-core head does not fit beside the busy 2-core job, and the 2-core medium job waits behind it.
    let five = WorkSettings {
        cpu_budget: Some(5.0),
        ..settings()
    };
    let d = quiet::decide(&input(
        five,
        machine(),
        vec![RunningJob {
            started_s: Some(NOW - 60.0),
            ..run("busy", 2.0, None)
        }],
        vec![
            stuck("head", 4.0, None, 10.0),
            stuck("medium", 2.0, None, 5.0),
        ],
    ));
    check(&d, "head", Cause::Cores);
    check(&d, "medium", Cause::Ahead);

    let d = pass(
        NOW,
        machine(),
        vec![RunningJob {
            started_s: Some(NOW - 100.0),
            ..run("H", 4.0, None)
        }],
        vec![stuck("T", 8.0, Some(1.0), 10.0)],
    );
    check(&d, "T", Cause::Quiet);
}
