//! apply_reconcile: one effect per action kind, the idempotency of a repeated key, and who may sign a reconcile op.
//! Rules: SPEC section 6 (apply semantics), D6 (system identity), decisions R3-10, R3-14/O-16 and O-36.

use super::*;

// ---- apply_reconcile: one effect per action kind ------------------------------------------------------------------

#[test]
fn an_expire_action_releases_nothing_before_the_lease_deadline() {
    let mut w = world();
    let sys = system(&mut w);
    let lease = path_lease(&mut w, "/work/a", 60.0);
    let deadline = lease.expires_ms.expect("a path lease expires");
    let before = deadline - 1;
    let action = ReconcileAction::ExpireLease {
        key: format!("expire:{}:{deadline}", lease.id),
        lease: lease.id.clone(),
    };
    assert!(
        w.s.apply_reconcile(before, &act(&sys, &action, before))
            .expect("apply")
    );
    assert_eq!(released(&w.s, &lease.id, before), None);
}

#[test]
fn an_expire_action_at_the_deadline_releases_a_path_lease_and_a_pool_lease() {
    let mut w = world();
    let sys = system(&mut w);
    let ctl = controller(&mut w);
    w.s.pool_define(
        NOW,
        &op(
            &ctl,
            "pool.define",
            "gpu",
            r#"{"name":"gpu","capacity":2,"unit":"slot"}"#,
        ),
    )
    .expect("define pool");
    let path = path_lease(&mut w, "/work/a", 60.0);
    let pool_arg =
        json!({"kind": "pool", "resource": "gpu", "amount": 1.0, "ttl_s": 60.0}).to_string();
    let pool =
        w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &pool_arg))
            .expect("acquire pool lease");
    let deadline = path.expires_ms.expect("a path lease expires");
    for lease in [&path, &pool] {
        let action = ReconcileAction::ExpireLease {
            key: format!("expire:{}:{deadline}", lease.id),
            lease: lease.id.clone(),
        };
        assert!(
            w.s.apply_reconcile(deadline, &act(&sys, &action, deadline))
                .expect("apply")
        );
    }
    assert_eq!(released(&w.s, &path.id, deadline), Some(deadline));
    assert_eq!(released(&w.s, &pool.id, deadline), Some(deadline));
}

#[test]
fn an_expire_action_never_releases_a_quiet_window() {
    let mut w = world();
    let sys = system(&mut w);
    let ctl = controller(&mut w);
    let quiet = quiet_window(&mut w, &ctl, NOW + 3_600_000);
    let later = NOW + 7_200_000;
    let action = ReconcileAction::ExpireLease {
        key: format!("expire:{}:{later}", quiet.id),
        lease: quiet.id.clone(),
    };
    assert!(
        w.s.apply_reconcile(later, &act(&sys, &action, later))
            .expect("apply")
    );
    assert_eq!(released(&w.s, &quiet.id, later), None);
}

#[test]
fn a_close_quiet_action_releases_a_quiet_window_and_leaves_a_path_lease_alone() {
    let mut w = world();
    let sys = system(&mut w);
    let ctl = controller(&mut w);
    let path = path_lease(&mut w, "/work/a", 3600.0);
    let quiet = quiet_window(&mut w, &ctl, NOW + 3_600_000);
    let on_path = ReconcileAction::CloseQuiet {
        key: format!("quiet:{}:{}", path.id, NOW),
        lease: path.id.clone(),
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &on_path, NOW))
            .expect("apply")
    );
    assert_eq!(released(&w.s, &path.id, NOW), None);
    let on_quiet = ReconcileAction::CloseQuiet {
        key: format!("quiet:{}:{}", quiet.id, NOW + 3_600_000),
        lease: quiet.id.clone(),
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &on_quiet, NOW))
            .expect("apply")
    );
    assert_eq!(released(&w.s, &quiet.id, NOW), Some(NOW));
}

#[test]
fn a_notice_action_posts_its_kind_subject_and_text_to_its_target() {
    let mut w = world();
    let sys = system(&mut w);
    let action = ReconcileAction::Notice {
        key: "stale1:c1:1".into(),
        target: pk_hex(&w.alice),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "card c1 (Port scheduler) has been claimed for 30 minutes without activity".into(),
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &action, NOW))
            .expect("apply")
    );
    assert_eq!(
        notices_of(&w, &w.alice),
        vec![(
            "stale".to_string(),
            "c1".to_string(),
            "card c1 (Port scheduler) has been claimed for 30 minutes without activity".to_string(),
            NOW,
        )]
    );
}

