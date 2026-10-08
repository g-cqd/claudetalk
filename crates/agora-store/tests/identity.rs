//! Public-API tests for identities: bootstrap, role grants and forgery, unknown and
//! revoked keys, replay protection, nonce handling, and tamper/clock rules.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::Identity;
use agora_store::*;

#[test]
fn bootstrap_owner_happens_once() {
    let mut s = Store::open_in_memory().unwrap();
    let first = Identity::from_seed([1; 32]);
    let second = Identity::from_seed([9; 32]);
    s.bootstrap_owner(NOW, &first.public()).unwrap();
    assert_eq!(s.role_of(&first.public()).unwrap(), Role::Owner);
    assert!(matches!(
        s.bootstrap_owner(NOW, &second.public()),
        Err(StoreError::Invalid("an owner already exists"))
    ));
    assert!(matches!(
        s.role_of(&second.public()),
        Err(StoreError::Invalid("unknown identity"))
    ));
}

#[test]
fn roles_cannot_be_forged_or_escalated() {
    let mut w = world();
    make_card(&mut w, "c1");

    // Backlog to ready is supervisor-only.
    let err =
        w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "ready"), false)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));

    // A worker cannot grant any role.
    let err =
        w.s.grant_role(
            NOW,
            &op(&w.alice, "role.grant", &pk_hex(&w.bob), "supervisor"),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);

    // The owner makes a controller. A controller cannot grant controller, but can grant supervisor.
    let charlie = Identity::from_seed([5; 32]);
    w.s.register_identity(NOW, &charlie.public(), "charlie")
        .unwrap();
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&charlie), "controller"),
    )
    .unwrap();
    assert_eq!(w.s.role_of(&charlie.public()).unwrap(), Role::Controller);
    let err =
        w.s.grant_role(
            NOW,
            &op(&charlie, "role.grant", &pk_hex(&w.bob), "controller"),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Controller))
    ));
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
    w.s.grant_role(
        NOW,
        &op(&charlie, "role.grant", &pk_hex(&w.bob), "supervisor"),
    )
    .unwrap();
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Supervisor);

    // A controller cannot revoke the owner.
    let err =
        w.s.revoke_identity(NOW, &op(&charlie, "identity.revoke", &pk_hex(&w.owner), ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Controller))
    ));
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn observer_role_is_granted_and_cannot_write() {
    let mut w = world();
    assert_eq!(w.s.role_of(&w.olive.public()).unwrap(), Role::Observer);
    let err =
        w.s.create_card(NOW, &op(&w.olive, "card.create", "c1", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
}

#[test]
fn unknown_and_revoked_identities_are_rejected() {
    let mut w = world();
    let err =
        w.s.create_card(NOW, &op(&w.eve, "card.create", "c1", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("unknown identity")));

    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .unwrap();
    let err =
        w.s.create_card(NOW, &op(&w.alice, "card.create", "c1", r#"{"title":"t"}"#))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
}

#[test]
fn replayed_operations_are_rejected() {
    let mut w = world();
    make_card(&mut w, "c1");
    let ev = op_n(&w.alice, "card.evidence", "c1", "run.log", "explicit-1");
    w.s.add_evidence(NOW, &ev).unwrap();
    let before = w.s.event_count().unwrap();
    let err = w.s.add_evidence(NOW, &ev).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("replayed operation")));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn replay_with_same_nonce_but_different_content_is_rejected() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.add_evidence(NOW, &op_n(&w.alice, "card.evidence", "c1", "a", "N"))
        .unwrap();
    let err =
        w.s.add_evidence(NOW, &op_n(&w.alice, "card.evidence", "c1", "b", "N"))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "replayed operation"),
        "{err:?}"
    );
}

#[test]
fn same_nonce_from_a_different_actor_is_allowed() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.add_evidence(NOW, &op_n(&w.alice, "card.evidence", "c1", "a", "N"))
        .unwrap();
    w.s.add_evidence(NOW, &op_n(&w.bob, "card.evidence", "c1", "b", "N"))
        .unwrap();
}

#[test]
fn nonce_gc_survives_extreme_now() {
    let mut w = world();
    make_card(&mut w, "c1");
    let t = i64::MIN;
    w.s.add_evidence(t, &op_at(&w.alice, "card.evidence", "c1", "r", t))
        .unwrap();
}

