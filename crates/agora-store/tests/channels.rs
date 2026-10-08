//! Public-API tests for channels: creation and its validation, adding members, removing
//! them with key rotation, and the wrapped-key history kept per epoch.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::{ChannelKey, Identity};
use agora_store::*;
use serde_json::json;

/// The key `by` wraps for each of `to`, for channel `ch` at `epoch`.
fn wraps(
    by: &Identity,
    ch: &str,
    epoch: i64,
    key: &ChannelKey,
    to: &[&Identity],
) -> Vec<(Pub, Vec<u8>)> {
    let aad = format!("{ch}#{epoch}");
    to.iter()
        .map(|r| (r.public(), by.wrap_key(&r.public(), &aad, key).unwrap()))
        .collect()
}

/// What `to` obtains by unwrapping `from`'s wrapped key for channel `ch` at `epoch`.
fn unwrap_as(to: &Identity, from: &Identity, ch: &str, epoch: i64, wrapped: &[u8]) -> ChannelKey {
    to.unwrap_key(&from.public(), &format!("{ch}#{epoch}"), wrapped)
        .unwrap()
}

/// Asserts that `unwrapped` behaves as `original`: it opens what `original` seals.
fn assert_same_key(original: &ChannelKey, unwrapped: &ChannelKey) {
    let blob = original.seal(b"probe", b"payload").unwrap();
    assert_eq!(
        unwrapped.open(b"probe", &blob).unwrap(),
        b"payload".to_vec()
    );
}

/// Arguments of a channel.create operation.
fn create_arg(kind: &str, name: &str, members: &[(&Identity, &str)]) -> String {
    let members: Vec<_> = members
        .iter()
        .map(|(id, role)| json!({ "pk": pk_hex(id), "role": role }))
        .collect();
    json!({ "kind": kind, "name": name, "members": members }).to_string()
}

/// Arguments of a channel.add operation.
fn add_arg(id: &Identity, role: &str) -> String {
    json!({ "pk": pk_hex(id), "role": role }).to_string()
}

/// The membership the store must report: (identity, role) pairs, sorted by public key.
fn roster(entries: &[(&Identity, &str)]) -> Vec<(Pub, String)> {
    let mut v: Vec<(Pub, String)> = entries
        .iter()
        .map(|(id, role)| (id.public(), role.to_string()))
        .collect();
    v.sort();
    v
}

fn has_member(s: &Store, ch: &str, id: &Identity) -> bool {
    s.channel_members(ch)
        .unwrap()
        .iter()
        .any(|(pk, _)| *pk == id.public())
}

/// Plain channel `ch` created by `by` with the given members.
fn create_plain(s: &mut Store, by: &Identity, ch: &str, members: &[(&Identity, &str)]) {
    s.create_channel(
        NOW,
        &op(by, "channel.create", ch, &create_arg("group", ch, members)),
        &[],
    )
    .unwrap();
}

/// Encrypted channel "e" at epoch 0, created by alice with bob and carol as members and one
/// key per member. Returns the epoch-0 key.
fn encrypted_e(w: &mut World) -> ChannelKey {
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member"), (&w.carol, "member")]);
    let keys = wraps(&w.alice, "e", 0, &key, &[&w.alice, &w.bob, &w.carol]);
    w.s.create_channel(NOW, &op(&w.alice, "channel.create", "e", &arg), &keys)
        .unwrap();
    key
}

/// Alice removes carol from encrypted "e", rotating to epoch 1 with a new key for alice and
/// bob. Returns the epoch-1 key.
fn rotate_out_carol(w: &mut World) -> ChannelKey {
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob]);
    w.s.remove_member(
        NOW,
        &op(&w.alice, "channel.remove", "e", &pk_hex(&w.carol)),
        &keys,
    )
    .unwrap();
    key
}

/// A create the store must refuse. Returns the error, after checking that it left no event
/// and no channel behind.
fn create_rejected(
    s: &mut Store,
    actor: &Identity,
    ch: &str,
    arg: &str,
    keys: &[(Pub, Vec<u8>)],
) -> StoreError {
    let before = s.event_count().unwrap();
    let err = s
        .create_channel(NOW, &op(actor, "channel.create", ch, arg), keys)
        .unwrap_err();
    assert_eq!(s.event_count().unwrap(), before);
    assert!(matches!(s.channel_epoch(ch), Err(StoreError::NotFound(_))));
    err
}

