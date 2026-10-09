//! Tests for `apply_reconcile`: every action kind, idempotency and the refusals.

use super::*;
use agora_core::{Role, TransitionError};

#[test]
fn every_action_kind_applies_once_and_then_is_a_no_op() {
    let (mut s, sys) = world();
    let worker = person(5);
    enroll(&mut s, &worker, "worker", false);
    let w = pk_hex(&worker);
    lease(&s, "L1", "path", &w, Some(NOW - SEC), None);
    lease(&s, "Q1", "quiet", &w, None, None);
    card(&s, "c1", "t", "l", "claimed", Some(&w), NOW - 2 * MIN);
    let actions = [
        ReconcileAction::ExpireLease {
            key: "expire:L1".into(),
            lease: "L1".into(),
        },
        ReconcileAction::CloseQuiet {
            key: "quiet:Q1".into(),
            lease: "Q1".into(),
        },
        ReconcileAction::Notice {
            key: "stale1:c1:1".into(),
            target: w.clone(),
            kind: "stale".into(),
            subject: "c1".into(),
            text: "move it".into(),
        },
        ReconcileAction::ReclaimCard {
            key: "reclaim:c1:1".into(),
            card: "c1".into(),
            owner: w.clone(),
        },
        ReconcileAction::Blocker {
            key: "blocker:1".into(),
            open: true,
            title: "Machine floor breached".into(),
            owner_action: "free disk".into(),
        },
        ReconcileAction::Blocker {
            key: "unblock:blocker-1".into(),
            open: false,
            title: String::new(),
            owner_action: String::new(),
        },
        ReconcileAction::Admission {
            key: "pause:1".into(),
            paused: Some("disk low".into()),
        },
        ReconcileAction::Admission {
            key: "resume:1".into(),
            paused: None,
        },
    ];
    for (i, action) in actions.iter().enumerate() {
        assert!(
            apply(&mut s, &sys, action, &format!("a{i}")).expect("first apply"),
            "first apply of {} must apply",
            action.key()
        );
    }
    for (i, action) in actions.iter().enumerate() {
        assert!(
            !apply(&mut s, &sys, action, &format!("b{i}")).expect("second apply"),
            "second apply of {} must be a no-op",
            action.key()
        );
    }
    assert_eq!(count(&s, "SELECT COUNT(*) FROM reconcile_marks"), 8);
}

#[test]
fn expire_and_close_release_open_leases_at_now_and_keep_earlier_releases() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    lease(&s, "L1", "path", &w, Some(NOW - SEC), None);
    lease(&s, "Q1", "quiet", &w, None, None);
    lease(&s, "L2", "path", &w, Some(NOW - SEC), Some(NOW - MIN));
    let expire = ReconcileAction::ExpireLease {
        key: "expire:L1".into(),
        lease: "L1".into(),
    };
    let close = ReconcileAction::CloseQuiet {
        key: "quiet:Q1".into(),
        lease: "Q1".into(),
    };
    let again = ReconcileAction::ExpireLease {
        key: "expire:L2".into(),
        lease: "L2".into(),
    };
    assert!(apply(&mut s, &sys, &expire, "e1").expect("expire"));
    assert!(apply(&mut s, &sys, &close, "e2").expect("close"));
    assert!(apply(&mut s, &sys, &again, "e3").expect("already released"));
    assert_eq!(released_of(&s, "L1"), Some(NOW));
    assert_eq!(released_of(&s, "Q1"), Some(NOW));
    assert_eq!(released_of(&s, "L2"), Some(NOW - MIN));
}

#[test]
fn expire_never_releases_a_quiet_lease_and_close_never_releases_a_path_lease() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    lease(&s, "Q1", "quiet", &w, Some(NOW - SEC), None);
    lease(&s, "P1", "path", &w, None, None);
    lease(&s, "O1", "pool", &w, None, None);
    let expire_quiet = ReconcileAction::ExpireLease {
        key: "expire:Q1".into(),
        lease: "Q1".into(),
    };
    let expire_unbounded = ReconcileAction::ExpireLease {
        key: "expire:O1".into(),
        lease: "O1".into(),
    };
    let close_path = ReconcileAction::CloseQuiet {
        key: "quiet:P1".into(),
        lease: "P1".into(),
    };
    apply(&mut s, &sys, &expire_quiet, "q1").expect("expire aimed at a quiet lease");
    apply(&mut s, &sys, &expire_unbounded, "q2")
        .expect("expire aimed at a pool lease without expiry");
    apply(&mut s, &sys, &close_path, "q3").expect("close aimed at a path lease");
    assert_eq!(released_of(&s, "Q1"), None);
    assert_eq!(released_of(&s, "P1"), None);
    assert_eq!(released_of(&s, "O1"), None);
}