#[test]
fn nonces_are_forgotten_only_after_twice_the_skew_window() {
    let mut w = world();
    make_card(&mut w, "c1");
    w.s.add_evidence(NOW, &op_n(&w.alice, "card.evidence", "c1", "a", "N"))
        .unwrap();
    let edge = NOW + 2 * MAX_SKEW_MS;
    let at_edge = SignedOp::new("card.evidence", "c1", "b", edge, "N").sign(&w.alice);
    assert!(matches!(
        w.s.add_evidence(edge, &at_edge),
        Err(StoreError::Invalid("replayed operation"))
    ));
    let past = SignedOp::new("card.evidence", "c1", "b", edge + 1, "N").sign(&w.alice);
    w.s.add_evidence(edge + 1, &past).unwrap();
}

#[test]
fn failed_operations_do_not_burn_their_nonce() {
    let mut w = world();
    let ev = op(&w.alice, "card.evidence", "c2", "run.log");
    // The card does not exist yet, so the operation fails inside its transaction.
    assert!(matches!(
        w.s.add_evidence(NOW, &ev),
        Err(StoreError::NotFound(_))
    ));
    make_card(&mut w, "c2");
    w.s.add_evidence(NOW, &ev).unwrap();
}

#[test]
fn tampered_or_misdirected_operations_are_rejected() {
    let mut w = world();
    make_card(&mut w, "c1");

    let mut tampered_arg = op(&w.alice, "card.evidence", "c1", "run.log");
    tampered_arg.arg = "forged.log".into();
    assert!(matches!(
        w.s.add_evidence(NOW, &tampered_arg),
        Err(StoreError::Crypto(_))
    ));

    let mut tampered_subject = op(&w.alice, "card.evidence", "c1", "run.log");
    tampered_subject.subject = "c2".into();
    assert!(matches!(
        w.s.add_evidence(NOW, &tampered_subject),
        Err(StoreError::Crypto(_))
    ));

    let wrong_kind = op(&w.alice, "card.approve", "c1", "");
    assert!(matches!(
        w.s.add_evidence(NOW, &wrong_kind),
        Err(StoreError::Invalid("wrong operation kind"))
    ));
}

/// Owner grants carol and bob the controller role, making them peers.
fn with_two_controllers() -> World {
    let mut w = world();
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&w.carol), "controller"),
    )
    .unwrap();
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&w.bob), "controller"),
    )
    .unwrap();
    w
}

#[test]
fn extreme_timestamps_are_rejected_without_panic_or_event() {
    let mut w = world();
    make_card(&mut w, "c1");
    let before = w.s.event_count().unwrap();
    // NOW - far_past == i64::MAX, the largest distance an i64 can hold.
    let far_past = NOW - (1i64 << 62) - (1i64 << 62) + 1;
    for ts in [i64::MIN, i64::MAX, far_past] {
        let stamped = op_at(&w.alice, "card.evidence", "c1", "run.log", ts);
        let err = w.s.add_evidence(NOW, &stamped).unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid(m) if m.contains("clock skew")),
            "ts {ts}: {err:?}"
        );
    }
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn skew_beyond_the_window_is_reported_as_clock_skew() {
    let mut w = world();
    make_card(&mut w, "c1");
    let ahead = op_at(
        &w.alice,
        "card.evidence",
        "c1",
        "run.log",
        NOW + MAX_SKEW_MS + 1,
    );
    let err = w.s.add_evidence(NOW, &ahead).unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m.contains("clock skew")),
        "{err:?}"
    );
    let behind = op_at(
        &w.alice,
        "card.evidence",
        "c1",
        "run.log",
        NOW - MAX_SKEW_MS - 1,
    );
    let err = w.s.add_evidence(NOW, &behind).unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m.contains("clock skew")),
        "{err:?}"
    );
}

#[test]
fn skew_exactly_at_the_window_is_accepted_in_both_directions() {
    let mut w = world();
    make_card(&mut w, "c1");
    let ahead = op_at(
        &w.alice,
        "card.evidence",
        "c1",
        "ahead.log",
        NOW + MAX_SKEW_MS,
    );
    w.s.add_evidence(NOW, &ahead).unwrap();
    let behind = op_at(
        &w.alice,
        "card.evidence",
        "c1",
        "behind.log",
        NOW - MAX_SKEW_MS,
    );
    w.s.add_evidence(NOW, &behind).unwrap();
}