/// A removal of carol from encrypted "e" that the store must refuse. Returns the error, after
/// checking that the epoch, the membership and the event count are unchanged.
fn carol_removal_rejected(w: &mut World, keys: &[(Pub, Vec<u8>)]) -> StoreError {
    let before = w.s.event_count().unwrap();
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.alice, "channel.remove", "e", &pk_hex(&w.carol)),
            keys,
        )
        .unwrap_err();
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
    assert!(has_member(&w.s, "e", &w.carol));
    assert_eq!(w.s.event_count().unwrap(), before);
    // No partial key insert survived the rejection.
    assert_eq!(w.s.wrapped_key("e", &w.alice.public(), 1).unwrap(), None);
    assert_eq!(w.s.wrapped_key("e", &w.bob.public(), 1).unwrap(), None);
    err
}

/// Encrypted channel "e" at epoch 0 created by alice alone, with her own key. Returns that key.
fn encrypted_alone(w: &mut World) -> ChannelKey {
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 0, &key, &[&w.alice]);
    w.s.create_channel(
        NOW,
        &op(
            &w.alice,
            "channel.create",
            "e",
            &create_arg("group", "e", &[]),
        ),
        &keys,
    )
    .unwrap();
    key
}

// ---- creation ----

#[test]
fn creating_a_plain_channel_makes_the_creator_admin() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
    assert_eq!(w.s.channel_epoch("g").unwrap(), 0);
    assert_eq!(w.s.wrapped_key("g", &w.alice.public(), 0).unwrap(), None);
    assert_eq!(w.s.wrapped_key("g", &w.bob.public(), 0).unwrap(), None);
}

#[test]
fn create_rejects_an_unknown_member() {
    let mut w = world();
    let arg = create_arg("group", "g", &[(&w.eve, "member")]);
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_a_revoked_member() {
    let mut w = world();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let arg = create_arg("group", "g", &[(&w.bob, "member")]);
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_a_duplicate_member() {
    let mut w = world();
    let arg = create_arg("group", "g", &[(&w.bob, "member"), (&w.bob, "member")]);
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn a_creator_listed_as_a_member_becomes_admin() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "g",
        &[(&w.alice, "member"), (&w.bob, "member")],
    );
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn create_rejects_an_uppercase_member_key() {
    let mut w = world();
    let upper = pk_hex(&w.bob).to_uppercase();
    assert_ne!(upper, pk_hex(&w.bob));
    let arg =
        json!({ "kind": "group", "name": "g", "members": [{ "pk": upper, "role": "member" }] })
            .to_string();
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_an_existing_channel_id() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let before = w.s.event_count().unwrap();
    let arg = create_arg("group", "g", &[(&w.carol, "member")]);
    let err =
        w.s.create_channel(NOW, &op(&w.sup, "channel.create", "g", &arg), &[])
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("channel already exists")));
    assert_eq!(w.s.event_count().unwrap(), before);
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_worker_who_is_not_in_the_parent_cannot_create_a_child() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[]);
    let arg = json!({ "kind": "thread", "name": "c", "parent": "p", "members": [] }).to_string();
    let err = create_rejected(&mut w.s, &w.bob, "c", &arg, &[]);
    assert!(matches!(err, StoreError::NotMember(ref p) if p == "p"));
}

#[test]
fn a_supervisor_can_create_a_child_of_a_channel_it_is_not_in() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[]);
    let arg = json!({ "kind": "thread", "name": "c", "parent": "p", "members": [] }).to_string();
    w.s.create_channel(NOW, &op(&w.sup, "channel.create", "c", &arg), &[])
        .unwrap();
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.sup, "admin")])
    );
}

#[test]
fn a_member_of_the_parent_can_create_a_child() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[]);
    let arg = json!({ "kind": "thread", "name": "c", "parent": "p", "members": [] }).to_string();
    w.s.create_channel(NOW, &op(&w.alice, "channel.create", "c", &arg), &[])
        .unwrap();
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin")])
    );
}