#[test]
fn expire_releases_only_a_lease_whose_expiry_has_passed() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    lease(&s, "early", "path", &w, Some(NOW + MIN), None);
    lease(&s, "at-expiry", "pool", &w, Some(NOW), None);
    let early = ReconcileAction::ExpireLease {
        key: "expire:early".into(),
        lease: "early".into(),
    };
    let at_expiry = ReconcileAction::ExpireLease {
        key: "expire:at-expiry".into(),
        lease: "at-expiry".into(),
    };
    apply(&mut s, &sys, &early, "x1").expect("expire before expiry");
    apply(&mut s, &sys, &at_expiry, "x2").expect("expire at expiry");
    assert_eq!(released_of(&s, "early"), None);
    assert_eq!(released_of(&s, "at-expiry"), Some(NOW));
}

#[test]
fn every_applied_action_is_audited_with_its_entity_as_subject_and_its_key_as_data() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    lease(&s, "L1", "path", &w, Some(NOW - SEC), None);
    card(&s, "c1", "t", "l", "doing", Some(&w), NOW - MIN);
    let actions = [
        ReconcileAction::ExpireLease {
            key: "expire:L1".into(),
            lease: "L1".into(),
        },
        ReconcileAction::Notice {
            key: "stale:c1".into(),
            target: w.clone(),
            kind: "stale".into(),
            subject: "c1".into(),
            text: "move it".into(),
        },
        ReconcileAction::ReclaimCard {
            key: "reclaim:c1".into(),
            card: "c1".into(),
            owner: w.clone(),
        },
        ReconcileAction::Blocker {
            key: "blocker:1".into(),
            open: true,
            title: "Machine floor breached".into(),
            owner_action: "free disk".into(),
        },
        ReconcileAction::Admission {
            key: "pause:1".into(),
            paused: Some("disk low".into()),
        },
    ];
    for (i, action) in actions.iter().enumerate() {
        assert!(apply(&mut s, &sys, action, &format!("au{i}")).expect("apply"));
    }
    let rows: Vec<(String, String, String)> = {
        let mut st = s
            .conn
            .prepare(
                "SELECT kind, subject, data FROM events WHERE kind LIKE 'reconcile.%' ORDER BY seq",
            )
            .expect("prepare");
        let mapped = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query");
        mapped.collect::<Result<_, _>>().expect("rows")
    };
    let row = |kind: &str, subject: &str, key: &str| {
        (
            kind.to_string(),
            subject.to_string(),
            format!(r#"{{"key":"{key}"}}"#),
        )
    };
    assert_eq!(
        rows,
        vec![
            row("reconcile.expire", "L1", "expire:L1"),
            row("reconcile.notice", &w, "stale:c1"),
            row("reconcile.reclaim", "c1", "reclaim:c1"),
            row("reconcile.blocker", "blocker-1", "blocker:1"),
            row("reconcile.admission", "admission", "pause:1"),
        ]
    );
}

#[test]
fn resolution_evidence_is_iso_utc_without_the_sub_second_part() {
    assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso_utc(NOW + 999), "2023-11-14T22:13:20Z");
    assert_eq!(iso_utc(NOW + 61 * SEC), "2023-11-14T22:14:21Z");
}

#[test]
fn a_notice_action_is_posted_once_to_its_target() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    let notice = ReconcileAction::Notice {
        key: "stale1:c1:1".into(),
        target: w.clone(),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "move it".into(),
    };
    assert!(apply(&mut s, &sys, &notice, "n1").expect("first"));
    assert!(!apply(&mut s, &sys, &notice, "n2").expect("second"));
    let rows: i64 = s
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notices WHERE target = ?1 AND kind = 'stale'",
            [w.as_str()],
            |r| r.get(0),
        )
        .expect("notices");
    assert_eq!(rows, 1);
}

