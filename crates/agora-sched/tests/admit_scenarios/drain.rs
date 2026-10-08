//! Quiet windows and exclusive jobs, ref-admission section 8 (table D), through `quiet::blocker` and `quiet::decide`.

use super::*;
use agora_sched::quiet::Start;

// ---- D. Quiet and exclusive (Q1-Q17), 8 CPUs, quiet drain 60 s, quiet max 180 s ----

#[test]
fn q1_an_exclusive_job_drains_the_heavy_job_still_running() {
    let heavy = RunningJob {
        started_s: Some(NOW - 100.0),
        ..run("H", 4.0, None)
    };
    let inp = input(
        settings(),
        machine(),
        vec![heavy],
        vec![wait("T", 8.0, Some(1.0), 10.0)],
    );
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Quiet,
            "it drains: job H, predicted above 1 core, still runs; it starts in 50.0 s at the latest",
        ),
    );
}

#[test]
fn q1b_the_drain_names_every_heavy_job_still_running() {
    let heavy = |id: &str| RunningJob {
        started_s: Some(NOW - 100.0),
        ..run(id, 4.0, None)
    };
    let two = input(
        settings(),
        machine(),
        vec![heavy("H1"), heavy("H2")],
        vec![wait("T", 8.0, Some(1.0), 10.0)],
    );
    assert_eq!(
        blocked(&two, "T"),
        hold(
            Cause::Quiet,
            "it drains: jobs H1 and H2, predicted above 1 core, still run; it starts in 50.0 s at the latest",
        ),
    );
    let three = input(
        settings(),
        machine(),
        vec![heavy("H1"), heavy("H2"), heavy("H3")],
        vec![wait("T", 8.0, Some(1.0), 10.0)],
    );
    assert_eq!(
        blocked(&three, "T"),
        hold(
            Cause::Quiet,
            "it drains: jobs H1, H2 and H3, predicted above 1 core, still run; it starts in 50.0 s at the latest",
        ),
    );
}

#[test]
fn q2_past_the_drain_the_exclusive_job_starts_beside_heavy_jobs() {
    let heavy = RunningJob {
        started_s: Some(NOW - 100.0),
        ..run("H", 4.0, None)
    };
    let at_drain_end = input(
        settings(),
        machine(),
        vec![heavy.clone()],
        vec![wait("T", 8.0, Some(1.0), 60.0)],
    );
    assert_eq!(blocked(&at_drain_end, "T"), None);
    let past = input(
        settings(),
        machine(),
        vec![heavy],
        vec![wait("T", 8.0, Some(1.0), 61.0)],
    );
    assert_eq!(blocked(&past, "T"), None);
    assert_eq!(
        quiet::decide(&past).start,
        vec![Start {
            id: "T".to_owned(),
            exclusive: true,
            quiet_until_s: Some(NOW + 180.0),
        }],
    );
}

#[test]
fn q3_an_exclusive_job_reports_its_cores_and_when_the_drain_ends() {
    let inp = input(
        settings(),
        machine(),
        vec![run("L", 1.0, Some(1.0))],
        vec![wait("T", 8.0, Some(8.0), 20.0)],
    );
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Cores,
            "predicted 1 of 8 cores are taken and this job needs 8; it starts in 40.0 s at the latest",
        ),
    );
}

#[test]
fn q4_a_second_exclusive_job_waits_for_the_open_window() {
    let window = RunningJob {
        quiet_until_s: Some(NOW + 30.0),
        ..run("F", 8.0, Some(1.0))
    };
    let inp = input(
        settings(),
        machine(),
        vec![window],
        vec![wait("S", 8.0, None, 40.0)],
    );
    assert_eq!(
        blocked(&inp, "S"),
        hold(
            Cause::Quiet,
            "job F's quiet window is open for 30.0 s more: one quiet window at a time",
        ),
    );
}

