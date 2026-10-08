//! Public-API tests for cards: the review-gated lifecycle, independent approval,
//! ownership of claimed work, role gates, failed transitions, the stale-claim
//! watchdog, and malformed create args.

mod common;
use common::*;

use agora_core::{CardState, Role, TransitionError};
use agora_store::*;

/// Walk a backlog card to Review as its owner (alice), without evidence or approval.
fn to_review(w: &mut World, id: &str) {
    w.s.transition(NOW, &op(&w.sup, "card.move", id, "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", id, "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", id, "doing"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", id, "review"), false)
        .unwrap();
}

#[test]
fn card_lifecycle_with_review_gate() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), true)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), true)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "doing"), true)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "review"), true)
        .unwrap();

    // No evidence yet.
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::EvidenceRequired)
    ));

    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    // The owner's own approval does not count as review.
    w.s.approve(NOW, &op(&w.alice, "card.approve", "c1", ""))
        .unwrap();
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::ReviewRequired)
    ));

    w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Done);
}

#[test]
fn only_the_owner_or_a_supervisor_can_move_claimed_work() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    assert_eq!(
        w.s.stale_cards(NOW + 2000, 1000).unwrap(),
        vec![("c1".to_string(), Some(pk_hex(&w.alice)))]
    );

    let err =
        w.s.transition(NOW, &op(&w.bob, "card.move", "c1", "doing"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "doing"), false)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Doing);
}

#[test]
fn non_owner_cannot_move_work_out_of_doing_or_review() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "doing"), false)
        .unwrap();

    let err =
        w.s.transition(NOW, &op(&w.bob, "card.move", "c1", "review"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Doing);
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "review"), false)
        .unwrap();

    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
        .unwrap();
    let err =
        w.s.transition(NOW, &op(&w.bob, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Review);

    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Done);
}

#[test]
fn failed_transition_appends_no_event() {
    let mut w = world();
    make_card(&mut w, "c1");
    let before = w.s.event_count().unwrap();
    assert!(matches!(
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "ready"), false),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    assert!(matches!(
        w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "limbo"), false),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn watchdog_lists_and_reclaims_stale_claims() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    assert!(w.s.stale_cards(NOW + 100, 1000).unwrap().is_empty());
    assert_eq!(
        w.s.stale_cards(NOW + 2000, 1000).unwrap(),
        vec![("c1".to_string(), Some(pk_hex(&w.alice)))]
    );

    let later = NOW + 2000;
    let err =
        w.s.reclaim_stale(later, &op_at(&w.alice, "card.reclaim", "c1", "", later))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    w.s.reclaim_stale(later, &op_at(&w.sup, "card.reclaim", "c1", "", later))
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
    assert!(w.s.stale_cards(later, 1000).unwrap().is_empty());
    let err =
        w.s.reclaim_stale(later, &op_at(&w.sup, "card.reclaim", "c1", "", later))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn leaving_backlog_requires_a_supervisor() {
    let mut w = world();
    make_card(&mut w, "c1");
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "blocked"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    // rejected by agora-core's cancel rule as well; the blocked case above is what exercises leave-backlog
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "cancelled"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Backlog);

    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "blocked"), false)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Blocked);
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn duplicate_card_id_is_rejected() {
    let mut w = world();
    make_card(&mut w, "c1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.create_card(
            NOW,
            &op(&w.sup, "card.create", "c1", r#"{"title":"Port scheduler"}"#),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("card already exists")));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn watchdog_window_saturates_instead_of_overflowing() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    assert!(w.s.stale_cards(-5, i64::MAX).unwrap().is_empty());
}

#[test]
fn reclaim_from_doing_returns_card_to_ready() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "doing"), false)
        .unwrap();
    w.s.reclaim_stale(NOW, &op(&w.sup, "card.reclaim", "c1", ""))
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn create_card_rejects_malformed_arguments_without_an_event() {
    let mut w = world();
    let before = w.s.event_count().unwrap();
    for arg in [
        "not json",
        r#"{"lane":"x"}"#,
        r#"{"title":42}"#,
        r#"{"title":""}"#,
        r#"{"title":"t","lane":7}"#,
        r#"["t"]"#,
    ] {
        let err =
            w.s.create_card(NOW, &op(&w.alice, "card.create", "c1", arg))
                .unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid(_)),
            "arg {arg} gave {err:?}"
        );
    }
    let err =
        w.s.create_card(NOW, &op(&w.alice, "card.create", "", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(w.s.event_count().unwrap(), before);
    assert!(matches!(w.s.card_state("c1"), Err(StoreError::NotFound(_))));
}

#[test]
fn approval_outside_review_is_rejected_and_leaves_nothing_behind() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    let err =
        w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("approval only in review")
    ));

    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "doing"), false)
        .unwrap();
    let err =
        w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("approval only in review")
    ));

    // Neither rejected approval counts once the card reaches review.
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "review"), false)
        .unwrap();
    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::ReviewRequired)
    ));
}

#[test]
fn rework_clears_approvals_so_done_needs_a_fresh_one() {
    let mut w = world();
    make_card(&mut w, "c1");
    to_review(&mut w, "c1");
    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
        .unwrap();

    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "doing"), true)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "review"), true)
        .unwrap();
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::ReviewRequired)
    ));

    w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Done);
}

#[test]
fn approval_by_a_revoked_identity_does_not_count() {
    let mut w = world();
    make_card(&mut w, "c1");
    to_review(&mut w, "c1");
    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    w.s.approve(NOW, &op(&w.bob, "card.approve", "c1", ""))
        .unwrap();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();

    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::ReviewRequired)
    ));

    w.s.approve(NOW, &op(&w.carol, "card.approve", "c1", ""))
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "done"), true)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Done);
}

#[test]
fn blocked_claimed_work_cannot_be_unblocked_by_another_worker() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "blocked"), false)
        .unwrap();

    let err =
        w.s.transition(NOW, &op(&w.bob, "card.move", "c1", "ready"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Blocked);

    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn workers_cannot_promote_or_cancel_cards() {
    let mut w = world();
    make_card(&mut w, "c1");
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "ready"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Backlog);

    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "cancelled"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn observers_cannot_create_or_claim_cards() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();

    let err =
        w.s.create_card(NOW, &op(&w.olive, "card.create", "c2", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    let err =
        w.s.transition(NOW, &op(&w.olive, "card.move", "c1", "claimed"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Ready);
    assert!(matches!(w.s.card_state("c2"), Err(StoreError::NotFound(_))));
}

#[test]
fn failed_gate_rolls_back_so_the_same_signed_op_can_be_retried() {
    let mut w = world();
    make_card(&mut w, "c1");
    to_review(&mut w, "c1");
    let done = op_n(&w.alice, "card.move", "c1", "done", "retry-me");

    let before = w.s.event_count().unwrap();
    let err = w.s.transition(NOW, &done, false).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::EvidenceRequired)
    ));
    assert_eq!(w.s.event_count().unwrap(), before);

    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", "run.log"))
        .unwrap();
    w.s.transition(NOW, &done, false).unwrap();
    assert_eq!(w.s.card_state("c1").unwrap(), CardState::Done);
}

#[test]
fn unknown_and_revoked_identities_cannot_create_cards() {
    let mut w = world();
    let err =
        w.s.create_card(NOW, &op(&w.eve, "card.create", "c1", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("unknown identity")));

    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let err =
        w.s.create_card(NOW, &op(&w.bob, "card.create", "c1", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
    assert!(matches!(w.s.card_state("c1"), Err(StoreError::NotFound(_))));
}