#[test]
fn grant_signed_by_bob_cannot_be_reattributed_to_owner() {
    let mut w = world();
    let mut forged = op(&w.bob, "role.grant", &pk_hex(&w.bob), "supervisor");
    forged.actor = w.owner.public();
    let before = w.s.event_count().unwrap();
    assert!(matches!(
        w.s.grant_role(NOW, &forged),
        Err(StoreError::Crypto(_))
    ));
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn owner_signed_grant_cannot_be_reattributed_to_worker() {
    let mut w = world();
    let mut forged = op(&w.owner, "role.grant", &pk_hex(&w.alice), "supervisor");
    forged.actor = w.bob.public();
    assert!(matches!(
        w.s.grant_role(NOW, &forged),
        Err(StoreError::Crypto(_))
    ));
    assert_eq!(w.s.role_of(&w.alice.public()).unwrap(), Role::Worker);
}

#[test]
fn observer_cannot_grant_roles() {
    let mut w = world();
    let err =
        w.s.grant_role(
            NOW,
            &op(&w.olive, "role.grant", &pk_hex(&w.bob), "supervisor"),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
}

#[test]
fn last_owner_cannot_revoke_itself() {
    let mut w = world();
    let err =
        w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.owner), ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m.contains("last owner")),
        "{err:?}"
    );
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn last_owner_cannot_demote_itself() {
    let mut w = world();
    let err =
        w.s.grant_role(
            NOW,
            &op(&w.owner, "role.grant", &pk_hex(&w.owner), "worker"),
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m.contains("last owner")),
        "{err:?}"
    );
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn an_owner_can_be_revoked_while_another_active_owner_remains() {
    let mut w = world();
    w.s.grant_role(NOW, &op(&w.owner, "role.grant", &pk_hex(&w.alice), "owner"))
        .unwrap();
    assert_eq!(w.s.role_of(&w.alice.public()).unwrap(), Role::Owner);
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .unwrap();
    assert!(matches!(
        w.s.role_of(&w.alice.public()),
        Err(StoreError::Invalid("identity revoked"))
    ));
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn revoked_owner_does_not_count_toward_the_last_owner() {
    let mut w = world();
    w.s.grant_role(NOW, &op(&w.owner, "role.grant", &pk_hex(&w.alice), "owner"))
        .unwrap();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .unwrap();
    let err =
        w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.owner), ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(m) if m.contains("last owner")),
        "{err:?}"
    );
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn demoting_one_of_two_active_owners_succeeds() {
    let mut w = world();
    w.s.grant_role(NOW, &op(&w.owner, "role.grant", &pk_hex(&w.alice), "owner"))
        .unwrap();
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&w.alice), "worker"),
    )
    .unwrap();
    assert_eq!(w.s.role_of(&w.alice.public()).unwrap(), Role::Worker);
    assert_eq!(w.s.role_of(&w.owner.public()).unwrap(), Role::Owner);
}

#[test]
fn controller_cannot_grant_owner() {
    let mut w = world();
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&w.carol), "controller"),
    )
    .unwrap();
    let err =
        w.s.grant_role(NOW, &op(&w.carol, "role.grant", &pk_hex(&w.bob), "owner"))
            .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Transition(TransitionError::Forbidden(Role::Controller))
        ),
        "{err:?}"
    );
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
}

#[test]
fn controller_cannot_revoke_or_regrant_a_peer_controller() {
    let mut w = with_two_controllers();
    let err =
        w.s.revoke_identity(NOW, &op(&w.carol, "identity.revoke", &pk_hex(&w.bob), ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Controller))
    ));
    let err =
        w.s.grant_role(
            NOW,
            &op(&w.carol, "role.grant", &pk_hex(&w.bob), "supervisor"),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Controller))
    ));
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Controller);
}

#[test]
fn controller_can_grant_supervisor_to_a_worker() {
    let mut w = with_two_controllers();
    w.s.grant_role(
        NOW,
        &op(&w.carol, "role.grant", &pk_hex(&w.alice), "supervisor"),
    )
    .unwrap();
    assert_eq!(w.s.role_of(&w.alice.public()).unwrap(), Role::Supervisor);
}

#[test]
fn owner_can_revoke_a_controller() {
    let mut w = with_two_controllers();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    assert!(matches!(
        w.s.role_of(&w.bob.public()),
        Err(StoreError::Invalid("identity revoked"))
    ));
}

