//! Job lifecycle rules (job.start, job.finish, job.note refusals), the job_history rows they write, and the card sync
//! that rides in the same transaction as a finish. Rules: SPEC section 3.2 (jobs.rs) and D7 to D9.

use super::*;

// ---- job lifecycle -------------------------------------------------------------------------------------------------

#[test]
fn job_start_moves_a_waiting_job_to_running_at_its_start_time() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    let started = start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    assert_eq!(started.state, TaskState::Running);
    assert_eq!(started.started_ms, Some(at_s(1, 5)));
}

#[test]
fn job_start_refuses_a_job_that_is_already_running() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    let again = sys_op(
        &sys,
        "job.start",
        "j1",
        &json!({"pid": 4343, "supervisor_pid": 4200}),
        at_s(1, 6),
    );
    assert!(matches!(
        w.s.job_start(at_s(1, 6), &again),
        Err(StoreError::Invalid("job is not waiting to start"))
    ));
}

#[test]
fn a_job_that_exits_zero_is_done() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    let done = finish_job(
        &mut w.s,
        &sys,
        "j1",
        &finish_arg(Some(0), at(2)),
        at(2),
        None,
    );
    assert_eq!((done.state, done.status), (TaskState::Done, Some(0)));
}

#[test]
fn a_job_that_exits_non_zero_is_failed_with_its_status() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    let failed = finish_job(
        &mut w.s,
        &sys,
        "j1",
        &finish_arg(Some(3), at(2)),
        at(2),
        None,
    );
    assert_eq!((failed.state, failed.status), (TaskState::Failed, Some(3)));
}

#[test]
fn a_cancelled_running_job_ends_cancelled_with_the_status_it_reports() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    w.s.work_cancel(
        at_s(1, 6),
        &op_at(&w.sup, "work.cancel", "j1", "{}", at_s(1, 6)),
    )
    .expect("cancel running job");
    let ended = finish_job(
        &mut w.s,
        &sys,
        "j1",
        &finish_arg(Some(143), at(2)),
        at(2),
        None,
    );
    assert_eq!(
        (ended.state, ended.status),
        (TaskState::Cancelled, Some(143))
    );
}

#[test]
fn a_job_that_never_started_fails_without_a_history_row() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    let mut arg = finish_arg(None, at(2));
    arg["before_start"] = json!(true);
    arg["note"] = json!("the supervisor never reported");
    let failed = finish_job(&mut w.s, &sys, "j1", &arg, at(2), None);
    assert_eq!(failed.state, TaskState::Failed);
    assert_eq!(failed.started_ms, None);
    assert!(ran_jobs(&w.s).is_empty());
}

#[test]
fn a_job_that_is_not_running_cannot_be_finished() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    let early = sys_op(&sys, "job.finish", "j1", &finish_arg(Some(0), at(2)), at(2));
    assert!(matches!(
        w.s.job_finish(at(2), &early, None),
        Err(StoreError::Invalid("job is not running"))
    ));
}

#[test]
fn a_job_whose_command_never_ran_leaves_no_history_row() {
    let mut w = world();
    let sys = system(&mut w);
    for (id, status) in [("j126", 126), ("j127", 127)] {
        request_job(&mut w.s, &w.alice, id, None, at(1));
        start_job(&mut w.s, &sys, id, at_s(1, 5));
        let task = finish_job(
            &mut w.s,
            &sys,
            id,
            &finish_arg(Some(status), at(2)),
            at(2),
            None,
        );
        assert_eq!(
            (task.state, task.status),
            (TaskState::Failed, Some(status)),
            "status {status}"
        );
    }
    assert!(ran_jobs(&w.s).is_empty());
}

#[test]
fn a_finished_job_is_recorded_in_history_with_its_run_times_cpu_and_peak() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    let mut arg = finish_arg(Some(0), at(2));
    arg["cpu_seconds"] = json!(2.5);
    arg["rss_peak_kib"] = json!(1200);
    finish_job(&mut w.s, &sys, "j1", &arg, at(2), None);
    assert_eq!(
        ran_jobs(&w.s),
        vec![Run {
            started_ms: at_s(1, 5),
            ended_ms: at(2),
            cpu_seconds: 2.5,
            rss_peak_kib: Some(1200)
        }]
    );
}

