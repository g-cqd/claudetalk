//! Backfill behind a blocked head, ref-admission section 6 (table C), through `admit::backfill` and
//! `admit::head_start_estimate`, plus the tiny-job line rule through `quiet::blocker`.

use super::*;

// ---- C. Backfill (BF1-BF11) ----

/// A backfill setup for the BF1 family: budget 3, small weight 0.1, and running R (w2, p2) started 5 s ago with a p90
/// run time of 100 s, so the head (w2, p2) may start in 95 s.
fn bf1_running() -> Vec<RunningJob> {
    vec![RunningJob {
        started_s: Some(NOW - 5.0),
        pred_duration_p90_s: Some(100.0),
        ..run("R", 2.0, Some(2.0))
    }]
}

fn bf1_settings() -> WorkSettings {
    WorkSettings {
        small_weight: 0.1,
        ..budget(3.0)
    }
}

#[test]
fn bf1_a_job_that_ends_before_the_head_may_start_is_not_held() {
    let head = wait("head", 2.0, Some(2.0), 10.0);
    let cand = WaitingJob {
        pred_duration_p90_s: Some(90.0),
        ..wait("c", 0.5, Some(0.5), 5.0)
    };
    assert_eq!(
        back(&cand, &[&head], &bf1_running(), &bf1_settings(), &machine()),
        None
    );
}

#[test]
fn bf1b_a_job_that_outlasts_the_head_estimate_is_held_with_its_p90() {
    let head = wait("head", 2.0, Some(2.0), 10.0);
    let cand = WaitingJob {
        pred_duration_p90_s: Some(110.0),
        ..wait("c", 0.5, Some(0.5), 5.0)
    };
    assert_eq!(
        back(&cand, &[&head], &bf1_running(), &bf1_settings(), &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it and needs 2 of the 3 cores, which leaves 0 for jobs behind it; this job needs 0.5, 1 min 50 s at p90, and the head may start in 1 min 35 s",
        ),
    );
}

#[test]
fn bf1c_a_job_without_a_p90_is_held_without_the_p90_clause() {
    let head = wait("head", 2.0, Some(2.0), 10.0);
    let cand = wait("c", 0.5, Some(0.5), 5.0);
    assert_eq!(
        back(&cand, &[&head], &bf1_running(), &bf1_settings(), &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it and needs 2 of the 3 cores, which leaves 0 for jobs behind it; this job needs 0.5, and the head may start in 1 min 35 s",
        ),
    );
}

#[test]
fn bf2_a_head_whose_estimate_is_already_due_may_start_any_moment() {
    let running = [RunningJob {
        started_s: Some(NOW - 101.0),
        pred_duration_p90_s: Some(100.0),
        ..run("R", 2.0, Some(2.0))
    }];
    let head = wait("head", 2.0, Some(2.0), 150.0);
    let s = bf1_settings();
    let b = admit::budgets(&s, &machine());
    assert_eq!(admit::head_start_estimate(2.0, &running, &b, NOW), NOW);
    let cand = WaitingJob {
        pred_duration_p90_s: Some(90.0),
        ..wait("c", 0.5, Some(0.5), 5.0)
    };
    assert_eq!(
        back(&cand, &[&head], &running, &s, &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it and needs 2 of the 3 cores, which leaves 0 for jobs behind it; this job needs 0.5, 1 min 30 s at p90, and the head may start any moment",
        ),
    );
}

#[test]
fn bf3_the_head_start_estimate_follows_each_running_end_in_order() {
    let running = [
        RunningJob {
            started_s: Some(NOW - 50.0),
            pred_duration_p90_s: Some(60.0),
            ..run("A", 2.0, Some(2.0))
        },
        run("B", 1.0, Some(1.0)),
        RunningJob {
            pred_duration_p90_s: Some(200.0),
            ..run("C", 1.0, Some(1.0))
        },
        RunningJob {
            started_s: None,
            ..run("D", 1.0, Some(1.0))
        },
    ];
    let b = admit::budgets(&budget(6.0), &machine());
    assert_eq!(
        admit::head_start_estimate(4.0, &running, &b, NOW),
        NOW + 10.0
    );
    assert_eq!(
        admit::head_start_estimate(5.0, &running, &b, NOW),
        NOW + 10.0
    );
    assert_eq!(
        admit::head_start_estimate(6.0, &running, &b, NOW),
        NOW + 200.0
    );
}

#[test]
fn bf4_a_job_that_would_take_the_heads_cores_waits() {
    let s = budget(5.0);
    let busy = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 2.0, None)
    }];
    let head = wait("head", 4.0, None, 10.0);
    let medium = wait("medium", 2.0, None, 5.0);
    let light = wait("light", 1.0, None, 4.0);
    assert_eq!(
        back(&medium, &[&head], &busy, &s, &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it and needs 4 of the 5 cores, which leaves 1 for jobs behind it; this job needs 2, and the head may start any moment",
        ),
    );
    assert_eq!(back(&light, &[&head], &busy, &s, &machine()), None);
}

