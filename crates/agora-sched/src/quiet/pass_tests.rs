//! Admission-pass tests: the load wait (W1-W4), pause, manual quiet, one pass of several jobs, and next_wakeup.

use super::tests::{NOW, idle_signals, input, run, wait};
use super::{ManualQuiet, decide};
use crate::admit::{Cause, MemLevel, Signals};

#[test]
fn w1_idle_fails_once_the_load_wait_has_passed() {
    let mut i = input(0.0, Vec::new(), vec![wait("J", 1.0, 0.0, None)]);
    i.machine.signals = Some(idle_signals(5.0));
    let d = decide(&i);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), Some(0.0))]);
    assert!(d.machine_gated);

    i.now_s = 1799.0;
    i.waiting[0].machine_held_since_s = Some(0.0);
    let d = decide(&i);
    assert!(d.fail.is_empty());
    assert_eq!(d.held.len(), 1);

    i.now_s = 1800.0;
    let d = decide(&i);
    assert_eq!(d.fail.len(), 1);
    assert_eq!(
        d.fail[0].note,
        "the CPU is 5% idle; jobs start at 10% idle or more for 30 min 0 s, the longest a job waits on the \
         machine's pressure (agora work settings --load-wait)"
    );
    assert!(d.held.is_empty());
}

#[test]
fn w2_a_non_machine_hold_clears_the_timer() {
    let mut i = input(0.0, Vec::new(), vec![wait("J", 1.0, 0.0, None)]);
    i.machine.signals = Some(idle_signals(5.0));
    assert_eq!(
        decide(&i).machine_held_since,
        vec![("J".to_string(), Some(0.0))]
    );

    // At 900 s a running job takes the whole CPU budget: a cores hold, which clears the timer.
    i.now_s = 900.0;
    i.machine.signals = Some(idle_signals(50.0));
    i.running = vec![run("R", 8.0, Some(8.0), 850.0, None)];
    i.waiting[0].machine_held_since_s = Some(0.0);
    let d = decide(&i);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), None)]);
    assert!(!d.machine_gated);

    // Idle again at 901 s: the timer restarts there, so no failure at 1800 s.
    i.now_s = 901.0;
    i.running.clear();
    i.machine.signals = Some(idle_signals(5.0));
    i.waiting[0].machine_held_since_s = None;
    let d = decide(&i);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), Some(901.0))]);

    i.now_s = 2700.0;
    i.waiting[0].machine_held_since_s = Some(901.0);
    assert!(decide(&i).fail.is_empty());
    i.now_s = 2701.0;
    assert_eq!(decide(&i).fail.len(), 1);
}

#[test]
fn w3_memory_idle_and_load_share_one_timer() {
    let mut i = input(0.0, Vec::new(), vec![wait("J", 1.0, 0.0, None)]);
    i.machine.signals = Some(Signals {
        memory_level: Some(MemLevel::Critical),
        ..Signals::default()
    });
    let d = decide(&i);
    assert_eq!(d.held[0].cause, Cause::Memory);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), Some(0.0))]);

    // Idle from 600 s on, then load from 1200 s on (load 20 is above 8 CPUs * 1.5): no reset.
    i.now_s = 600.0;
    i.waiting[0].machine_held_since_s = Some(0.0);
    i.machine.signals = Some(idle_signals(5.0));
    let d = decide(&i);
    assert_eq!(d.held[0].cause, Cause::Idle);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), Some(0.0))]);

    i.now_s = 1200.0;
    i.machine.signals = Some(Signals {
        load1: Some(20.0),
        load_gate: true,
        ..Signals::default()
    });
    let d = decide(&i);
    assert_eq!(d.held[0].cause, Cause::Load);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), Some(0.0))]);

    i.now_s = 1800.0;
    let d = decide(&i);
    assert_eq!(d.fail.len(), 1);
    assert!(
        d.fail[0]
            .note
            .starts_with("the load average 20.00 is at or above 12.00 for 30 min 0 s")
    );
}

#[test]
fn w4_holds_that_are_not_the_machines_never_fail() {
    // Cores and quiet holds keep no timer and never fail, however long they last.
    let mut i = input(
        5000.0,
        vec![run("R", 8.0, Some(8.0), 4000.0, None)],
        vec![wait("J", 1.0, 0.0, None)],
    );
    i.waiting[0].machine_held_since_s = Some(0.0);
    let d = decide(&i);
    assert!(d.fail.is_empty());
    assert_eq!(d.held[0].cause, Cause::Cores);
    assert_eq!(d.machine_held_since, vec![("J".to_string(), None)]);

    let mut i = input(5000.0, Vec::new(), vec![wait("N", 4.0, 0.0, None)]);
    i.waiting[0].machine_held_since_s = Some(0.0);
    i.manual_quiet = vec![ManualQuiet {
        lease: "l".into(),
        holder_name: "Ann".into(),
        until_s: 9000.0,
        note: String::new(),
    }];
    let d = decide(&i);
    assert!(d.fail.is_empty());
    assert_eq!(d.held[0].cause, Cause::Quiet);
    assert_eq!(d.machine_held_since, vec![("N".to_string(), None)]);
}