#[test]
fn a_history_row_takes_the_peak_sampled_while_running_when_the_finish_has_none() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    w.s.job_sample("j1", 50.0, 800, 900, at_s(1, 30))
        .expect("sample");
    let task = finish_job(
        &mut w.s,
        &sys,
        "j1",
        &finish_arg(Some(0), at(2)),
        at(2),
        None,
    );
    assert_eq!(task.rss_peak_kib, Some(900));
    assert_eq!(ran_jobs(&w.s)[0].rss_peak_kib, Some(900));
}

#[test]
fn a_finish_peak_overrides_the_sampled_peak_in_history() {
    let mut w = world();
    let sys = system(&mut w);
    request_job(&mut w.s, &w.alice, "j1", None, at(1));
    start_job(&mut w.s, &sys, "j1", at_s(1, 5));
    w.s.job_sample("j1", 50.0, 800, 900, at_s(1, 30))
        .expect("sample");
    let mut arg = finish_arg(Some(0), at(2));
    arg["rss_peak_kib"] = json!(1200);
    finish_job(&mut w.s, &sys, "j1", &arg, at(2), None);
    assert_eq!(ran_jobs(&w.s)[0].rss_peak_kib, Some(1200));
}

#[test]
fn a_job_finish_moves_its_linked_card_to_review_and_records_the_job_as_evidence() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    card_at(&mut w.s, &w.sup, "k1", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "k1", "ready", at(0));
    move_card(&mut w.s, &w.alice, "k1", "claimed", at(1));
    move_card(&mut w.s, &w.alice, "k1", "doing", at(2));
    request_job(&mut w.s, &w.alice, "k1-run", Some("k1"), at(3));
    start_job(&mut w.s, &sys, "k1-run", at_s(3, 5));
    let sync_arg = json!({"task": "k1-run", "outcome": "done", "status": 0,
        "log": "/var/agora/logs/k1-run.log", "note": ""});
    let sync = sys_op(&sys, "card.job_sync", "k1-run", &sync_arg, at(4));
    let task = finish_job(
        &mut w.s,
        &sys,
        "k1-run",
        &finish_arg(Some(0), at(4)),
        at(4),
        Some(&sync),
    );
    assert_eq!(task.state, TaskState::Done);
    assert_eq!(w.s.card_state("k1").expect("state"), CardState::Review);
    assert_eq!(
        evidence_refs(&scratch, "k1"),
        vec!["job:k1-run status=0 log=/var/agora/logs/k1-run.log".to_string()]
    );
}

#[test]
fn a_card_sync_for_another_outcome_refuses_the_finish_and_the_job_keeps_running() {
    let mut w = world();
    let sys = system(&mut w);
    card_at(&mut w.s, &w.sup, "k2", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "k2", "ready", at(0));
    move_card(&mut w.s, &w.alice, "k2", "claimed", at(1));
    move_card(&mut w.s, &w.alice, "k2", "doing", at(2));
    request_job(&mut w.s, &w.alice, "k2-run", Some("k2"), at(3));
    start_job(&mut w.s, &sys, "k2-run", at_s(3, 5));
    let sync_arg =
        json!({"task": "k2-run", "outcome": "failed", "status": 0, "log": null, "note": ""});
    let sync = sys_op(&sys, "card.job_sync", "k2-run", &sync_arg, at(4));
    let finish = sys_op(
        &sys,
        "job.finish",
        "k2-run",
        &finish_arg(Some(0), at(4)),
        at(4),
    );
    assert!(matches!(
        w.s.job_finish(at(4), &finish, Some(&sync)),
        Err(StoreError::Invalid(
            "card.job_sync does not match the job it finishes"
        ))
    ));
    assert_eq!(
        w.s.work_task("k2-run").expect("task").state,
        TaskState::Running
    );
    assert_eq!(w.s.card_state("k2").expect("state"), CardState::Doing);
}
