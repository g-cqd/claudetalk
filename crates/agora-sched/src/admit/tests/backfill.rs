use super::*;

#[test]
fn shorter_predicted_jobs_go_first_and_ties_go_by_request() {
    let long = |req: f64| queued("long", "work", 1, req, Some(600.0));
    assert_eq!(
        order(
            vec![long(0.0), queued("short", "work", 2, 100.0, Some(30.0))],
            1.0
        ),
        ["short", "long"]
    );
    assert_eq!(
        order(
            vec![long(0.0), queued("late", "work", 2, 580.0, Some(30.0))],
            1.0
        ),
        ["long", "late"]
    );
    assert_eq!(
        order(
            vec![long(0.0), queued("short", "work", 2, 570.0, Some(30.0))],
            1.0
        ),
        ["long", "short"]
    );
    assert_eq!(
        order(
            vec![
                queued("later", "work", 5, 0.0, Some(30.0)),
                queued("earlier", "work", 3, 0.0, Some(30.0))
            ],
            1.0
        ),
        ["earlier", "later"]
    );
}

#[test]
fn validity_work_goes_first_and_an_unknown_duration_counts_as_thirty() {
    assert_eq!(
        order(
            vec![
                queued("short", "work", 1, 50.0, Some(1.0)),
                queued("fix", "validity", 2, 60.0, Some(900.0))
            ],
            1.0
        ),
        ["fix", "short"]
    );
    assert_eq!(
        order(
            vec![
                queued("short", "work", 1, 50.0, Some(1.0)),
                queued("fix", "Validity", 2, 60.0, Some(900.0))
            ],
            1.0
        ),
        ["fix", "short"]
    );
    assert_eq!(
        order(
            vec![
                queued("known", "work", 1, 0.0, Some(60.0)),
                queued("unknown", "work", 0, 0.0, None)
            ],
            1.0
        ),
        ["unknown", "known"]
    );
    assert_eq!(
        order(
            vec![
                queued("unknown", "work", 0, 0.0, None),
                queued("known", "work", 1, 0.0, Some(10.0))
            ],
            1.0
        ),
        ["known", "unknown"]
    );
}

#[test]
fn aging_bounds_how_long_a_long_job_waits() {
    let long = queued("long", "work", 1, 0.0, Some(600.0));
    let short_at = |req: f64| queued("short", "work", 2, req, Some(30.0));
    assert_eq!(
        order(vec![long.clone(), short_at(9_000.0)], 0.0),
        ["short", "long"]
    );
    assert_eq!(
        order(vec![long.clone(), short_at(1139.0)], 0.5),
        ["short", "long"]
    );
    assert_eq!(
        order(vec![long.clone(), short_at(1140.0)], 0.5),
        ["long", "short"]
    );
    assert_eq!(order(vec![long, short_at(1141.0)], 0.5), ["long", "short"]);
}

#[test]
fn a_backfill_job_that_ends_before_the_head_can_start_goes_ahead() {
    let b = Budgets {
        budget: 3.0,
        ..fixture()
    };
    let s = WorkSettings {
        small_weight: 0.1,
        ..WorkSettings::default()
    };
    let running = [RunningJob {
        started_s: Some(NOW - 5.0),
        pred_duration_p90_s: Some(100.0),
        ..run("R", 2.0, Some(2.0))
    }];
    let head = job("head", 2.0, Some(2.0), 10.0);
    let ahead = [&head];
    let mut cand = WaitingJob {
        pred_duration_p90_s: Some(90.0),
        ..job("cand", 0.5, Some(0.5), 1.0)
    };
    assert!(backfill(&cand, &ahead, &running, &s, &b, Some(&calm()), NOW).is_none());
    cand.pred_duration_p90_s = Some(110.0);
    assert_eq!(
        text(backfill(
            &cand,
            &ahead,
            &running,
            &s,
            &b,
            Some(&calm()),
            NOW
        )),
        "job head is ahead of it and needs 2 of the 3 cores, which leaves 0 for jobs behind it; this job needs 0.5, 1 min 50 s at p90, and the head may start in 1 min 35 s"
    );
    cand.pred_duration_p90_s = None;
    assert_eq!(
        text(backfill(
            &cand,
            &ahead,
            &running,
            &s,
            &b,
            Some(&calm()),
            NOW
        )),
        "job head is ahead of it and needs 2 of the 3 cores, which leaves 0 for jobs behind it; this job needs 0.5, and the head may start in 1 min 35 s"
    );
    let finished = [RunningJob {
        started_s: Some(NOW - 101.0),
        pred_duration_p90_s: Some(100.0),
        ..run("R", 2.0, Some(2.0))
    }];
    let late_head = job("head", 2.0, Some(2.0), 150.0);
    let late_ahead = [&late_head];
    let quick = WaitingJob {
        pred_duration_p90_s: Some(90.0),
        ..job("cand", 0.5, Some(0.5), 1.0)
    };
    let reason = text(backfill(
        &quick,
        &late_ahead,
        &finished,
        &s,
        &b,
        Some(&calm()),
        NOW,
    ));
    assert!(
        reason.ends_with("and the head may start any moment"),
        "{reason}"
    );
}