#[test]
fn create_rejects_an_unknown_kind() {
    let mut w = world();
    let arg = create_arg("lobby", "g", &[]);
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_a_bad_member_role() {
    let mut w = world();
    let arg = create_arg("group", "g", &[(&w.bob, "moderator")]);
    let err = create_rejected(&mut w.s, &w.alice, "g", &arg, &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_malformed_args() {
    let mut w = world();
    let err = create_rejected(&mut w.s, &w.alice, "g", "{not json", &[]);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn observers_cannot_create_channels() {
    let mut w = world();
    let arg = create_arg("group", "g", &[]);
    let err = create_rejected(&mut w.s, &w.olive, "g", &arg, &[]);
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
}

#[test]
fn create_with_a_missing_parent_is_not_found() {
    let mut w = world();
    let arg = json!({ "kind": "thread", "name": "t", "parent": "nope", "members": [] }).to_string();
    let err = create_rejected(&mut w.s, &w.alice, "t", &arg, &[]);
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[test]
fn encrypted_create_gives_each_member_a_key_for_epoch_zero() {
    let mut w = world();
    let key = encrypted_e(&mut w);
    let bob_wrapped =
        w.s.wrapped_key("e", &w.bob.public(), 0)
            .unwrap()
            .expect("bob has a key");
    assert_same_key(&key, &unwrap_as(&w.bob, &w.alice, "e", 0, &bob_wrapped));
    assert!(
        w.s.wrapped_key("e", &w.alice.public(), 0)
            .unwrap()
            .is_some()
    );
    assert!(
        w.s.wrapped_key("e", &w.carol.public(), 0)
            .unwrap()
            .is_some()
    );
}

#[test]
fn encrypted_create_rejects_a_missing_member_key() {
    let mut w = world();
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member"), (&w.carol, "member")]);
    let keys = wraps(&w.alice, "e", 0, &key, &[&w.alice, &w.bob]);
    let err = create_rejected(&mut w.s, &w.alice, "e", &arg, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn encrypted_create_rejects_a_key_for_a_non_member() {
    let mut w = world();
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member"), (&w.carol, "member")]);
    let keys = wraps(
        &w.alice,
        "e",
        0,
        &key,
        &[&w.alice, &w.bob, &w.carol, &w.sup],
    );
    let err = create_rejected(&mut w.s, &w.alice, "e", &arg, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn encrypted_create_rejects_a_duplicate_key_entry() {
    let mut w = world();
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member"), (&w.carol, "member")]);
    // Every member has a key and no key is for a non-member, but carol's key appears twice.
    let keys = wraps(
        &w.alice,
        "e",
        0,
        &key,
        &[&w.alice, &w.bob, &w.carol, &w.carol],
    );
    let err = create_rejected(&mut w.s, &w.alice, "e", &arg, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_a_wrapped_key_one_byte_short() {
    let mut w = world();
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member")]);
    let mut keys = wraps(&w.alice, "e", 0, &key, &[&w.alice, &w.bob]);
    keys[1].1.truncate(WRAPPED_KEY_LEN - 1);
    let err = create_rejected(&mut w.s, &w.alice, "e", &arg, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn create_rejects_a_wrapped_key_one_byte_long() {
    let mut w = world();
    let key = ChannelKey::generate().unwrap();
    let arg = create_arg("group", "e", &[(&w.bob, "member")]);
    let mut keys = wraps(&w.alice, "e", 0, &key, &[&w.alice, &w.bob]);
    keys[1].1.push(0);
    let err = create_rejected(&mut w.s, &w.alice, "e", &arg, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn a_real_wrapped_key_is_exactly_the_length_the_store_requires() {
    assert_eq!(WRAPPED_KEY_LEN, 72);
    let w = world();
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 0, &key, &[&w.bob]);
    assert_eq!(keys[0].1.len(), WRAPPED_KEY_LEN);
}

// ---- adding members ----

#[test]
fn admin_adds_a_member_to_a_plain_channel() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    w.s.add_member(
        NOW,
        &op(&w.alice, "channel.add", "g", &add_arg(&w.sup, "member")),
        None,
    )
    .unwrap();
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member"), (&w.sup, "member")])
    );
}

#[test]
fn a_worker_who_is_not_an_admin_cannot_add_members() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.bob, "channel.add", "g", &add_arg(&w.carol, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_global_supervisor_can_add_members_without_being_one() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    w.s.add_member(
        NOW,
        &op(&w.sup, "channel.add", "g", &add_arg(&w.carol, "member")),
        None,
    )
    .unwrap();
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[
            (&w.alice, "admin"),
            (&w.bob, "member"),
            (&w.carol, "member")
        ])
    );
}

#[test]
fn adding_an_existing_member_is_rejected() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "g", &add_arg(&w.bob, "admin")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn adding_an_unknown_identity_is_rejected() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "g", &add_arg(&w.eve, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn adding_a_revoked_identity_is_rejected() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.carol), ""))
        .unwrap();
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "g", &add_arg(&w.carol, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_plain_channel_add_rejects_a_wrapped_key() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "g", &add_arg(&w.carol, "member")),
            Some(vec![0; WRAPPED_KEY_LEN]),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn an_encrypted_channel_add_requires_a_wrapped_key() {
    let mut w = world();
    encrypted_e(&mut w);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        w.s.channel_members("e").unwrap(),
        roster(&[
            (&w.alice, "admin"),
            (&w.bob, "member"),
            (&w.carol, "member")
        ])
    );
}

#[test]
fn an_encrypted_channel_add_rejects_an_empty_wrapped_key() {
    let mut w = world();
    encrypted_e(&mut w);
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
            Some(vec![]),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(!has_member(&w.s, "e", &w.sup));
}

#[test]
fn an_encrypted_channel_add_rejects_a_wrapped_key_one_byte_short() {
    let mut w = world();
    encrypted_e(&mut w);
    let short = vec![0; WRAPPED_KEY_LEN - 1];
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
            Some(short),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(!has_member(&w.s, "e", &w.sup));
}

#[test]
fn an_encrypted_channel_add_rejects_a_wrapped_key_one_byte_long() {
    let mut w = world();
    encrypted_e(&mut w);
    let long = vec![0; WRAPPED_KEY_LEN + 1];
    let err =
        w.s.add_member(
            NOW,
            &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
            Some(long),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(!has_member(&w.s, "e", &w.sup));
}

#[test]
fn add_member_rejects_an_uppercase_public_key() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let upper = pk_hex(&w.sup).to_uppercase();
    assert_ne!(upper, pk_hex(&w.sup));
    let arg = json!({ "pk": upper, "role": "member" }).to_string();
    let err =
        w.s.add_member(NOW, &op(&w.alice, "channel.add", "g", &arg), None)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_non_admin_worker_adding_to_a_missing_channel_is_forbidden_not_not_found() {
    let mut w = world();
    let err =
        w.s.add_member(
            NOW,
            &op(&w.carol, "channel.add", "ghost", &add_arg(&w.bob, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
}

#[test]
fn a_supervisor_adding_to_a_missing_channel_is_not_found() {
    let mut w = world();
    let err =
        w.s.add_member(
            NOW,
            &op(&w.sup, "channel.add", "ghost", &add_arg(&w.bob, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[test]
fn an_encrypted_channel_add_stores_the_wrapped_key_at_epoch_zero() {
    let mut w = world();
    let key = encrypted_e(&mut w);
    let (_, blob) = wraps(&w.alice, "e", 0, &key, &[&w.sup]).remove(0);
    w.s.add_member(
        NOW,
        &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
        Some(blob.clone()),
    )
    .unwrap();
    assert_eq!(
        w.s.wrapped_key("e", &w.sup.public(), 0).unwrap(),
        Some(blob)
    );
}

#[test]
fn an_encrypted_channel_add_stores_the_wrapped_key_at_the_current_epoch() {
    let mut w = world();
    encrypted_e(&mut w);
    let new_key = rotate_out_carol(&mut w);
    let (_, blob) = wraps(&w.alice, "e", 1, &new_key, &[&w.sup]).remove(0);
    w.s.add_member(
        NOW,
        &op(&w.alice, "channel.add", "e", &add_arg(&w.sup, "member")),
        Some(blob.clone()),
    )
    .unwrap();
    assert_eq!(
        w.s.wrapped_key("e", &w.sup.public(), 1).unwrap(),
        Some(blob)
    );
    assert_eq!(w.s.wrapped_key("e", &w.sup.public(), 0).unwrap(), None);
}

// ---- removing members and rotating keys ----

#[test]
fn removing_a_member_advances_the_epoch_and_drops_them() {
    let mut w = world();
    encrypted_e(&mut w);
    rotate_out_carol(&mut w);
    assert_eq!(w.s.channel_epoch("e").unwrap(), 1);
    assert_eq!(
        w.s.channel_members("e").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn rotation_gives_the_new_key_only_to_the_remaining_members() {
    let mut w = world();
    encrypted_e(&mut w);
    let new_key = rotate_out_carol(&mut w);
    assert_eq!(w.s.wrapped_key("e", &w.carol.public(), 1).unwrap(), None);
    let bob_wrapped =
        w.s.wrapped_key("e", &w.bob.public(), 1)
            .unwrap()
            .expect("bob has the new key");
    assert_same_key(&new_key, &unwrap_as(&w.bob, &w.alice, "e", 1, &bob_wrapped));
}

#[test]
fn rotation_with_keys_missing_a_remaining_member_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice]);
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn rotation_with_a_key_for_the_removed_member_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob, &w.carol]);
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn rotation_with_a_duplicate_key_entry_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    // The remaining members are alice and bob, every one has a key, but bob's key appears twice.
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob, &w.bob]);
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn rotation_with_an_empty_wrapped_key_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    let mut keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob]);
    keys[1].1.clear();
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn rotation_with_a_wrapped_key_one_byte_short_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    let mut keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob]);
    keys[1].1.truncate(WRAPPED_KEY_LEN - 1);
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn rotation_with_a_wrapped_key_one_byte_long_is_rejected() {
    let mut w = world();
    encrypted_e(&mut w);
    let key = ChannelKey::generate().unwrap();
    let mut keys = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob]);
    keys[1].1.push(0);
    let err = carol_removal_rejected(&mut w, &keys);
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn last_member_leaving_an_encrypted_channel_rejects_keys() {
    let mut w = world();
    encrypted_alone(&mut w);
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice]);
    let before = w.s.event_count().unwrap();
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.alice, "channel.remove", "e", &pk_hex(&w.alice)),
            &keys,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
    assert!(has_member(&w.s, "e", &w.alice));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn last_member_leaving_an_encrypted_channel_keeps_it_encrypted() {
    let mut w = world();
    encrypted_alone(&mut w);
    w.s.remove_member(
        NOW,
        &op(&w.alice, "channel.remove", "e", &pk_hex(&w.alice)),
        &[],
    )
    .unwrap();
    assert_eq!(w.s.channel_epoch("e").unwrap(), 1);
    assert_eq!(w.s.channel_members("e").unwrap(), roster(&[]));

    // The channel is still encrypted, so an add without a wrapped key is refused.
    let err =
        w.s.add_member(
            NOW,
            &op(&w.sup, "channel.add", "e", &add_arg(&w.bob, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));

    // With a wrapped key the add is stored at the current epoch.
    let key = ChannelKey::generate().unwrap();
    let (_, blob) = wraps(&w.sup, "e", 1, &key, &[&w.bob]).remove(0);
    w.s.add_member(
        NOW,
        &op(&w.sup, "channel.add", "e", &add_arg(&w.bob, "member")),
        Some(blob.clone()),
    )
    .unwrap();
    assert_eq!(
        w.s.wrapped_key("e", &w.bob.public(), 1).unwrap(),
        Some(blob)
    );
}

#[test]
fn a_rejected_rotation_can_be_retried_with_the_same_signed_op() {
    let mut w = world();
    encrypted_e(&mut w);
    let remove = op(&w.alice, "channel.remove", "e", &pk_hex(&w.carol));
    let key = ChannelKey::generate().unwrap();

    let short = wraps(&w.alice, "e", 1, &key, &[&w.alice]);
    assert!(matches!(
        w.s.remove_member(NOW, &remove, &short),
        Err(StoreError::Invalid(_))
    ));

    // The keys are not part of the signed operation, so the same op succeeds with the right keys.
    let full = wraps(&w.alice, "e", 1, &key, &[&w.alice, &w.bob]);
    w.s.remove_member(NOW, &remove, &full).unwrap();
    assert_eq!(w.s.channel_epoch("e").unwrap(), 1);
    assert_eq!(
        w.s.channel_members("e").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

// ---- removal authorization on plain channels ----

#[test]
fn a_worker_who_is_not_an_admin_cannot_remove_another_member() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "g",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.bob, "channel.remove", "g", &pk_hex(&w.carol)),
            &[],
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[
            (&w.alice, "admin"),
            (&w.bob, "member"),
            (&w.carol, "member")
        ])
    );
}

#[test]
fn a_member_can_leave_by_themselves() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "g",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    w.s.remove_member(
        NOW,
        &op(&w.carol, "channel.remove", "g", &pk_hex(&w.carol)),
        &[],
    )
    .unwrap();
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_global_supervisor_can_remove_a_member() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "g",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    w.s.remove_member(
        NOW,
        &op(&w.sup, "channel.remove", "g", &pk_hex(&w.bob)),
        &[],
    )
    .unwrap();
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.carol, "member")])
    );
}

