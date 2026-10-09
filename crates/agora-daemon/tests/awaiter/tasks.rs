use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_ends_with_status_zero_and_the_final_task_once_its_job_is_done() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(&mut w.owner, d.home.path(), &["sleep", "0.5"]).await;

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id.clone()], false),
        WAIT_MS,
        None,
    )
    .await;

    assert_eq!(result.condition, "tasks");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0, "tasks: {:?}", result.tasks);
    assert_eq!(result.targets, vec![task.id.clone()]);
    assert!(result.pending.is_empty());
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].id, task.id);
    assert_eq!(result.tasks[0].state, TaskState::Done);
    assert_eq!(result.tasks[0].status, Some(0));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_any_tasks_wait_returns_when_the_first_of_two_jobs_ends() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let fast = run_inline(&mut w.owner, d.home.path(), &["sleep", "0.3"]).await;
    let slow = run_inline(&mut w.owner, d.home.path(), &["sleep", "30"]).await;

    let ids = vec![fast.id.clone(), slow.id.clone()];
    let result = wait_on(&mut w.owner, tasks_spec(ids, true), WAIT_MS, None).await;

    assert!(!result.timed_out, "the first job ended, so the wait ends");
    assert_eq!(result.status, 0);
    assert_eq!(result.tasks.len(), 2);
    assert_eq!(result.tasks[0].state, TaskState::Done);
    assert!(
        !result.tasks[1].state.is_final(),
        "the second job still runs when the wait returns"
    );
    cancel_and_end(&mut w.owner, &slow.id).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_tail_holds_the_last_lines_of_the_first_final_tasks_log() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo line-one; echo line-two"],
    )
    .await;

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id], false),
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(
        result.tail,
        vec!["line-one".to_string(), "line-two".to_string()]
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_tasks_wait_tail_holds_the_log_of_the_first_task_that_has_one() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo started; sleep 30"],
    )
    .await;
    // The log matches once the job has written its first line, so the timed-out wait below cannot race the echo.
    let log = jobdir::log_path(&jobdir::work_root(d.home.path()), &task.id);
    let started = wait_on(
        &mut w.owner,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^started$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert!(!started.timed_out, "the job wrote its first line");

    let result = wait_on(
        &mut w.owner,
        tasks_spec(vec![task.id.clone()], false),
        300,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.pending, vec![task.id.clone()]);
    assert_eq!(result.tail, vec!["started".to_string()]);
    cancel_and_end(&mut w.owner, &task.id).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_that_neither_authored_nor_claimed_a_task_gets_its_status_but_no_log_tail() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let task = run_inline(
        &mut w.owner,
        d.home.path(),
        &["sh", "-c", "echo owner-only"],
    )
    .await;

    let result = wait_on(&mut w.bob, tasks_spec(vec![task.id], false), WAIT_MS, None).await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert!(
        result.tail.is_empty(),
        "the tail is the author's and claimant's to read"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_all_wait_waits_for_the_jobs_that_were_running_when_it_was_called() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let short = run_inline(&mut w.owner, d.home.path(), &["sleep", "1"]).await;
    let long = run_inline(&mut w.owner, d.home.path(), &["sleep", "2"]).await;

    let result = wait_on(&mut w.owner, WaitSpec::All { scope: None }, WAIT_MS, None).await;

    assert_eq!(result.condition, "all");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    let mut targets = result.targets.clone();
    targets.sort();
    let mut expected = vec![short.id.clone(), long.id.clone()];
    expected.sort();
    assert_eq!(targets, expected);
    assert_eq!(result.tasks.len(), 2);
    assert!(result.tasks.iter().all(|t| t.state == TaskState::Done));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_all_wait_with_no_task_in_scope_is_done_at_once() {
    let d = daemon().await;
    let mut w = world(&d).await;

    let result = wait_on(&mut w.owner, WaitSpec::All { scope: None }, 10_000, None).await;

    assert_eq!(result.condition, "all");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert!(result.targets.is_empty());
    assert!(result.tasks.is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tasks_wait_on_an_unknown_task_is_not_found() {
    let d = daemon().await;
    let mut w = world(&d).await;

    let error = wait_result(
        &mut w.owner,
        tasks_spec(vec!["no-such-task".to_string()], false),
        1_000,
        None,
    )
    .await
    .expect_err("an unknown task is refused");

    assert_eq!(error.code, ErrorCode::NotFound);
    d.stop().await;
}