#[test]
fn head_start_counts_each_running_end_in_order() {
    let b = Budgets {
        budget: 6.0,
        ..fixture()
    };
    let running = [
        RunningJob {
            started_s: Some(NOW - 50.0),
            pred_duration_p90_s: Some(60.0),
            ..run("A", 2.0, Some(2.0))
        },
        RunningJob {
            pred_duration_p90_s: None,
            ..run("B", 1.0, Some(1.0))
        },
        RunningJob {
            pred_duration_p90_s: Some(200.0),
            ..run("C", 1.0, Some(1.0))
        },
        RunningJob {
            started_s: None,
            ..run("D", 1.0, Some(1.0))
        },
    ];
    assert_eq!(head_start_estimate(4.0, &running, &b, NOW), NOW + 10.0);
    assert_eq!(head_start_estimate(5.0, &running, &b, NOW), NOW + 10.0);
    assert_eq!(head_start_estimate(6.0, &running, &b, NOW), NOW + 200.0);
    assert_eq!(head_start_estimate(1.0, &[], &b, NOW), NOW);
}

#[test]
fn a_job_that_would_take_the_heads_cores_waits_and_a_light_one_starts() {
    let b = Budgets {
        budget: 5.0,
        ..fixture()
    };
    let s = WorkSettings::default();
    let busy = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 2.0, None)
    }];
    let head = job("head", 4.0, None, 10.0);
    let ahead = [&head];
    let medium = job("medium", 2.0, None, 5.0);
    assert_eq!(
        text(backfill(&medium, &ahead, &busy, &s, &b, Some(&calm()), NOW)),
        "job head is ahead of it and needs 4 of the 5 cores, which leaves 1 for jobs behind it; this job needs 2, and the head may start any moment"
    );
    let light = job("light", 1.0, None, 4.0);
    assert!(backfill(&light, &ahead, &busy, &s, &b, Some(&calm()), NOW).is_none());
}