#[test]
fn grant_to_unknown_identity_is_invalid() {
    let mut w = world();
    let err =
        w.s.grant_role(NOW, &op(&w.owner, "role.grant", &pk_hex(&w.eve), "worker"))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("unknown identity")));
    assert!(matches!(
        w.s.role_of(&w.eve.public()),
        Err(StoreError::Invalid("unknown identity"))
    ));
}

#[test]
fn grant_to_revoked_identity_is_invalid_and_does_not_restore_it() {
    let mut w = world();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .unwrap();
    let err =
        w.s.grant_role(
            NOW,
            &op(&w.owner, "role.grant", &pk_hex(&w.alice), "supervisor"),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
    assert!(matches!(
        w.s.role_of(&w.alice.public()),
        Err(StoreError::Invalid("identity revoked"))
    ));
}

#[test]
fn unknown_role_name_is_invalid() {
    let mut w = world();
    let err =
        w.s.grant_role(NOW, &op(&w.owner, "role.grant", &pk_hex(&w.bob), "god"))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("unknown role")),
        "{err:?}"
    );
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
}

#[test]
fn non_canonical_subject_is_invalid() {
    let mut w = world();
    let canonical = pk_hex(&w.bob);
    let rest = &canonical[2..];
    for subject in [canonical.to_uppercase(), format!("+f{rest}")] {
        let err =
            w.s.grant_role(NOW, &op(&w.owner, "role.grant", &subject, "supervisor"))
                .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Invalid("subject must be a 64-char hex key")
            ),
            "subject {subject}: {err:?}"
        );
    }
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Worker);
}

#[test]
fn empty_or_oversized_nonce_is_invalid() {
    let mut w = world();
    make_card(&mut w, "c1");
    for nonce in [String::new(), "n".repeat(129)] {
        let evidence = op_n(&w.alice, "card.evidence", "c1", "run.log", &nonce);
        let err = w.s.add_evidence(NOW, &evidence).unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid("nonce must be 1..=128 bytes")),
            "nonce of {} bytes: {err:?}",
            nonce.len()
        );
    }
}

#[test]
fn oversized_subject_is_invalid() {
    let mut w = world();
    let subject = "c".repeat(257);
    let err =
        w.s.create_card(
            NOW,
            &op(&w.alice, "card.create", &subject, r#"{"title":"t"}"#),
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("subject longer than 256 bytes")),
        "{err:?}"
    );
}

#[test]
fn oversized_arg_is_invalid() {
    let mut w = world();
    make_card(&mut w, "c1");
    let arg = "x".repeat(64 * 1024 + 1);
    let err =
        w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", &arg))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("arg longer than 64 KiB")),
        "{err:?}"
    );
}

#[test]
fn size_limits_accept_exactly_their_maximum() {
    let mut w = world();
    make_card(&mut w, "c1");
    let max_nonce = "n".repeat(128);
    w.s.add_evidence(
        NOW,
        &op_n(&w.alice, "card.evidence", "c1", "run.log", &max_nonce),
    )
    .unwrap();
    let max_subject = "c".repeat(256);
    w.s.create_card(
        NOW,
        &op(&w.alice, "card.create", &max_subject, r#"{"title":"t"}"#),
    )
    .unwrap();
    let max_arg = "x".repeat(64 * 1024);
    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c1", &max_arg))
        .unwrap();
}

#[test]
fn rejected_operation_leaves_no_event_and_its_nonce_is_reusable() {
    let mut w = world();
    let grant = op(&w.alice, "role.grant", &pk_hex(&w.bob), "supervisor");
    let before = w.s.event_count().unwrap();
    let err = w.s.grant_role(NOW, &grant).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.event_count().unwrap(), before);

    // Once alice is a controller, the very same signed grant is valid.
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&w.alice), "controller"),
    )
    .unwrap();
    w.s.grant_role(NOW, &grant).unwrap();
    assert_eq!(w.s.role_of(&w.bob.public()).unwrap(), Role::Supervisor);
}

#[test]
fn unhex32_round_trips_canonical_lowercase_hex() {
    let pk = [0xab; 32];
    assert_eq!(unhex32(&hex(&pk)), Some(pk));
}

#[test]
fn unhex32_rejects_uppercase_plus_prefix_and_wrong_length() {
    let rest = "ab".repeat(31);
    let cases = [
        "AB".repeat(32),
        format!("+f{rest}"),
        "ab".repeat(31),
        "ab".repeat(33),
    ];
    for case in cases {
        assert_eq!(unhex32(&case), None, "input {case}");
    }
}
