use super::*;

// ---- finish ----------------------------------------------------------------------------------------------------

#[test]
fn the_claimant_closes_a_task_without_a_command_as_done_or_failed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({}));
    claim(s, bob, "t1");
    claim(s, bob, "t2");
    for (id, state) in [("t1", TaskState::Done), ("t2", TaskState::Failed)] {
        let closed = finish(s, bob, id, state.as_str(), "ok").expect("finish");
        assert_eq!((closed.state, closed.note.as_str()), (state, "ok"));
        assert_eq!(closed.ended_ms, Some(NOW));
    }
}

#[test]
fn the_author_closes_its_own_queued_task_without_a_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let closed = finish(s, alice, "t1", "done", "").expect("finish");
    assert_eq!(closed.state, TaskState::Done);
}

#[test]
fn a_task_with_a_command_cannot_be_closed_by_hand() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    let msg = refusal(finish(s, alice, "t1", "done", ""));
    assert_eq!(
        msg,
        "task t1 has a command: run it with `agora work run t1`, or cancel it"
    );
}

#[test]
fn a_queued_task_is_closed_only_by_its_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let msg = refusal(finish(s, bob, "t1", "done", ""));
    assert_eq!(
        msg,
        format!(
            "task t1 was queued by {name}; until an agent claims it, only {name} closes it",
            name = author_name(s, "t1")
        )
    );
}

#[test]
fn a_claimed_task_is_closed_only_by_its_claimant() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(finish(s, carol, "t1", "done", ""));
    assert_eq!(
        msg,
        format!("task t1 is claimed by {holder}; only that agent closes it")
    );
}

#[test]
fn a_final_task_cannot_be_closed_again() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    finish(s, alice, "t1", "done", "").expect("first close");
    let msg = refusal(finish(s, alice, "t1", "failed", ""));
    assert_eq!(msg, "task t1 is done already");
}

// ---- cancel ----------------------------------------------------------------------------------------------------

#[test]
fn the_author_cancels_its_queued_task_as_cancelled() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let outcome = s
        .work_cancel(
            NOW,
            &op(alice, "work.cancel", "t1", r#"{"note":"dropped"}"#),
        )
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
    assert_eq!(outcome.task.note, "dropped");
    assert_eq!(outcome.stop, None);
}

#[test]
fn the_claimant_cancels_its_claimed_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let outcome = s
        .work_cancel(NOW, &op(bob, "work.cancel", "t1", "{}"))
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
}

#[test]
fn a_worker_cannot_cancel_a_task_claimed_by_someone_else() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let err = s.work_cancel(NOW, &op(carol, "work.cancel", "t1", "{}"));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_supervisor_cancels_any_queued_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup) = (&mut w.s, &w.alice, &w.sup);
    queue(s, alice, "t1", json!({}));
    let outcome = s
        .work_cancel(NOW, &op(sup, "work.cancel", "t1", "{}"))
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
}

#[test]
fn cancelling_a_running_task_requests_a_stop_and_keeps_its_state() {
    let mut w = world();
    project(&mut w);
    let sys = system_identity(&mut w.s);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    run(s, bob, "t1").expect("run");
    start_job(s, &sys, "t1");
    let outcome = s
        .work_cancel(NOW, &op(bob, "work.cancel", "t1", r#"{"note":"stop"}"#))
        .expect("cancel running");
    assert_eq!(outcome.task.state, TaskState::Running);
    assert!(outcome.task.cancel_requested);
    assert_eq!(
        outcome.stop,
        Some(StopTarget {
            supervisor_pid: 4200,
            supervisor_start_s: Some(1700),
            job_dir: String::new(),
        })
    );
    assert!(s.work_task("t1").expect("task").cancel_requested);
}

#[test]
fn a_final_task_cannot_be_cancelled() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    s.work_cancel(NOW, &op(alice, "work.cancel", "t1", "{}"))
        .expect("first cancel");
    let msg = refusal(s.work_cancel(NOW, &op(alice, "work.cancel", "t1", "{}")));
    assert_eq!(msg, "task t1 is cancelled already");
}
