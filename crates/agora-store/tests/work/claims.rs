use super::*;

// ---- claims ----------------------------------------------------------------------------------------------------

#[test]
fn a_claim_waits_while_a_prerequisite_is_not_done_and_names_it() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"after": ["t1"]}));
    let waiting = refusal(s.work_claim(NOW, &op(bob, "work.claim", "t2", "{}")));
    assert_eq!(waiting, "task t2 waits for t1 (queued)");
    finish(s, alice, "t1", "done", "").expect("close t1");
    let t2 = claim(s, bob, "t2");
    assert_eq!(t2.agent, Some(pk_hex(bob)));
}

#[test]
fn a_claim_is_refused_when_a_claimed_task_holds_its_files_and_names_the_holder() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src"]}));
    claim(s, bob, "t0");
    queue(s, alice, "t1", json!({"files": ["/proj/src/lib.rs"]}));
    let holder = claimant_name(s, "t0");
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(
        msg,
        format!("task t1 changes files that task t0 holds (claimed by {holder})")
    );
}

#[test]
fn a_claim_is_refused_when_a_path_lease_of_another_identity_covers_its_files() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    let lease = path_lease(s, bob, "/proj/docs", None);
    queue(s, alice, "t1", json!({"files": ["/proj/docs/readme.md"]}));
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(
        msg,
        format!("task t1 changes files that lease:{} holds", lease.id)
    );
}

#[test]
fn a_claimant_does_not_conflict_with_its_own_path_lease() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    path_lease(s, bob, "/proj/docs", None);
    queue(s, alice, "t1", json!({"files": ["/proj/docs/readme.md"]}));
    let t1 = claim(s, bob, "t1");
    assert_eq!(
        (t1.state, t1.agent),
        (TaskState::Claimed, Some(pk_hex(bob)))
    );
}

#[test]
fn a_task_already_claimed_is_refused_and_names_its_claimant() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(msg, format!("task t1 is claimed by {holder}"));
}

#[test]
fn claiming_an_unknown_task_is_not_found() {
    let mut w = world();
    let bob = &w.bob;
    let err = w.s.work_claim(NOW, &op(bob, "work.claim", "ghost", "{}"));
    assert_eq!(not_found(err), "task ghost");
}

// ---- next ------------------------------------------------------------------------------------------------------

#[test]
fn next_claims_queued_tasks_in_arrival_order() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    queue_at(s, alice, "t3", NOW + 2, json!({}));
    let mut order = Vec::new();
    for step in 0..3 {
        let claimed = next_task(s, bob, Some(PROJ), NOW + 3 + step);
        order.push(claimed.claimed.expect("a claim").id);
    }
    assert_eq!(order, vec!["t1", "t2", "t3"]);
}

#[test]
fn next_puts_a_validity_task_before_an_earlier_ordinary_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({"kind": "feature"}));
    queue_at(s, alice, "t2", NOW + 1, json!({"kind": "security"}));
    let claimed = next_task(s, bob, Some(PROJ), NOW + 2);
    assert_eq!(claimed.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_skips_a_task_waiting_for_an_unfinished_prerequisite() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t0", NOW, json!({}));
    queue_at(s, alice, "t1", NOW + 1, json!({"after": ["t0"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({}));
    let first = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(first.claimed.map(|t| t.id), Some("t0".to_string()));
    let second = next_task(s, bob, Some(PROJ), NOW + 4);
    assert_eq!(second.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_skips_a_task_whose_files_a_claimed_task_holds() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src/main.rs"]}));
    claim(s, carol, "t0");
    queue_at(s, alice, "t1", NOW + 1, json!({"files": ["/proj/src"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({}));
    let claimed = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(claimed.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_in_a_scope_with_nothing_queued_names_the_scope() {
    let mut w = world();
    project(&mut w);
    scope(&mut w.s, &w.alice, "other", None);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"scope": "other"}));
    let outcome = next_task(s, bob, Some(PROJ), NOW + 1);
    assert_eq!(outcome.claimed, None);
    assert_eq!(outcome.reason, "nothing is queued in scope proj");
}

#[test]
fn next_without_a_scope_and_an_empty_queue_says_nothing_is_queued() {
    let mut w = world();
    let bob = &w.bob;
    let outcome =
        w.s.work_next(NOW, &op(bob, "work.next", "", "{}"))
            .expect("next");
    assert_eq!(outcome.claimed, None);
    assert_eq!(outcome.reason, "nothing is queued");
}

#[test]
fn next_reports_how_many_queued_tasks_wait_or_are_held_when_nothing_is_claimable() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src/main.rs"]}));
    claim(s, carol, "t0");
    queue_at(s, alice, "t1", NOW + 1, json!({"files": ["/proj/src"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({"after": ["t0"]}));
    let outcome = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(outcome.claimed, None);
    assert_eq!(
        outcome.reason,
        "of 2 queued task(s) in scope proj, 1 wait for their prerequisites and 1 change files that claimed or \
         running tasks hold"
    );
}

#[test]
fn next_refuses_an_unknown_scope() {
    let mut w = world();
    let bob = &w.bob;
    let msg = refusal(w.s.work_next(NOW, &op(bob, "work.next", "", r#"{"scope":"nope"}"#)));
    assert_eq!(
        msg,
        "unknown scope nope; create it with `agora work scope set nope --kind project`"
    );
}