#[test]
fn paused_holds_every_job_and_names_the_reason() {
    let mut i = input(
        NOW,
        Vec::new(),
        vec![wait("T", 8.0, 900.0, Some(8.0)), wait("A", 1.0, 0.0, None)],
    );
    i.paused = Some("disk floor breached".into());
    let d = decide(&i);
    assert!(d.start.is_empty());
    assert!(!d.machine_gated);
    assert_eq!(d.held.len(), 2);
    for h in &d.held {
        assert_eq!(h.cause, Cause::Paused);
        assert_eq!(h.reason, "admission is paused: disk floor breached");
    }
    assert!(
        d.machine_held_since
            .iter()
            .all(|(_, since)| since.is_none())
    );
}

#[test]
fn manual_quiet_holds_heavy_jobs_and_light_ones_start() {
    let mut i = input(
        NOW,
        Vec::new(),
        vec![wait("N", 4.0, 0.0, None), wait("L", 1.0, 10.0, None)],
    );
    i.manual_quiet = vec![ManualQuiet {
        lease: "l-1".into(),
        holder_name: "Ann".into(),
        until_s: 1120.0,
        note: "benchmark".into(),
    }];
    let d = decide(&i);
    assert_eq!(
        d.start.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["L"]
    );
    assert_eq!(d.held.len(), 1);
    assert_eq!(d.held[0].id, "N");
    assert_eq!(d.held[0].cause, Cause::Quiet);
    assert_eq!(
        d.held[0].reason,
        "quiet window l-1 opened by Ann is open for 2 min 0 s more: benchmark; no job predicted above 1 core \
         starts in it, and this job needs 4"
    );
}

#[test]
fn two_jobs_started_in_one_pass_the_second_sees_the_first() {
    let i = input(
        NOW,
        Vec::new(),
        vec![wait("A", 6.0, 0.0, None), wait("B", 4.0, 10.0, None)],
    );
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert_eq!(d.start[0].id, "A");
    assert!(!d.start[0].exclusive);
    assert_eq!(d.held.len(), 1);
    assert_eq!(d.held[0].id, "B");
    assert_eq!(d.held[0].cause, Cause::Cores);
    assert_eq!(
        d.held[0].reason,
        "predicted 6 of 8 cores are taken and this job needs 4"
    );
}

#[test]
fn next_wakeup_is_the_earliest_future_instant() {
    // H's p90 (1100) and T's drain end (990 + 60 = 1050) are later than N's head limit (500 + 300 = 800, past).
    let mut h = run("H", 4.0, None, 900.0, None);
    h.pred_duration_p90_s = Some(200.0);
    let i = input(
        NOW,
        vec![h],
        vec![
            wait("N", 4.0, 500.0, None),
            wait("T", 8.0, 990.0, Some(1.0)),
        ],
    );
    let d = decide(&i);
    assert!(d.start.is_empty());
    assert_eq!(d.next_wakeup_s, Some(1050.0));
}

#[test]
fn next_wakeup_head_backfill_limit_and_load_wait_and_manual_until() {
    // The head (Z, no prediction) passes its backfill limit at 900 + 300 = 1200.
    let i = input(
        NOW,
        vec![run("R", 8.0, Some(8.0), 950.0, None)],
        vec![wait("Z", 1.0, 900.0, None)],
    );
    assert_eq!(decide(&i).next_wakeup_s, Some(1200.0));

    // A machine hold: its load-wait instant is now + 1800; the open manual window (1500) is earlier.
    let mut i = input(NOW, Vec::new(), vec![wait("J", 1.0, 0.0, None)]);
    i.machine.signals = Some(idle_signals(5.0));
    assert_eq!(decide(&i).next_wakeup_s, Some(2800.0));
    i.manual_quiet = vec![ManualQuiet {
        lease: "l".into(),
        holder_name: "Ann".into(),
        until_s: 1500.0,
        note: String::new(),
    }];
    assert_eq!(decide(&i).next_wakeup_s, Some(1500.0));
}

#[test]
fn started_exclusive_job_wakes_at_its_window_close() {
    let i = input(NOW, Vec::new(), vec![wait("T", 8.0, 1000.0, Some(1.0))]);
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert!(d.start[0].exclusive);
    assert_eq!(d.start[0].quiet_until_s, Some(1180.0));
    assert_eq!(d.next_wakeup_s, Some(1180.0));
}
