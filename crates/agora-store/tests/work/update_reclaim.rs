use super::*;

// ---- update and assign -----------------------------------------------------------------------------------------

#[test]
fn a_supervisor_edits_a_task_it_did_not_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup) = (&mut w.s, &w.alice, &w.sup);
    queue(s, alice, "t1", json!({"title": "old"}));
    let arg = json!({"title": "new", "size": 3.0}).to_string();
    let edited = s
        .work_update(NOW, &op(sup, "work.update", "t1", &arg))
        .expect("update");
    assert_eq!((edited.title.as_str(), edited.size), ("new", 3.0));
}

#[test]
fn a_worker_cannot_edit_a_queued_task_it_did_not_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let err = s.work_update(NOW, &op(bob, "work.update", "t1", r#"{"title":"mine"}"#));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_supervisor_assigns_a_queued_task_by_claiming_it_for_the_identity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(bob)}).to_string();
    let assigned = s
        .work_update(NOW, &op(sup, "work.update", "t1", &arg))
        .expect("assign");
    assert_eq!(assigned.state, TaskState::Claimed);
    assert_eq!(assigned.agent, Some(pk_hex(bob)));
    assert_eq!(assigned.claimed_ms, Some(NOW));
}

#[test]
fn a_worker_cannot_assign_a_task_to_an_identity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(bob)}).to_string();
    let err = s.work_update(NOW, &op(alice, "work.update", "t1", &arg));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn assigning_to_an_unregistered_identity_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup, eve) = (&mut w.s, &w.alice, &w.sup, &w.eve);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(eve)}).to_string();
    let err = s.work_update(NOW, &op(sup, "work.update", "t1", &arg));
    assert_eq!(invalid(err), "agent is not an active worker identity");
}

#[test]
fn an_update_that_closes_a_prerequisite_cycle_is_refused_with_its_path() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"after": ["t1"]}));
    let arg = json!({"after": ["t2"]}).to_string();
    let msg = refusal(s.work_update(NOW, &op(alice, "work.update", "t1", &arg)));
    assert_eq!(msg, "the tasks form a cycle: t1 -> t2 -> t1");
}

#[test]
fn an_update_naming_an_unknown_prerequisite_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"after": ["ghost"]}).to_string();
    let msg = refusal(s.work_update(NOW, &op(alice, "work.update", "t1", &arg)));
    assert_eq!(msg, "task t1 is after unknown task ghost");
}

#[test]
fn a_claimant_unassigns_its_claimed_task_back_to_the_queue() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let unassigned = s
        .work_update(NOW, &op(bob, "work.update", "t1", r#"{"agent":null}"#))
        .expect("unassign");
    assert_eq!(
        (unassigned.state, unassigned.agent, unassigned.claimed_ms),
        (TaskState::Queued, None, None)
    );
}

// ---- reclaim ---------------------------------------------------------------------------------------------------

#[test]
fn a_supervisor_reclaims_a_claimed_task_to_the_queue() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    s.work_reclaim(
        NOW,
        &op(sup, "work.reclaim", "task:t1", r#"{"note":"stale"}"#),
    )
    .expect("reclaim");
    let t = s.work_task("t1").expect("task");
    assert_eq!(
        (t.state, t.agent, t.claimed_ms, t.note.as_str()),
        (TaskState::Queued, None, None, "stale")
    );
}

#[test]
fn a_worker_cannot_reclaim_a_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let err = s.work_reclaim(
        NOW,
        &op(bob, "work.reclaim", "task:t1", r#"{"note":"mine"}"#),
    );
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_task_that_was_requested_cannot_be_reclaimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    run(s, bob, "t1").expect("run");
    let holder = claimant_name(s, "t1");
    let msg = refusal(s.work_reclaim(NOW, &op(sup, "work.reclaim", "task:t1", "{}")));
    assert_eq!(
        msg,
        format!(
            "task t1 is waiting for capacity (claimed by {holder}); only a claimed task that has not been requested \
             can be reclaimed"
        )
    );
}

#[test]
fn a_supervisor_releases_a_path_lease_so_it_no_longer_holds_its_path() {
    let mut w = world();
    project(&mut w);
    let (s, bob, sup) = (&mut w.s, &w.bob, &w.sup);
    let lease = path_lease(s, bob, "/proj/docs", None);
    let subject = format!("lease:{}", lease.id);
    s.work_reclaim(
        NOW,
        &op(sup, "work.reclaim", &subject, r#"{"note":"stale"}"#),
    )
    .expect("release");
    assert!(s.work_holders(NOW, None).expect("holders").is_empty());
    assert!(
        s.leases(NOW, Some(LeaseKind::Path), None, true)
            .expect("leases")
            .is_empty()
    );
}