#[test]
fn an_observer_admin_cannot_add_or_remove_others_but_can_leave() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "g",
        &[(&w.olive, "admin"), (&w.bob, "member")],
    );

    let err =
        w.s.add_member(
            NOW,
            &op(&w.olive, "channel.add", "g", &add_arg(&w.carol, "member")),
            None,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.olive, "channel.remove", "g", &pk_hex(&w.bob)),
            &[],
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.olive, "admin"), (&w.bob, "member")])
    );

    w.s.remove_member(
        NOW,
        &op(&w.olive, "channel.remove", "g", &pk_hex(&w.olive)),
        &[],
    )
    .unwrap();
    assert!(!has_member(&w.s, "g", &w.olive));
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_non_member_worker_removing_from_a_missing_channel_is_forbidden() {
    let mut w = world();
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.carol, "channel.remove", "ghost", &pk_hex(&w.bob)),
            &[],
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
}

#[test]
fn a_supervisor_removing_from_a_missing_channel_is_not_found() {
    let mut w = world();
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.sup, "channel.remove", "ghost", &pk_hex(&w.bob)),
            &[],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[test]
fn remove_member_rejects_a_non_hex_argument() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "g", "zz"), &[])
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(has_member(&w.s, "g", &w.bob));
}

#[test]
fn remove_member_rejects_an_uppercase_argument() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let upper = pk_hex(&w.bob).to_uppercase();
    assert_ne!(upper, pk_hex(&w.bob));
    let err =
        w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "g", &upper), &[])
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(has_member(&w.s, "g", &w.bob));
}