#[test]
fn q5_a_just_closed_window_still_counts_the_drain_from_its_close() {
    let closed = RunningJob {
        quiet_until_s: Some(NOW - 1.0),
        ..run("F", 8.0, Some(1.0))
    };
    let inp = AdmitInput {
        window_closes: vec![("F".to_owned(), NOW - 1.0)],
        ..input(
            settings(),
            machine(),
            vec![closed],
            vec![wait("S", 8.0, None, 40.0)],
        )
    };
    assert_eq!(
        blocked(&inp, "S"),
        hold(
            Cause::Cores,
            "predicted 1 of 8 cores are taken and this job needs 8; it starts in 59.0 s at the latest",
        ),
    );
}

#[test]
fn q6_a_drain_that_began_after_the_window_closed_has_passed() {
    let closed = RunningJob {
        quiet_until_s: Some(NOW - 61.0),
        ..run("F", 8.0, Some(1.0))
    };
    let inp = AdmitInput {
        window_closes: vec![("F".to_owned(), NOW - 61.0)],
        ..input(
            settings(),
            machine(),
            vec![closed],
            vec![wait("S", 8.0, None, 100.0)],
        )
    };
    assert_eq!(blocked(&inp, "S"), None);
}

#[test]
fn q7_a_second_exclusive_job_waits_behind_the_first_in_line() {
    let heavy = RunningJob {
        started_s: Some(NOW - 100.0),
        ..run("H", 4.0, None)
    };
    let inp = input(
        settings(),
        machine(),
        vec![heavy],
        vec![
            wait("First", 8.0, Some(1.0), 20.0),
            wait("Second", 8.0, Some(1.0), 10.0),
        ],
    );
    assert_eq!(
        blocked(&inp, "Second"),
        hold(
            Cause::Quiet,
            "job First, exclusive too, is ahead of it: one quiet window at a time",
        ),
    );
    assert_eq!(
        blocked(&inp, "First"),
        hold(
            Cause::Quiet,
            "it drains: job H, predicted above 1 core, still runs; it starts in 40.0 s at the latest",
        ),
    );
}

#[test]
fn q8_a_heavy_job_waits_while_a_quiet_window_is_open() {
    let window = RunningJob {
        quiet_until_s: Some(NOW + 100.0),
        ..run("T", 8.0, Some(1.0))
    };
    let inp = input(
        settings(),
        machine(),
        vec![window],
        vec![wait("N", 4.0, None, 10.0)],
    );
    assert_eq!(
        blocked(&inp, "N"),
        hold(
            Cause::Quiet,
            "job T runs in its quiet window for 1 min 40 s more; no job predicted above 1 core starts in it, and this \
             job needs 4",
        ),
    );
}

#[test]
fn q9_a_light_job_starts_while_a_quiet_window_is_open() {
    let window = RunningJob {
        quiet_until_s: Some(NOW + 100.0),
        ..run("T", 8.0, Some(1.0))
    };
    let inp = input(
        settings(),
        machine(),
        vec![window],
        vec![wait("L", 1.0, None, 10.0)],
    );
    assert_eq!(blocked(&inp, "L"), None);
}

#[test]
fn q10_a_heavy_job_starts_once_the_window_is_over() {
    let closed = RunningJob {
        quiet_until_s: Some(NOW - 1.0),
        ..run("T", 8.0, Some(1.0))
    };
    let inp = input(
        settings(),
        machine(),
        vec![closed],
        vec![wait("N", 4.0, None, 10.0)],
    );
    assert_eq!(blocked(&inp, "N"), None);
}

#[test]
fn q11_a_heavy_job_waits_for_the_exclusive_jobs_drain_to_start() {
    let inp = input(
        settings(),
        machine(),
        vec![],
        vec![wait("T", 8.0, None, 10.0), wait("N", 4.0, None, 10.0)],
    );
    assert_eq!(
        blocked(&inp, "N"),
        hold(
            Cause::Quiet,
            "job T waits to run exclusive, and for 50.0 s more no job predicted above 1 core starts; this job needs 4",
        ),
    );
    // Once the exclusive job's drain has passed, the heavy job is no longer held by it.
    let drained = input(
        settings(),
        machine(),
        vec![],
        vec![wait("T", 8.0, None, 61.0), wait("N", 4.0, None, 10.0)],
    );
    assert_eq!(blocked(&drained, "N"), None);
}

