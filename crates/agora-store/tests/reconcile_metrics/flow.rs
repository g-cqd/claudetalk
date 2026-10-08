//! Flow metrics over a scripted history at fixed instants, and retention (work_prune). Rules: SPEC section 3.2 (metrics.rs,
//! jobs.rs) and D17; every expected value is derived by hand in its comment.

use super::*;

// ---- flow metrics over the scripted history --------------------------------------------------------------------------

fn scripted_world() -> World {
    let mut w = world();
    let sys = system(&mut w);
    scripted_history(&mut w, &sys);
    w
}

#[test]
fn the_scripted_history_gives_the_hand_derived_flow_metrics() {
    let w = scripted_world();
    let report =
        w.s.metrics(at(100), None, None, Some(at(10)))
            .expect("metrics");
    // Done in the window: a (lead 10 h, cycle 30-23 = 7 h), b (lead 42-15 = 27 h, cycle 42-18 = 24 h), d (lead 66-50
    // = 16 h, cycle 66-55 = 11 h). Lead sorted [10, 16, 27] h: p50 16 h = 57600 s, p90 nearest rank ceil(2.7) = 3
    // gives 27 h = 97200 s. Cycle sorted [7, 11, 24] h: p50 11 h = 39600 s, p90 24 h = 86400 s.
    // Blocked clipped to [10, 100] h: b 35-38 = 3 h, c 90-100 (open, at now) = 10 h, f 8-12 clipped to 10-12 = 2 h,
    // e 5-6 is outside the window. Total 15 h = 54000 s over 3 cards: average 5 h = 18000 s.
    // WIP now: e and f ready (2), c blocked (1); a, b, d are done.
    // Jobs ended in [10, 100] h: j1 done, j2 failed, j3 done, j4 cancelled (no count); j0 ended at 2 h is outside.
    // Queue waits for jobs started in the window: j1 30 s, j2 90 s, j3 600 s, j4 10 s. Sorted [10, 30, 90, 600]:
    // p50 = (30 + 90) / 2 = 60 s, p90 nearest rank ceil(3.6) = 4 gives 600 s.
    let expected = MetricsReport {
        since_ms: at(10),
        until_ms: at(100),
        cards_done: 3,
        lead_time_p50_s: Some(57_600.0),
        lead_time_p90_s: Some(97_200.0),
        cycle_time_p50_s: Some(39_600.0),
        cycle_time_p90_s: Some(86_400.0),
        wip: wip_counts([
            ("backlog", 0),
            ("ready", 2),
            ("claimed", 0),
            ("doing", 0),
            ("review", 0),
            ("blocked", 1),
        ]),
        blocked_time_total_s: 54_000.0,
        blocked_time_avg_s: Some(18_000.0),
        jobs_done: 2,
        jobs_failed: 1,
        queue_wait_p50_s: Some(60.0),
        queue_wait_p90_s: Some(600.0),
    };
    assert_eq!(report, expected);
}

#[test]
fn a_team_filter_restricts_the_cards_but_not_the_job_counts() {
    let w = scripted_world();
    let core =
        w.s.metrics(at(100), Some("core"), None, Some(at(10)))
            .expect("metrics");
    // Team core is a, b, d, e, f. Blocked: b 3 h + f 2 h = 5 h = 18000 s over 2 cards (average 9000 s). WIP: e and
    // f ready, nothing blocked (c is ops). Cards done and every job figure are the same as the unfiltered report.
    assert_eq!(core.blocked_time_total_s, 18_000.0);
    assert_eq!(core.blocked_time_avg_s, Some(9_000.0));
    assert_eq!(core.wip["ready"], 2);
    assert_eq!(core.wip["blocked"], 0);
    assert_eq!(core.cards_done, 3);
    assert_eq!((core.jobs_done, core.jobs_failed), (2, 1));
    assert_eq!(
        (core.queue_wait_p50_s, core.queue_wait_p90_s),
        (Some(60.0), Some(600.0))
    );
}

