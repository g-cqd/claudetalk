//! Scenario tests for the admission pass (ref-admission section 15, tables D and E; fixture F8 machine).

use super::{AdmitInput, ManualQuiet, blocker, decide, quiet_blocker};
use crate::admit::{Cause, Hold, MachineSnapshot, MemLevel, RunningJob, Signals, WaitingJob};
use crate::settings::WorkSettings;

pub(super) const NOW: f64 = 1000.0;

pub(super) fn snap(cpus: f64) -> MachineSnapshot {
    MachineSnapshot {
        cpus,
        total_memory_kib: None,
        disk_free_kib: None,
        signals: None,
    }
}

pub(super) fn idle_signals(pct: f64) -> Signals {
    Signals {
        idle_pct: Some(pct),
        ..Signals::default()
    }
}

pub(super) fn input(now: f64, running: Vec<RunningJob>, waiting: Vec<WaitingJob>) -> AdmitInput {
    AdmitInput {
        now_s: now,
        settings: WorkSettings::default(),
        machine: snap(8.0),
        running,
        waiting,
        window_closes: Vec::new(),
        paused: None,
        manual_quiet: Vec::new(),
    }
}

pub(super) fn wait(id: &str, weight: f64, requested: f64, pred_cores: Option<f64>) -> WaitingJob {
    WaitingJob {
        id: id.into(),
        kind: "feature".into(),
        seq: 1,
        requested_s: requested,
        weight,
        pred_cores,
        pred_duration_s: None,
        pred_duration_p90_s: None,
        pred_rss_p90_kib: None,
        machine_held_since_s: None,
    }
}

pub(super) fn run(
    id: &str,
    weight: f64,
    pred_cores: Option<f64>,
    started: f64,
    quiet_until: Option<f64>,
) -> RunningJob {
    RunningJob {
        id: id.into(),
        weight,
        pred_cores,
        pred_rss_p90_kib: None,
        pred_duration_p90_s: None,
        rss_kib: None,
        started_s: Some(started),
        quiet_until_s: quiet_until,
    }
}

/// The blocker verdict for the waiting job `id` against the input's own running and waiting sets.
pub(super) fn judge(i: &AdmitInput, id: &str) -> Option<Hold> {
    let job = i
        .waiting
        .iter()
        .find(|w| w.id == id)
        .expect("job is waiting");
    blocker(job, i, &i.running, &i.waiting)
}

pub(super) fn hold(cause: Cause, reason: &str) -> Option<Hold> {
    Some(Hold {
        cause,
        reason: reason.into(),
    })
}

#[test]
fn q1_drain_names_the_heavy_jobs_and_the_time_left() {
    let t = wait("T", 8.0, 990.0, Some(1.0));
    let one = input(NOW, vec![run("H", 4.0, None, 900.0, None)], vec![t.clone()]);
    let text =
        "it drains: job H, predicted above 1 core, still runs; it starts in 50.0 s at the latest";
    assert_eq!(judge(&one, "T"), hold(Cause::Quiet, text));

    let two = input(
        NOW,
        vec![
            run("H1", 4.0, None, 900.0, None),
            run("H2", 4.0, None, 900.0, None),
        ],
        vec![t.clone()],
    );
    let text = "it drains: jobs H1 and H2, predicted above 1 core, still run; it starts in 50.0 s at the latest";
    assert_eq!(judge(&two, "T"), hold(Cause::Quiet, text));

    let three = input(
        NOW,
        vec![
            run("a", 4.0, None, 900.0, None),
            run("b", 4.0, None, 900.0, None),
            run("c", 4.0, None, 900.0, None),
        ],
        vec![t],
    );
    let text = "it drains: jobs a, b and c, predicted above 1 core, still run; it starts in 50.0 s at the latest";
    assert_eq!(judge(&three, "T"), hold(Cause::Quiet, text));
}

#[test]
fn q2_past_the_drain_the_job_starts_and_opens_its_window() {
    // Asked 61 s ago, or exactly at the drain end (60 s): both are past the drain, with no cores check.
    for requested in [939.0, 940.0] {
        let i = input(NOW, Vec::new(), vec![wait("T", 8.0, requested, None)]);
        assert_eq!(judge(&i, "T"), None);
    }
    let i = input(NOW, Vec::new(), vec![wait("T", 8.0, 939.0, Some(1.0))]);
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert!(d.start[0].exclusive);
    assert_eq!(d.start[0].quiet_until_s, Some(1180.0));
}

