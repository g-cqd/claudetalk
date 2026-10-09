use super::*;

// ---- acceptance (D5) -------------------------------------------------------------------------------------------

#[test]
fn accept_is_refused_for_a_task_without_a_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let msg = refusal(accept(s, bob, "t1", TOUCH_DIGEST));
    assert_eq!(msg, "task t1 has no command to accept");
}

#[test]
fn accept_is_refused_before_the_task_is_claimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    let msg = refusal(accept(s, bob, "t1", TOUCH_DIGEST));
    assert_eq!(
        msg,
        "task t1 is queued; accept a command after you claim its task"
    );
}

#[test]
fn accept_is_refused_for_a_task_claimed_by_someone_else() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(accept(s, carol, "t1", TOUCH_DIGEST));
    assert_eq!(
        msg,
        format!("task t1 is claimed by {holder}; only that agent accepts its command")
    );
}

#[test]
fn accept_refuses_a_digest_that_does_not_match_the_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let wrong = "0".repeat(64);
    let msg = refusal(accept(s, bob, "t1", &wrong));
    assert_eq!(
        msg,
        "the digest does not match the command of task t1 in its directory; read it with `agora work show t1`, \
         and accept it with the digest that show prints"
    );
}

#[test]
fn accepting_the_d5_digest_records_the_claimant_as_accepter() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let (task, first) = accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(bob)));
}

#[test]
fn accept_takes_the_digest_in_upper_case() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let (task, first) = accept(s, bob, "t1", &TOUCH_DIGEST.to_uppercase()).expect("accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(bob)));
}

#[test]
fn accepting_again_is_not_a_first_acceptance() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("first accept");
    let (_, first) = accept(s, bob, "t1", TOUCH_DIGEST).expect("second accept");
    assert!(!first);
}

#[test]
fn an_author_may_not_accept_their_own_command_by_default() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    claim(s, alice, "t1");
    let err = accept(s, alice, "t1", TOUCH_DIGEST);
    assert_eq!(
        invalid(err),
        "a command must be accepted by an identity other than its author"
    );
}

#[test]
fn an_author_may_accept_their_own_command_once_the_controller_allows_self_accept() {
    let mut w = world();
    project(&mut w);
    let ctl = controller(&mut w);
    let settings = json!({"patch": {"self_accept": "true"}}).to_string();
    w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &settings))
        .expect("settings");
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    claim(s, alice, "t1");
    let (task, first) = accept(s, alice, "t1", TOUCH_DIGEST).expect("self accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(alice)));
}

// ---- run requests ----------------------------------------------------------------------------------------------

#[test]
fn an_inline_run_creates_a_claimed_accepted_job_with_a_prediction_and_log_path() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let arg = json!({"add": {"command": ["touch", "/tmp/x"], "cwd": "/"}}).to_string();
    let job = s
        .work_request(NOW, &op(alice, "work.run", "", &arg), 4.0, LOG_DIR)
        .expect("inline run");
    assert_eq!(job.id, "job-1");
    assert_eq!(job.state, TaskState::Claimed);
    assert_eq!(job.agent, Some(pk_hex(alice)));
    assert_eq!(job.accepted_by, Some(pk_hex(alice)));
    assert_eq!(job.requested_ms, Some(NOW));
    assert_eq!(job.log.as_deref(), Some("/var/agora/logs/job-1.log"));
    assert_eq!(job.prediction.map(|p| p.level), Some(Level::None));
}

#[test]
fn a_queued_command_cannot_run_until_its_claimant_accepts_it() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let msg = refusal(run(s, bob, "t1"));
    assert_eq!(
        msg,
        format!(
            "task t1 runs a command that {} queued, and you have not accepted it: read it with `agora work show t1`, \
             then accept it with the `agora work accept t1 --digest ...` line that show prints",
            author_name(s, "t1")
        )
    );
}

#[test]
fn an_accepted_claimed_task_is_requested_with_its_log_path_and_waits_for_capacity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    let requested = run(s, bob, "t1").expect("run");
    assert_eq!(requested.requested_ms, Some(NOW));
    assert_eq!(requested.log.as_deref(), Some("/var/agora/logs/t1.log"));
    let waiting: Vec<String> = s
        .work_waiting()
        .expect("waiting")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(waiting, vec!["t1"]);
}

#[test]
fn a_run_names_exactly_one_of_a_task_or_a_command() {
    let mut w = world();
    let alice = &w.alice;
    let neither =
        w.s.work_request(NOW, &op(alice, "work.run", "", "{}"), 4.0, LOG_DIR);
    assert_eq!(
        invalid(neither),
        "name a task you claimed, or give a command to run"
    );
    let both = json!({"task": "t1", "add": {"command": ["true"], "cwd": "/"}}).to_string();
    let err =
        w.s.work_request(NOW, &op(alice, "work.run", "", &both), 4.0, LOG_DIR);
    assert_eq!(invalid(err), "give a task ID or a command, not both");
}

#[test]
fn a_queued_task_cannot_be_run_before_it_is_claimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    let msg = refusal(run(s, bob, "t1"));
    assert_eq!(msg, "claim task t1 first: `agora work claim t1`");
}