#[test]
fn bf5_jobs_started_while_the_head_waited_keep_their_cores() {
    let s = WorkSettings {
        small_weight: 0.5,
        ..budget(5.0)
    };
    let running = [
        RunningJob {
            started_s: Some(NOW - 60.0),
            ..run("busy", 1.0, None)
        },
        RunningJob {
            started_s: Some(NOW - 10.0),
            ..run("first", 1.0, None)
        },
    ];
    let head = wait("head", 4.0, None, 30.0);
    let second = wait("second", 1.0, None, 5.0);
    assert_eq!(
        back(&second, &[&head], &running, &s, &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it and needs 4 of the 5 cores, which leaves 0 for jobs behind it; this job needs 1, and the head may start any moment",
        ),
    );
}

#[test]
fn bf6_a_small_job_backfills_until_the_head_has_waited_the_limit() {
    let s = budget(3.0);
    let busy = [RunningJob {
        started_s: Some(NOW - 600.0),
        ..run("busy", 1.0, None)
    }];
    let head = wait("head", 3.0, None, 10.0);
    let small = wait("small", 1.0, None, 5.0);
    assert_eq!(back(&small, &[&head], &busy, &s, &machine()), None);

    let starved_head = wait("head", 3.0, None, 400.0);
    assert_eq!(
        back(&small, &[&starved_head], &busy, &s, &machine()),
        hold(
            Cause::Ahead,
            "job head has waited 6 min 40 s, past the backfill limit of 5 min 0 s, so no job starts before it",
        ),
    );
    // Once the busy job ends, the head is no longer held and the small job still waits behind it.
    assert_eq!(back(&head, &[], &[], &s, &machine()), None);
    assert_eq!(
        back(&small, &[&head], &[], &s, &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it, and there is room for it"
        ),
    );
}

#[test]
fn bf7_a_validity_head_keeps_its_place_from_small_jobs() {
    let busy = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 1.0, None)
    }];
    let fix = WaitingJob {
        kind: "validity".to_owned(),
        ..wait("fix", 3.0, None, 10.0)
    };
    let small = wait("small", 1.0, None, 5.0);
    assert_eq!(
        back(&small, &[&fix], &busy, &budget(3.0), &machine()),
        hold(
            Cause::Ahead,
            "job fix is ahead of it and needs 3 of the 3 cores, which leaves 0 for jobs behind it; this job needs 1, and the head may start any moment, and job fix, validity work, keeps its place",
        ),
    );
    // With five cores and a 3-core busy job, the 3-core validity head leaves 2 for the small job: it may start.
    let busy_three = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 3.0, None)
    }];
    assert_eq!(
        back(&small, &[&fix], &busy_three, &budget(5.0), &machine()),
        None
    );
}

#[test]
fn bf8_backfill_never_takes_the_heads_turn() {
    let s = budget(2.0);
    let head = wait("head", 1.0, None, 10.0);
    let small = wait("small", 1.0, None, 5.0);
    assert_eq!(
        back(&small, &[&head], &[], &s, &machine()),
        hold(
            Cause::Ahead,
            "job head is ahead of it, and there is room for it"
        ),
    );
    assert_eq!(back(&head, &[], &[], &s, &machine()), None);
}

#[test]
fn bf9_a_head_held_for_memory_is_reported_as_waiting_for_memory() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let m = machine_with(Signals {
        memory_used_kib: Some(900_000),
        ..calm()
    });
    let running = [run("R", 1.0, Some(1.0))];
    let head = WaitingJob {
        pred_rss_p90_kib: Some(200_000),
        ..wait("head", 1.0, Some(1.0), 10.0)
    };
    let cand = wait("cand", 1.0, Some(1.0), 5.0);
    assert_eq!(
        back(&cand, &[&head], &running, &s, &m),
        hold(
            Cause::Ahead,
            "job head is ahead of it and waits for memory: used memory, the running jobs' predicted growth and this job's predicted peak come to 1.0 GiB, above the memory budget of 1.0 GiB",
        ),
    );
}

#[test]
fn bf10_a_tiny_job_keeps_no_place_in_line() {
    let s = budget(3.0);
    let busy = [RunningJob {
        started_s: Some(NOW - 600.0),
        ..run("busy", 1.0, None)
    }];
    let inp = input(
        s,
        machine(),
        busy.to_vec(),
        vec![
            wait("head", 3.0, None, 400.0),
            wait("tiny", 1.0, Some(0.3), 10.0),
        ],
    );
    assert_eq!(
        blocked(&inp, "head"),
        hold(
            Cause::Cores,
            "predicted 1 of 3 cores are taken and this job needs 3"
        ),
    );
    assert_eq!(blocked(&inp, "tiny"), None);
}

#[test]
fn bf11_a_head_that_waited_exactly_the_limit_blocks_the_line() {
    let head = wait("head", 3.0, None, 300.0);
    let small = wait("small", 1.0, None, 5.0);
    let busy = [RunningJob {
        started_s: Some(NOW - 600.0),
        ..run("busy", 1.0, None)
    }];
    assert_eq!(
        back(&small, &[&head], &busy, &budget(3.0), &machine()),
        hold(
            Cause::Ahead,
            "job head has waited 5 min 0 s, past the backfill limit of 5 min 0 s, so no job starts before it",
        ),
    );
}