#[test]
fn jobs_that_started_while_the_head_waited_keep_their_cores() {
    let b = Budgets {
        budget: 5.0,
        ..fixture()
    };
    let s = WorkSettings {
        small_weight: 0.5,
        ..WorkSettings::default()
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
    let head = job("head", 4.0, None, 30.0);
    let ahead = [&head];
    let second = job("second", 1.0, None, 5.0);
    let reason = text(backfill(
        &second,
        &ahead,
        &running,
        &s,
        &b,
        Some(&calm()),
        NOW,
    ));
    assert!(reason.contains("leaves 0 for jobs behind it"), "{reason}");
}

#[test]
fn a_head_that_waited_past_the_limit_holds_every_job_behind_it() {
    let b = Budgets {
        budget: 3.0,
        ..fixture()
    };
    let s = WorkSettings::default();
    let busy = [RunningJob {
        started_s: Some(NOW - 600.0),
        ..run("busy", 1.0, None)
    }];
    let small = job("small", 1.0, None, 5.0);
    let head = job("head", 3.0, None, 10.0);
    let ahead = [&head];
    assert!(backfill(&small, &ahead, &busy, &s, &b, Some(&calm()), NOW).is_none());
    let stale = job("head", 3.0, None, 400.0);
    let stale_ahead = [&stale];
    assert_eq!(
        text(backfill(
            &small,
            &stale_ahead,
            &busy,
            &s,
            &b,
            Some(&calm()),
            NOW
        )),
        "job head has waited 6 min 40 s, past the backfill limit of 5 min 0 s, so no job starts before it"
    );
    let exact = job("head", 3.0, None, 300.0);
    let exact_ahead = [&exact];
    assert!(
        text(backfill(
            &small,
            &exact_ahead,
            &busy,
            &s,
            &b,
            Some(&calm()),
            NOW
        ))
        .starts_with("job head has waited 5 min 0 s, past the backfill limit of 5 min 0 s")
    );
    assert_eq!(
        text(backfill(&small, &ahead, &[], &s, &b, Some(&calm()), NOW)),
        "job head is ahead of it, and there is room for it"
    );
}

#[test]
fn a_validity_head_keeps_its_place_from_small_jobs() {
    let b = Budgets {
        budget: 3.0,
        ..fixture()
    };
    let s = WorkSettings::default();
    let busy = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 1.0, None)
    }];
    let fix = WaitingJob {
        kind: "validity".to_owned(),
        ..job("fix", 3.0, None, 10.0)
    };
    let fix_ahead = [&fix];
    let small = job("small", 1.0, None, 5.0);
    assert_eq!(
        text(backfill(
            &small,
            &fix_ahead,
            &busy,
            &s,
            &b,
            Some(&calm()),
            NOW
        )),
        "job fix is ahead of it and needs 3 of the 3 cores, which leaves 0 for jobs behind it; this job needs 1, and the head may start any moment, and job fix, validity work, keeps its place"
    );
    let roomy = Budgets {
        budget: 5.0,
        ..fixture()
    };
    let busy_three = [RunningJob {
        started_s: Some(NOW - 60.0),
        ..run("busy", 3.0, None)
    }];
    assert!(
        backfill(
            &small,
            &fix_ahead,
            &busy_three,
            &s,
            &roomy,
            Some(&calm()),
            NOW
        )
        .is_none()
    );
}

#[test]
fn a_head_that_fits_the_cores_holds_for_memory_and_a_tiny_job_keeps_no_place() {
    let b = Budgets {
        budget: 3.0,
        memory_budget_kib: Some(1_000_000),
        ..fixture()
    };
    let s = WorkSettings::default();
    let mem = Signals {
        memory_used_kib: Some(900_000),
        ..calm()
    };
    let reserved = [RunningJob {
        rss_kib: Some(0),
        ..run("R", 1.0, Some(1.0))
    }];
    let big_head = WaitingJob {
        pred_rss_p90_kib: Some(200_000),
        ..job("head", 2.0, Some(2.0), 10.0)
    };
    let head_ahead = [&big_head];
    let small = job("small", 1.0, Some(1.0), 5.0);
    assert_eq!(
        text(backfill(
            &small,
            &head_ahead,
            &reserved,
            &s,
            &b,
            Some(&mem),
            NOW
        )),
        "job head is ahead of it and waits for memory: used memory, the running jobs' predicted growth and this job's predicted peak come to 1.0 GiB, above the memory budget of 1.0 GiB"
    );
    let busy = [RunningJob {
        started_s: Some(NOW - 600.0),
        ..run("busy", 1.0, None)
    }];
    let head = job("head", 3.0, None, 400.0);
    assert_eq!(
        text(admission(&head, &busy, &s, &b, Some(&calm()), NOW)),
        "predicted 1 of 3 cores are taken and this job needs 3"
    );
    let tiny = job("tiny", 1.0, Some(0.3), 5.0);
    assert!(admission(&tiny, &busy, &s, &b, Some(&calm()), NOW).is_none());
    assert!(backfill(&tiny, &[], &busy, &s, &b, Some(&calm()), NOW).is_none());
}