#[test]
fn a_reclaim_action_returns_the_claimed_card_to_ready_with_no_owner() {
    let mut w = world();
    let sys = system(&mut w);
    claimed_card(&mut w, "r1");
    let action = ReconcileAction::ReclaimCard {
        key: format!("reclaim:r1:{}", at(2)),
        card: "r1".into(),
        owner: pk_hex(&w.alice),
    };
    assert!(
        w.s.apply_reconcile(at(3), &act(&sys, &action, at(3)))
            .expect("apply")
    );
    assert_eq!(w.s.card_state("r1").expect("state"), CardState::Ready);
    let rows =
        w.s.list_cards(Some(CardState::Ready), None, 10)
            .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].owner, None);
}

#[test]
fn a_reclaim_action_records_a_card_moved_event_from_claimed_to_ready() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    claimed_card(&mut w, "r1");
    let action = ReconcileAction::ReclaimCard {
        key: format!("reclaim:r1:{}", at(2)),
        card: "r1".into(),
        owner: pk_hex(&w.alice),
    };
    w.s.apply_reconcile(at(3), &act(&sys, &action, at(3)))
        .expect("apply");
    let moves = card_moves(&scratch, "r1");
    assert_eq!(
        moves.last(),
        Some(&(
            at(3),
            hex(&sys.public()),
            json!({"from": "claimed", "to": "ready"})
        ))
    );
}

#[test]
fn a_reclaim_action_clears_the_card_approvals() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    claimed_card(&mut w, "r1");
    // Approvals exist only while a card is in review, and every move back to claimed or doing clears them, so a
    // claimed card holds none through the public API. The row is planted to check the reclaim's own clearing.
    scratch
        .conn()
        .execute(
            "INSERT INTO approvals(card, by) VALUES ('r1', 'planted')",
            [],
        )
        .expect("plant an approval");
    let action = ReconcileAction::ReclaimCard {
        key: format!("reclaim:r1:{}", at(2)),
        card: "r1".into(),
        owner: pk_hex(&w.alice),
    };
    w.s.apply_reconcile(at(3), &act(&sys, &action, at(3)))
        .expect("apply");
    let left: i64 = scratch
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM approvals WHERE card = 'r1'",
            [],
            |r| r.get(0),
        )
        .expect("count approvals");
    assert_eq!(left, 0);
}

#[test]
fn a_blocker_action_opens_one_blocked_ops_card_that_needs_an_owner() {
    let mut w = world();
    let sys = system(&mut w);
    assert!(open_blocker(
        &mut w,
        &sys,
        "blocker:1",
        BLOCKER_TITLE,
        BLOCKER_ACTION,
        NOW
    ));
    let rows =
        w.s.list_cards(Some(CardState::Blocked), Some("ops"), 10)
            .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].id.as_str(), rows[0].title.as_str()),
        ("blocker-1", BLOCKER_TITLE)
    );
    let ext = w.s.card_ext(&["blocker-1".to_string()]).expect("card ext");
    assert!(ext[0].needs_owner);
    assert_eq!(ext[0].owner_action, BLOCKER_ACTION);
    assert_eq!(input(&w, NOW).blocker_card.as_deref(), Some("blocker-1"));
}

#[test]
fn a_second_open_blocker_action_refreshes_the_open_card_instead_of_adding_one() {
    let mut w = world();
    let sys = system(&mut w);
    open_blocker(
        &mut w,
        &sys,
        "blocker:1",
        BLOCKER_TITLE,
        BLOCKER_ACTION,
        NOW,
    );
    let later = NOW + 600_000;
    let title = "Machine floor breached: available memory 0.4 GiB below the floor of 1.0 GiB";
    let action = "free memory (0.4 GiB < floor 1.0 GiB) -- admission is paused until then";
    assert!(open_blocker(
        &mut w,
        &sys,
        "blocker:2",
        title,
        action,
        later
    ));
    let rows = w.s.list_cards(None, None, 10).expect("list");
    assert_eq!(rows.len(), 1, "only the one blocker card exists");
    assert_eq!(
        (rows[0].id.as_str(), rows[0].title.as_str()),
        ("blocker-1", title)
    );
    assert_eq!(
        w.s.card_ext(&["blocker-1".to_string()]).expect("ext")[0].owner_action,
        action
    );
}

#[test]
fn a_close_blocker_action_cancels_its_card_and_forgets_it() {
    let mut w = world();
    let sys = system(&mut w);
    open_blocker(
        &mut w,
        &sys,
        "blocker:1",
        BLOCKER_TITLE,
        BLOCKER_ACTION,
        NOW,
    );
    let closing = NOW + 3_723_000;
    assert!(close_blocker(&mut w, &sys, "unblock:blocker-1", closing));
    assert_eq!(
        w.s.card_state("blocker-1").expect("state"),
        CardState::Cancelled
    );
    assert!(!w.s.card_ext(&["blocker-1".to_string()]).expect("ext")[0].needs_owner);
    assert_eq!(input(&w, closing).blocker_card, None);
}