#[test]
fn q3_q5_cores_hold_of_an_exclusive_job_says_when_it_starts() {
    let i = input(
        NOW,
        vec![run("L", 1.0, Some(1.0), 990.0, None)],
        vec![wait("T", 8.0, 980.0, Some(8.0))],
    );
    let text =
        "predicted 1 of 8 cores are taken and this job needs 8; it starts in 40.0 s at the latest";
    assert_eq!(judge(&i, "T"), hold(Cause::Cores, text));

    // F's window closed a second ago, so F is an ordinary running job of one core. The close (999) is later than S's
    // request (960), so the drain counts from it: 999 + 60 = 1059.
    let mut i = input(
        NOW,
        vec![run("F", 8.0, Some(1.0), 900.0, Some(999.0))],
        vec![wait("S", 8.0, 960.0, None)],
    );
    i.window_closes = vec![("F".into(), 999.0)];
    let text =
        "predicted 1 of 8 cores are taken and this job needs 8; it starts in 59.0 s at the latest";
    assert_eq!(judge(&i, "S"), hold(Cause::Cores, text));
}

#[test]
fn q4_one_quiet_window_at_a_time() {
    let i = input(
        NOW,
        vec![run("F", 8.0, Some(1.0), 900.0, Some(1030.0))],
        vec![wait("S", 8.0, 960.0, None)],
    );
    let text = "job F's quiet window is open for 30.0 s more: one quiet window at a time";
    assert_eq!(judge(&i, "S"), hold(Cause::Quiet, text));
}

#[test]
fn q6_q10_a_closed_window_no_longer_holds_the_line() {
    // Q6: the exclusive job's drain ended (window closed at 939, asked at 900, end 960).
    let i = input(
        NOW,
        vec![run("F", 8.0, Some(1.0), 900.0, Some(939.0))],
        vec![wait("S", 8.0, 900.0, None)],
    );
    assert_eq!(judge(&i, "S"), None);

    // Q10: the window closed a moment ago, so a heavy job is only held by the cores it finds.
    let i = input(
        NOW,
        vec![run("T", 8.0, Some(1.0), 900.0, Some(999.0))],
        vec![wait("N", 4.0, 950.0, None)],
    );
    assert_eq!(judge(&i, "N"), None);
}

#[test]
fn q7_a_second_exclusive_job_waits_for_the_first() {
    let base =
        |waiting: Vec<WaitingJob>| input(NOW, vec![run("H", 4.0, None, 900.0, None)], waiting);
    let i = base(vec![
        wait("First", 8.0, 980.0, Some(1.0)),
        wait("Second", 8.0, 990.0, Some(1.0)),
    ]);
    let text = "job First, exclusive too, is ahead of it: one quiet window at a time";
    assert_eq!(judge(&i, "Second"), hold(Cause::Quiet, text));
    let text =
        "it drains: job H, predicted above 1 core, still runs; it starts in 40.0 s at the latest";
    assert_eq!(judge(&i, "First"), hold(Cause::Quiet, text));
}

#[test]
fn q8_q9_an_open_window_holds_heavy_jobs_and_lets_light_ones_by() {
    let i = input(
        NOW,
        vec![run("T", 8.0, Some(1.0), 900.0, Some(1100.0))],
        vec![wait("N", 4.0, 950.0, None), wait("L", 1.0, 950.0, None)],
    );
    let text = "job T runs in its quiet window for 1 min 40 s more; no job predicted above 1 core starts in it, \
                and this job needs 4";
    assert_eq!(judge(&i, "N"), hold(Cause::Quiet, text));
    assert_eq!(judge(&i, "L"), None);
}

#[test]
fn an_open_manual_window_holds_heavy_exclusive_jobs_and_not_light_ones() {
    let manual = vec![ManualQuiet {
        lease: "L1".into(),
        holder_name: "alice".into(),
        until_s: NOW + 60.0,
        note: String::new(),
    }];
    // Exclusive on an 8-cpu machine and predicted at 4 cores: the window holds it before the exclusive rules run.
    let mut heavy = input(NOW, Vec::new(), vec![wait("X", 8.0, 939.0, Some(4.0))]);
    heavy.manual_quiet = manual.clone();
    let held = judge(&heavy, "X").expect("heavy exclusive job is held");
    assert_eq!(held.cause, Cause::Quiet);
    assert!(
        held.reason
            .starts_with("quiet window L1 opened by alice is open for"),
        "{}",
        held.reason
    );

    // Exclusive but predicted at half a core: not heavy, so the window does not hold it, and its drain is over.
    let mut light = input(NOW, Vec::new(), vec![wait("Y", 8.0, 939.0, Some(0.5))]);
    light.manual_quiet = manual;
    assert_eq!(judge(&light, "Y"), None);
}