#[test]
fn a_job_ended_exactly_at_the_window_start_is_counted_and_a_start_before_it_is_not() {
    let w = scripted_world();
    // The window is [26 h, 100 h]. j2 ended at exactly 26 h (counted as failed); it started at 25 h + 90 s, before the
    // window, so its queue wait is not sampled. j3 (done, started 50 h + 600 s) and j4 (cancelled, started 60 h + 10 s)
    // are inside. Queue waits sorted [10, 600]: p50 305 s, p90 nearest rank ceil(1.8) = 2 gives 600 s.
    let report =
        w.s.metrics(at(100), None, None, Some(at(26)))
            .expect("metrics");
    assert_eq!((report.jobs_done, report.jobs_failed), (1, 1));
    assert_eq!(
        (report.queue_wait_p50_s, report.queue_wait_p90_s),
        (Some(305.0), Some(600.0))
    );
}

// ---- retention (work_prune) ------------------------------------------------------------------------------------------

#[test]
fn work_prune_deletes_work_and_job_events_before_the_cutoff_and_keeps_the_rest() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    let cut = at(50);
    card_at(&mut w.s, &w.sup, "p1", "core", "alpha", at(0));
    request_job(&mut w.s, &w.alice, "old-job", None, at(0));
    start_job(&mut w.s, &sys, "old-job", at_s(0, 5));
    request_job(&mut w.s, &w.alice, "edge-job", None, cut);
    assert!(events_before(&scratch, "work.%", cut) + events_before(&scratch, "job.%", cut) > 0);
    let recent_before = work_events_from(&scratch, cut);
    w.s.work_prune(cut).expect("prune");
    assert_eq!(
        events_before(&scratch, "work.%", cut) + events_before(&scratch, "job.%", cut),
        0
    );
    assert_eq!(
        work_events_from(&scratch, cut),
        recent_before,
        "events at or after the cutoff stay"
    );
    assert_eq!(
        events_before(&scratch, "card.created", cut),
        1,
        "other kinds of event are kept"
    );
}

#[test]
fn work_prune_keeps_job_history_so_past_runs_still_inform_estimates() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "old-job", None, at(0));
    start_job(&mut w.s, &sys, "old-job", at_s(0, 5));
    finish_job(
        &mut w.s,
        &sys,
        "old-job",
        &finish_arg(Some(0), at(1)),
        at(1),
        None,
    );
    w.s.work_prune(at(50)).expect("prune");
    assert_eq!(ran_jobs(&w.s).len(), 1);
}

#[test]
fn work_prune_deletes_reconcile_marks_and_notices_older_than_the_cutoff() {
    let mut w = world();
    let sys = system(&mut w);
    for (key, ts, text) in [("old", at(0), "old text"), ("new", at(60), "new text")] {
        let action = ReconcileAction::Notice {
            key: key.into(),
            target: pk_hex(&w.alice),
            kind: "stale".into(),
            subject: "c1".into(),
            text: text.into(),
        };
        w.s.apply_reconcile(ts, &act(&sys, &action, ts))
            .expect("apply notice");
    }
    w.s.work_prune(at(50)).expect("prune");
    assert_eq!(
        input(&w, at(60)).marks,
        vec![Mark {
            key: "new".into(),
            last_ms: at(60),
            count: 1
        }]
    );
    assert_eq!(
        notices_of(&w, &w.alice),
        vec![("stale".into(), "c1".into(), "new text".into(), at(60))]
    );
}

#[test]
fn work_prune_returns_the_logs_of_ended_tasks_before_the_cutoff_once() {
    let mut w = world();
    let sys = system(&mut w);
    // Ended before the cutoff: returned. Ended at the cutoff: kept. Ended after it and still running: kept.
    request_job(&mut w.s, &w.alice, "old", None, at(0));
    start_job(&mut w.s, &sys, "old", at_s(0, 5));
    finish_job(
        &mut w.s,
        &sys,
        "old",
        &finish_arg(Some(0), at(10)),
        at(10),
        None,
    );
    request_job(&mut w.s, &w.alice, "edge", None, at(40));
    start_job(&mut w.s, &sys, "edge", at_s(40, 5));
    finish_job(
        &mut w.s,
        &sys,
        "edge",
        &finish_arg(Some(0), at(50)),
        at(50),
        None,
    );
    request_job(&mut w.s, &w.alice, "running", None, at(0));
    start_job(&mut w.s, &sys, "running", at_s(0, 5));
    assert_eq!(
        w.s.work_prune(at(50)).expect("prune"),
        vec!["old".to_string()]
    );
    assert_eq!(
        w.s.work_task("old").expect("task").log,
        None,
        "its log is forgotten"
    );
    assert!(w.s.work_prune(at(50)).expect("second prune").is_empty());
}