#[test]
fn closing_the_blocker_records_the_resolution_as_iso_utc_evidence() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    open_blocker(
        &mut w,
        &sys,
        "blocker:1",
        BLOCKER_TITLE,
        BLOCKER_ACTION,
        NOW,
    );
    // NOW is 2023-11-14T22:13:20Z; this close is 1 h 2 min 3 s later.
    close_blocker(&mut w, &sys, "unblock:blocker-1", NOW + 3_723_000);
    assert_eq!(
        evidence_refs(&scratch, "blocker-1"),
        vec!["resolved at 2023-11-14T23:15:23Z".to_string()]
    );
}

#[test]
fn closing_the_blocker_records_a_card_moved_event_from_blocked_to_cancelled() {
    let scratch = Scratch::fresh();
    let mut w = disk_world(&scratch);
    let sys = system(&mut w);
    open_blocker(
        &mut w,
        &sys,
        "blocker:1",
        BLOCKER_TITLE,
        BLOCKER_ACTION,
        NOW,
    );
    let closing = NOW + 3_723_000;
    close_blocker(&mut w, &sys, "unblock:blocker-1", closing);
    assert_eq!(
        card_moves(&scratch, "blocker-1").last(),
        Some(&(
            closing,
            hex(&sys.public()),
            json!({"from": "blocked", "to": "cancelled"})
        ))
    );
}

#[test]
fn an_admission_action_sets_the_pause_reason_and_a_later_one_clears_it() {
    let mut w = world();
    let sys = system(&mut w);
    let pause = ReconcileAction::Admission {
        key: "pause:1".into(),
        paused: Some("disk free below the floor".into()),
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &pause, NOW))
            .expect("apply")
    );
    assert_eq!(
        w.s.admission_paused().expect("paused"),
        Some("disk free below the floor".into())
    );
    let resume = ReconcileAction::Admission {
        key: "resume:1".into(),
        paused: None,
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &resume, NOW))
            .expect("apply")
    );
    assert_eq!(w.s.admission_paused().expect("paused"), None);
}

// ---- idempotency and authority -------------------------------------------------------------------------------------

#[test]
fn a_repeated_key_is_a_no_op_and_the_first_application_keeps_its_mark() {
    let mut w = world();
    let sys = system(&mut w);
    let first = ReconcileAction::Notice {
        key: "n1".into(),
        target: pk_hex(&w.alice),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "first".into(),
    };
    assert!(
        w.s.apply_reconcile(NOW, &act(&sys, &first, NOW))
            .expect("first apply")
    );
    let again = ReconcileAction::Notice {
        key: "n1".into(),
        target: pk_hex(&w.alice),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "second".into(),
    };
    let later = NOW + 60_000;
    assert!(
        !w.s.apply_reconcile(later, &act(&sys, &again, later))
            .expect("second apply")
    );
    assert_eq!(
        notices_of(&w, &w.alice),
        vec![("stale".into(), "c1".into(), "first".into(), NOW)]
    );
    assert_eq!(
        input(&w, later).marks,
        vec![Mark {
            key: "n1".into(),
            last_ms: NOW,
            count: 1
        }]
    );
}

#[test]
fn a_repeated_admission_key_keeps_the_first_pause_reason() {
    let mut w = world();
    let sys = system(&mut w);
    let first = ReconcileAction::Admission {
        key: "pause:1".into(),
        paused: Some("first".into()),
    };
    w.s.apply_reconcile(NOW, &act(&sys, &first, NOW))
        .expect("first apply");
    let again = ReconcileAction::Admission {
        key: "pause:1".into(),
        paused: Some("second".into()),
    };
    assert!(
        !w.s.apply_reconcile(NOW, &act(&sys, &again, NOW))
            .expect("second apply")
    );
    assert_eq!(
        w.s.admission_paused().expect("paused"),
        Some("first".into())
    );
}

#[test]
fn a_reconcile_action_signed_by_a_worker_is_forbidden_and_changes_nothing() {
    let mut w = world();
    let action = ReconcileAction::Notice {
        key: "n-forged".into(),
        target: pk_hex(&w.alice),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "forged".into(),
    };
    let forged = act(&w.alice, &action, NOW);
    let err =
        w.s.apply_reconcile(NOW, &forged)
            .expect_err("a worker may not reconcile");
    assert!(
        matches!(
            err,
            StoreError::Transition(TransitionError::Forbidden(Role::Worker))
        ),
        "got {err:?}"
    );
    assert!(notices_of(&w, &w.alice).is_empty());
    assert!(input(&w, NOW).marks.is_empty());
}