#[test]
fn q11_light_jobs_wait_while_an_exclusive_job_drains() {
    let i = input(
        NOW,
        Vec::new(),
        vec![
            wait("T", 8.0, 990.0, Some(1.0)),
            wait("N", 4.0, 950.0, None),
        ],
    );
    let text = "job T waits to run exclusive, and for 50.0 s more no job predicted above 1 core starts; this job needs 4";
    assert_eq!(judge(&i, "N"), hold(Cause::Quiet, text));

    // At 61 s since T asked the drain is over, so N is no longer held by quiet.
    let i = input(
        NOW,
        Vec::new(),
        vec![
            wait("T", 8.0, 939.0, Some(1.0)),
            wait("N", 4.0, 950.0, None),
        ],
    );
    assert_eq!(judge(&i, "N"), None);
}

#[test]
fn q12_small_weight_sets_which_jobs_are_heavy() {
    let mut i = input(
        NOW,
        vec![run("T", 8.0, Some(1.0), 900.0, Some(1100.0))],
        vec![wait("W", 1.0, 950.0, Some(0.2))],
    );
    i.settings.small_weight = 0.1;
    let text = "job T runs in its quiet window for 1 min 40 s more; no job predicted above 0.1 cores starts in it, \
                and this job needs 0.2";
    assert_eq!(judge(&i, "W"), hold(Cause::Quiet, text));
}

#[test]
fn q13_zero_quiet_max_never_opens_a_window() {
    let mut i = input(NOW, Vec::new(), vec![wait("T", 8.0, 939.0, Some(8.0))]);
    i.settings.quiet_max = 0.0;
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert_eq!(d.start[0].quiet_until_s, Some(NOW));

    // quiet_until == now is not open (strict), so the light job meets no window.
    let i = input(
        NOW,
        vec![run("T", 8.0, Some(1.0), 900.0, Some(NOW))],
        vec![wait("N", 4.0, 950.0, None)],
    );
    assert_eq!(judge(&i, "N"), None);
}

#[test]
fn q14_one_cpu_machine_has_no_quiet_logic() {
    let mut i = input(NOW, Vec::new(), vec![wait("A", 1.0, 0.0, None)]);
    i.machine = snap(1.0);
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert!(!d.start[0].exclusive);
    assert_eq!(d.start[0].quiet_until_s, None);
}

#[test]
fn q15_past_the_drain_the_cores_check_is_skipped() {
    let i = input(
        NOW,
        vec![run("H", 6.0, Some(6.0), 900.0, None)],
        vec![wait("T", 8.0, 900.0, Some(8.0))],
    );
    assert_eq!(judge(&i, "T"), None);
    let d = decide(&i);
    assert_eq!(d.start.len(), 1);
    assert_eq!(d.start[0].quiet_until_s, Some(1180.0));
}

#[test]
fn q16_past_the_drain_idle_holds_without_a_drain_suffix() {
    let mut i = input(NOW, Vec::new(), vec![wait("T", 8.0, 900.0, Some(8.0))]);
    i.machine.signals = Some(idle_signals(5.0));
    let text = "the CPU is 5% idle; jobs start at 10% idle or more";
    assert_eq!(judge(&i, "T"), hold(Cause::Idle, text));
}

#[test]
fn q17_past_the_drain_pressure_holds_as_it_is() {
    let mut i = input(NOW, Vec::new(), vec![wait("T", 8.0, 900.0, Some(8.0))]);
    i.machine.signals = Some(Signals {
        memory_level: Some(MemLevel::Critical),
        ..Signals::default()
    });
    let got = judge(&i, "T").expect("pressure holds the job");
    assert_eq!(got.cause, Cause::Memory);
    assert!(
        got.reason.starts_with("memory pressure is critical"),
        "{}",
        got.reason
    );
    assert!(!got.reason.contains("at the latest"), "{}", got.reason);
}

#[test]
fn quiet_blocker_on_a_bare_line_starts_a_job_that_fits_after_its_drain() {
    let t = wait("T", 8.0, 990.0, Some(1.0));
    let i = input(NOW, Vec::new(), vec![t.clone()]);
    // Called directly, the paused check is not run; with no window and no heavy job, the drain is over at 1050.
    assert_eq!(quiet_blocker(&t, &i, &[], &[&t]), None);
    // A second exclusive job ahead in the line is named, whatever the machine holds.
    let u = wait("U", 8.0, 900.0, Some(1.0));
    let text = "job U, exclusive too, is ahead of it: one quiet window at a time";
    assert_eq!(
        quiet_blocker(&t, &i, &[], &[&u, &t]),
        hold(Cause::Quiet, text)
    );
}
