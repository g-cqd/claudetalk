use super::*;

#[test]
fn start_marks_claimed_task_running() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");

    let task = start(&mut s, &system, "t1");

    assert_eq!(task.state, TaskState::Running);
    assert_eq!(task.started_ms, Some(NOW));
    assert_eq!(task.pid, Some(4242));
    assert_eq!(task.supervisor_pid, Some(4241));
    assert_eq!(task.qos.as_deref(), Some("utility"));
    assert_eq!(task.log.as_deref(), Some("/logs/t.log"));
    assert_eq!(task.note, "");
    assert_eq!(event_count(&s, "job.started", "t1"), 1);
}

#[test]
fn start_refuses_a_task_that_is_not_waiting() {
    let (mut s, system, _) = setup();
    s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state) VALUES ('queued', 'a', ?, 'queued')",
            [NOW],
        )
        .unwrap();
    let op = signed(&system, "job.start", "queued", &start_arg(), NOW);
    let err = s.job_start(NOW, &op).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("job is not waiting to start")
    ));

    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    let again = signed(&system, "job.start", "t1", &start_arg(), NOW + 1);
    let err = s.job_start(NOW + 1, &again).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("job is not waiting to start")
    ));
}

#[test]
fn finish_status_zero_is_done_and_records_history() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");

    let task = finish(&mut s, &system, "t1", Some(0), false).unwrap();

    assert_eq!(task.state, TaskState::Done);
    assert_eq!(task.status, Some(0));
    assert_eq!(task.ended_ms, Some(NOW + 5_000));
    assert_eq!(history_statuses(&s, "t1"), [Some(0)]);
    assert_eq!(event_count(&s, "job.finished", "t1"), 1);
}

#[test]
fn finish_nonzero_status_is_failed_and_records_history() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");

    let task = finish(&mut s, &system, "t1", Some(3), false).unwrap();

    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.status, Some(3));
    assert_eq!(history_statuses(&s, "t1"), [Some(3)]);
}

#[test]
fn finish_with_cancel_requested_is_cancelled() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    s.conn
        .execute("UPDATE work_tasks SET cancel_requested=1 WHERE id='t1'", [])
        .unwrap();

    let task = finish(&mut s, &system, "t1", Some(143), false).unwrap();

    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.status, Some(143));
    assert_eq!(history_statuses(&s, "t1"), [Some(143)]);
}

#[test]
fn finish_before_start_is_failed_without_history() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");

    let task = finish(&mut s, &system, "t1", None, true).unwrap();

    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.started_ms, None);
    assert_eq!(task.ended_ms, Some(NOW + 5_000));
    assert!(history_statuses(&s, "t1").is_empty());
}

#[test]
fn finish_with_status_126_or_127_leaves_no_history() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    insert_waiting(&s, "t2");
    start(&mut s, &system, "t1");
    start(&mut s, &system, "t2");

    let missing = finish(&mut s, &system, "t1", Some(127), false).unwrap();
    let not_exec = finish(&mut s, &system, "t2", Some(126), false).unwrap();

    assert_eq!(missing.state, TaskState::Failed);
    assert_eq!(not_exec.state, TaskState::Failed);
    assert!(history_statuses(&s, "t1").is_empty());
    assert!(history_statuses(&s, "t2").is_empty());
}

#[test]
fn finish_refuses_a_task_that_is_not_running() {
    let (mut s, system, _) = setup();
    s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state) VALUES ('queued', 'a', ?, 'queued')",
            [NOW],
        )
        .unwrap();
    insert_waiting(&s, "t1");

    let err = finish(&mut s, &system, "queued", Some(0), true).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("job is not running")));
    // Claimed and requested, but before_start is not set: the job never ran, so this is not a finish.
    let err = finish(&mut s, &system, "t1", Some(0), false).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("job is not running")));
    assert_eq!(state_of(&s, "t1"), "claimed");
}

#[test]
fn worker_signed_job_finish_is_forbidden() {
    let (mut s, system, worker) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    let op = signed(
        &worker,
        "job.finish",
        "t1",
        &finish_arg(Some(0), false),
        NOW + 5_000,
    );

    let err = s.job_finish(NOW + 5_000, &op, None).unwrap_err();

    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(_))
    ));
    assert_eq!(state_of(&s, "t1"), "running");
}

#[test]
fn failed_card_sync_rolls_back_the_finish() {
    let (mut s, system, worker) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    // A refused sync in the same transaction must take the job finish down with it.
    let sync = signed(&worker, "card.job_sync", "c1", "{}", NOW + 5_000);
    let arg = finish_arg(Some(0), false);
    let op = signed(&system, "job.finish", "t1", &arg, NOW + 5_000);

    assert!(s.job_finish(NOW + 5_000, &op, Some(&sync)).is_err());

    assert_eq!(state_of(&s, "t1"), "running");
    assert!(history_statuses(&s, "t1").is_empty());
    assert_eq!(event_count(&s, "job.finished", "t1"), 0);
}

#[test]
fn finish_without_a_peak_keeps_the_sampled_one() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    s.job_sample("t1", 1.0, 100, 4096, NOW + 1_000).unwrap();
    let arg = serde_json::to_string(&JobFinishArgs {
        status: Some(0),
        note: String::new(),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: NOW + 5_000,
        before_start: false,
    })
    .unwrap();

    s.job_finish(
        NOW + 5_000,
        &signed(&system, "job.finish", "t1", &arg, NOW + 5_000),
        None,
    )
    .unwrap();

    let peak: Option<i64> = s
        .conn
        .query_row("SELECT rss_peak FROM work_tasks WHERE id='t1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(peak, Some(4096));
}

#[test]
fn finish_without_a_peak_records_the_sampled_one_in_history() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    s.job_sample("t1", 1.0, 100, 4096, NOW + 1_000).unwrap();
    let arg = serde_json::to_string(&JobFinishArgs {
        status: Some(0),
        note: String::new(),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: NOW + 5_000,
        before_start: false,
    })
    .unwrap();

    s.job_finish(
        NOW + 5_000,
        &signed(&system, "job.finish", "t1", &arg, NOW + 5_000),
        None,
    )
    .unwrap();

    let peak: Option<i64> = s
        .conn
        .query_row(
            "SELECT rss_peak FROM job_history WHERE task='t1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(peak, Some(4096));
}