#[test]
fn q12_the_small_weight_sets_the_cores_named_in_a_window_hold() {
    let s = WorkSettings {
        small_weight: 0.1,
        ..settings()
    };
    let window = RunningJob {
        quiet_until_s: Some(NOW + 100.0),
        ..run("T", 8.0, Some(1.0))
    };
    let inp = input(
        s,
        machine(),
        vec![window],
        vec![wait("N", 1.0, Some(0.2), 10.0)],
    );
    assert_eq!(
        blocked(&inp, "N"),
        hold(
            Cause::Quiet,
            "job T runs in its quiet window for 1 min 40 s more; no job predicted above 0.1 cores starts in it, and \
             this job needs 0.2",
        ),
    );
}

#[test]
fn q13_a_zero_quiet_max_never_opens_a_window() {
    let s = WorkSettings {
        quiet_max: 0.0,
        ..settings()
    };
    let started = quiet::decide(&input(
        s.clone(),
        machine(),
        vec![],
        vec![wait("T", 8.0, None, 10.0)],
    ));
    assert_eq!(
        started.start,
        vec![Start {
            id: "T".to_owned(),
            exclusive: true,
            quiet_until_s: Some(NOW),
        }],
    );
    // The started job's window ends at its start (quiet_until == now is not open), so only cores hold the next job.
    let running = RunningJob {
        quiet_until_s: Some(NOW),
        ..run("T", 8.0, None)
    };
    let inp = input(
        s,
        machine(),
        vec![running],
        vec![wait("N", 4.0, None, 10.0)],
    );
    assert_eq!(
        blocked(&inp, "N"),
        hold(
            Cause::Cores,
            "predicted 8 of 8 cores are taken and this job needs 4"
        ),
    );
}

#[test]
fn q14_under_two_cpus_no_job_is_exclusive() {
    let one_cpu = MachineSnapshot {
        cpus: 1.0,
        ..machine()
    };
    let inp = input(
        settings(),
        one_cpu,
        vec![run("H", 1.0, None)],
        vec![wait("T", 1.0, Some(1.0), 10.0)],
    );
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Cores,
            "predicted 1 of 1 cores are taken and this job needs 1"
        ),
    );
}

#[test]
fn q15_past_the_drain_an_exclusive_job_starts_even_if_heavy_jobs_run() {
    let heavy = RunningJob {
        started_s: Some(NOW - 100.0),
        ..run("H", 6.0, Some(6.0))
    };
    let inp = input(
        settings(),
        machine(),
        vec![heavy],
        vec![wait("T", 8.0, Some(8.0), 100.0)],
    );
    assert_eq!(blocked(&inp, "T"), None);
}

#[test]
fn q16_past_the_drain_an_idle_hold_has_no_drain_suffix() {
    let inp = input(
        settings(),
        idle_machine(),
        vec![],
        vec![wait("T", 8.0, None, 100.0)],
    );
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Idle,
            "the CPU is 5% idle; jobs start at 10% idle or more"
        ),
    );
}

#[test]
fn q17_during_the_drain_a_memory_or_pressure_hold_is_returned_as_is() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let reserved_m = machine_with(Signals {
        memory_used_kib: Some(900_000),
        ..calm()
    });
    let light = run("R", 1.0, Some(1.0));
    let cand = WaitingJob {
        pred_rss_p90_kib: Some(200_000),
        ..wait("T", 8.0, Some(1.0), 10.0)
    };
    let inp = input(s, reserved_m, vec![light], vec![cand]);
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Reserved,
            "used memory, the running jobs' predicted growth and this job's predicted peak come to 1.0 GiB, above the \
             memory budget of 1.0 GiB",
        ),
    );
    let inp = input(
        settings(),
        idle_machine(),
        vec![],
        vec![wait("T", 8.0, None, 10.0)],
    );
    assert_eq!(
        blocked(&inp, "T"),
        hold(
            Cause::Idle,
            "the CPU is 5% idle; jobs start at 10% idle or more"
        ),
    );
}
