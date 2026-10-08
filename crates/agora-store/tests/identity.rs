//! Public-API tests for identities: bootstrap, role grants and forgery, unknown and
//! revoked keys, replay protection, nonce handling, tamper/clock rules, and the channel
//! effects of revocation with the identity status queries.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::{ChannelKey, Identity};
use agora_store::*;
use serde_json::json;

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

/// Creates channel `ch` as `creator`, with `others` as plain members. An encrypted channel
/// wraps one fresh key at epoch 0 for every member, the creator included.
fn make_channel(s: &mut Store, creator: &Identity, ch: &str, others: &[&Identity], encrypt: bool) {
    let keys: Vec<(Pub, Vec<u8>)> = if encrypt {
        let key = ChannelKey::generate().unwrap();
        std::iter::once(creator)
            .chain(others.iter().copied())
            .map(|m| {
                (
                    m.public(),
                    creator.wrap_key(&m.public(), ch, 0, &key).unwrap(),
                )
            })
            .collect()
    } else {
        vec![]
    };
    let listed: Vec<_> = others
        .iter()
        .map(|m| json!({"pk": pk_hex(m), "role": "member"}))
        .collect();
    let mut arg = json!({"kind": "group", "members": listed});
    if encrypt {
        arg["keys"] = json!(keys_digest(0, &keys));
    }
    s.create_channel(
        NOW,
        &op(creator, "channel.create", ch, &arg.to_string()),
        &keys,
    )
    .unwrap();
}

/// Alice creates a plain channel `plain` and an encrypted channel `secret`, each with
/// bob and carol as members. Alice is the admin of both.
fn with_plain_and_encrypted_channels() -> World {
    let mut w = world();
    make_channel(&mut w.s, &w.alice, "plain", &[&w.bob, &w.carol], false);
    make_channel(&mut w.s, &w.alice, "secret", &[&w.bob, &w.carol], true);
    w
}

/// Members of a channel, sorted by public key, as `channel_members` returns them.
fn sorted(mut members: Vec<(Pub, String)>) -> Vec<(Pub, String)> {
    members.sort();
    members
}

#[test]
fn revoking_a_member_removes_them_from_every_channel() {
    let mut w = with_plain_and_encrypted_channels();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let expected = sorted(vec![
        (w.alice.public(), "admin".to_string()),
        (w.carol.public(), "member".to_string()),
    ]);
    assert_eq!(w.s.channel_members("plain").unwrap(), expected);
    assert_eq!(w.s.channel_members("secret").unwrap(), expected);
}

#[test]
fn revoking_flags_only_the_encrypted_channels_for_rotation() {
    let mut w = with_plain_and_encrypted_channels();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let (plain, _) = w.s.channel_info(&w.alice.public(), "plain").unwrap();
    let (secret, _) = w.s.channel_info(&w.alice.public(), "secret").unwrap();
    assert_eq!(
        (plain.rotation_pending, secret.rotation_pending),
        (false, true)
    );
}

#[test]
fn revoking_appends_exactly_one_identity_revoked_event() {
    let mut w = with_plain_and_encrypted_channels();
    let before = w.s.event_count().unwrap();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    assert_eq!(w.s.event_count().unwrap() - before, 1);
}

#[test]
fn revoking_an_identity_in_no_channel_changes_no_channel() {
    let mut w = world();
    make_channel(&mut w.s, &w.alice, "plain", &[&w.bob], false);
    make_channel(&mut w.s, &w.alice, "secret", &[&w.bob], true);
    let plain_before = w.s.channel_members("plain").unwrap();
    let secret_before = w.s.channel_members("secret").unwrap();
    // Carol belongs to neither channel.
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.carol), ""))
        .unwrap();
    assert_eq!(w.s.channel_members("plain").unwrap(), plain_before);
    assert_eq!(w.s.channel_members("secret").unwrap(), secret_before);
    let (secret, _) = w.s.channel_info(&w.alice.public(), "secret").unwrap();
    assert!(!secret.rotation_pending);
}

#[test]
fn revoking_the_sole_channel_admin_is_allowed_and_leaves_no_admin() {
    let mut w = world();
    make_channel(&mut w.s, &w.alice, "plain", &[&w.bob], false);
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .unwrap();
    assert_eq!(
        w.s.channel_members("plain").unwrap(),
        vec![(w.bob.public(), "member".to_string())]
    );
}

#[test]
fn a_refused_revocation_changes_no_membership_rotation_or_event() {
    let mut w = with_plain_and_encrypted_channels();
    let members_before = w.s.channel_members("secret").unwrap();
    let before = w.s.event_count().unwrap();
    // Alice is a worker; revoking needs a controller or above.
    let err =
        w.s.revoke_identity(NOW, &op(&w.alice, "identity.revoke", &pk_hex(&w.bob), ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.channel_members("secret").unwrap(), members_before);
    assert_eq!(w.s.event_count().unwrap(), before);
    let (secret, _) = w.s.channel_info(&w.alice.public(), "secret").unwrap();
    assert!(!secret.rotation_pending);
    assert_eq!(
        w.s.identity_status(&w.bob.public()).unwrap(),
        IdentityStatus::Active(Role::Worker)
    );
}

#[test]
fn identity_status_is_unknown_for_a_key_never_registered() {
    let w = world();
    assert_eq!(
        w.s.identity_status(&w.eve.public()).unwrap(),
        IdentityStatus::Unknown
    );
}

#[test]
fn identity_status_reports_the_stored_role_of_active_identities() {
    let w = world();
    assert_eq!(
        w.s.identity_status(&w.owner.public()).unwrap(),
        IdentityStatus::Active(Role::Owner)
    );
    assert_eq!(
        w.s.identity_status(&w.alice.public()).unwrap(),
        IdentityStatus::Active(Role::Worker)
    );
}

#[test]
fn identity_status_is_revoked_whatever_the_former_role() {
    let mut w = world();
    for target in [&w.alice, &w.sup] {
        w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(target), ""))
            .unwrap();
    }
    assert_eq!(
        w.s.identity_status(&w.alice.public()).unwrap(),
        IdentityStatus::Revoked
    );
    assert_eq!(
        w.s.identity_status(&w.sup.public()).unwrap(),
        IdentityStatus::Revoked
    );
}

#[test]
fn owner_exists_is_false_on_a_fresh_store() {
    let s = Store::open_in_memory().unwrap();
    assert!(!s.owner_exists().unwrap());
}

#[test]
fn owner_exists_is_true_once_an_owner_is_bootstrapped() {
    let mut s = Store::open_in_memory().unwrap();
    s.bootstrap_owner(NOW, &Identity::from_seed([1; 32]).public())
        .unwrap();
    assert!(s.owner_exists().unwrap());
}