#[test]
fn reclaim_returns_the_card_to_ready_and_clears_owner_and_approvals() {
    let (mut s, sys) = world();
    let w = pk_hex(&person(5));
    card(&s, "c1", "t", "l", "doing", Some(&w), NOW - MIN);
    s.conn
        .execute(
            "INSERT INTO approvals(card, by) VALUES ('c1', ?1)",
            [w.as_str()],
        )
        .expect("approval");
    let action = ReconcileAction::ReclaimCard {
        key: "reclaim:c1:1".into(),
        card: "c1".into(),
        owner: w.clone(),
    };
    assert!(apply(&mut s, &sys, &action, "r1").expect("reclaim"));
    let (state, owner, updated): (String, Option<String>, i64) = s
        .conn
        .query_row(
            "SELECT state, owner, updated FROM cards WHERE id = 'c1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("card");
    assert_eq!((state.as_str(), owner, updated), ("ready", None, NOW));
    assert_eq!(
        count(&s, "SELECT COUNT(*) FROM approvals WHERE card = 'c1'"),
        0
    );
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'card.reclaimed' AND subject = 'c1'"
        ),
        1
    );
    let moved: String = s
        .conn
        .query_row(
            "SELECT data FROM events WHERE kind = 'card.moved' AND subject = 'c1'",
            [],
            |r| r.get(0),
        )
        .expect("card.moved");
    assert_eq!(moved, r#"{"from":"doing","to":"ready"}"#);
}

#[test]
fn reclaim_leaves_a_card_now_held_by_someone_else_alone() {
    let (mut s, sys) = world();
    let old = pk_hex(&person(5));
    let new = pk_hex(&person(6));
    card(&s, "c1", "t", "l", "claimed", Some(&new), NOW - MIN);
    let action = ReconcileAction::ReclaimCard {
        key: "reclaim:c1:1".into(),
        card: "c1".into(),
        owner: old,
    };
    assert!(apply(&mut s, &sys, &action, "r1").expect("planned against the old owner"));
    let owner: Option<String> = s
        .conn
        .query_row("SELECT owner FROM cards WHERE id = 'c1'", [], |r| r.get(0))
        .expect("card");
    assert_eq!(owner, Some(new));
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'card.reclaimed'"
        ),
        0
    );
}

#[test]
fn a_worker_signed_reconcile_op_is_forbidden_and_changes_nothing() {
    let (mut s, _sys) = world();
    let worker = person(5);
    enroll(&mut s, &worker, "worker", false);
    let w = pk_hex(&worker);
    lease(&s, "L1", "path", &w, Some(NOW - SEC), None);
    let action = ReconcileAction::ExpireLease {
        key: "expire:L1".into(),
        lease: "L1".into(),
    };
    let err = s
        .apply_reconcile(NOW, &signed_action(&worker, &action, "w1"))
        .expect_err("a worker cannot sign reconcile ops");
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(released_of(&s, "L1"), None);
    assert_eq!(count(&s, "SELECT COUNT(*) FROM reconcile_marks"), 0);
}

#[test]
fn an_op_whose_kind_does_not_match_its_action_is_invalid() {
    let (mut s, sys) = world();
    let action = ReconcileAction::ExpireLease {
        key: "expire:L1".into(),
        lease: "L1".into(),
    };
    let arg = serde_json::to_string(&action).expect("json");
    let mismatched = SignedOp::new("reconcile.notice", "", &arg, NOW, "m1").sign(&sys);
    assert!(matches!(
        s.apply_reconcile(NOW, &mismatched),
        Err(StoreError::Invalid("wrong operation kind"))
    ));
    let other_system_kind = SignedOp::new("job.start", "", &arg, NOW, "m2").sign(&sys);
    assert!(matches!(
        s.apply_reconcile(NOW, &other_system_kind),
        Err(StoreError::Invalid("wrong operation kind"))
    ));
    let garbage = SignedOp::new("reconcile.expire", "", "{}", NOW, "m3").sign(&sys);
    assert!(matches!(
        s.apply_reconcile(NOW, &garbage),
        Err(StoreError::Invalid(
            "reconcile arg must be a reconcile action"
        ))
    ));
}