#[test]
fn removing_a_non_member_is_not_a_member_error() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.alice, "channel.remove", "g", &pk_hex(&w.olive)),
            &[],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotMember(_)));
}

#[test]
fn a_plain_channel_removal_rejects_keys() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "g", 0, &key, &[&w.alice]);
    let err =
        w.s.remove_member(
            NOW,
            &op(&w.alice, "channel.remove", "g", &pk_hex(&w.bob)),
            &keys,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    assert!(has_member(&w.s, "g", &w.bob));
}

#[test]
fn a_plain_channel_removal_does_not_change_the_epoch() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "g", &[(&w.bob, "member")]);
    w.s.remove_member(
        NOW,
        &op(&w.alice, "channel.remove", "g", &pk_hex(&w.bob)),
        &[],
    )
    .unwrap();
    assert_eq!(w.s.channel_epoch("g").unwrap(), 0);
    assert_eq!(
        w.s.channel_members("g").unwrap(),
        roster(&[(&w.alice, "admin")])
    );
}

// ---- key history ----

#[test]
fn a_removed_member_keeps_old_epoch_keys_but_gets_none_after_later_rotations() {
    let mut w = world();
    let key0 = encrypted_e(&mut w);
    rotate_out_carol(&mut w);

    // A second rotation removes bob, leaving alice alone.
    let key2 = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 2, &key2, &[&w.alice]);
    w.s.remove_member(
        NOW,
        &op(&w.alice, "channel.remove", "e", &pk_hex(&w.bob)),
        &keys,
    )
    .unwrap();
    assert_eq!(w.s.channel_epoch("e").unwrap(), 2);

    let carol_old =
        w.s.wrapped_key("e", &w.carol.public(), 0)
            .unwrap()
            .expect("epoch-0 key is kept as history");
    assert_same_key(&key0, &unwrap_as(&w.carol, &w.alice, "e", 0, &carol_old));
    assert_eq!(w.s.wrapped_key("e", &w.carol.public(), 1).unwrap(), None);
    assert_eq!(w.s.wrapped_key("e", &w.carol.public(), 2).unwrap(), None);
}
