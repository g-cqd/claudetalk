//! Public-API tests for channel administration: promoting and demoting members, the
//! last-admin guard on removal, key rotation after a revocation, and the reader views of
//! channels (list, info, member key).

mod common;
use common::*;

use agora_core::TransitionError;
use agora_crypto::{ChannelKey, Identity};
use agora_store::*;
use serde_json::{Value, json};

/// `{"pk", "role"}` entries for a channel.create arg.
fn member_json(members: &[(&Identity, &str)]) -> Vec<Value> {
    members
        .iter()
        .map(|(id, role)| json!({"pk": pk_hex(id), "role": role}))
        .collect()
}

/// Arg of a channel.role operation.
fn role_arg(id: &Identity, role: &str) -> String {
    json!({"pk": pk_hex(id), "role": role}).to_string()
}

/// Arg of a channel.remove operation for `id`, for a plain channel.
fn remove_arg(id: &Identity) -> String {
    json!({"pk": pk_hex(id)}).to_string()
}

/// Expected membership: (pubkey, role) pairs sorted by pubkey, as the store reports them.
fn roster(entries: &[(&Identity, &str)]) -> Vec<(Pub, String)> {
    let mut v: Vec<(Pub, String)> = entries
        .iter()
        .map(|(id, role)| (id.public(), role.to_string()))
        .collect();
    v.sort();
    v
}

/// The wrapped key `by` seals for each of `to`, for channel `ch` at `epoch`.
fn wraps(
    by: &Identity,
    ch: &str,
    epoch: i64,
    key: &ChannelKey,
    to: &[&Identity],
) -> Vec<(Pub, Vec<u8>)> {
    to.iter()
        .map(|r| {
            (
                r.public(),
                by.wrap_key(&r.public(), ch, epoch, key).unwrap(),
            )
        })
        .collect()
}

/// A fresh key for encrypted `ch` at `epoch`, wrapped by `by` for each of `to`, and the
/// channel.rotate arg whose digest binds those wraps.
fn rotation(
    by: &Identity,
    ch: &str,
    epoch: i64,
    to: &[&Identity],
) -> (ChannelKey, Vec<(Pub, Vec<u8>)>, String) {
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(by, ch, epoch, &key, to);
    let arg = json!({"keys": keys_digest(epoch, &keys)}).to_string();
    (key, keys, arg)
}

/// Plain group channel `ch` created by `by`, with `members` besides the creator.
fn create_plain(s: &mut Store, by: &Identity, ch: &str, members: &[(&Identity, &str)]) {
    let arg = json!({"kind": "group", "name": ch, "members": member_json(members)}).to_string();
    s.create_channel(NOW, &op(by, "channel.create", ch, &arg), &[])
        .unwrap();
}

/// Encrypted group channel `ch` created by `by` at epoch 0, with `members` besides the
/// creator and one wrapped key per member, the creator included. Returns the epoch-0 key.
fn create_encrypted(
    s: &mut Store,
    by: &Identity,
    ch: &str,
    members: &[(&Identity, &str)],
) -> ChannelKey {
    let key = ChannelKey::generate().unwrap();
    let mut everyone: Vec<&Identity> = vec![by];
    everyone.extend(members.iter().map(|(id, _)| *id));
    let keys = wraps(by, ch, 0, &key, &everyone);
    let arg = json!({
        "kind": "group",
        "name": ch,
        "members": member_json(members),
        "keys": keys_digest(0, &keys),
    })
    .to_string();
    s.create_channel(NOW, &op(by, "channel.create", ch, &arg), &keys)
        .unwrap();
    key
}

/// Expected reader view of a plain channel "p" created by alice at epoch 0.
fn plain_info(my_role: Option<&str>) -> ChannelInfo {
    ChannelInfo {
        id: "p".into(),
        kind: "group".into(),
        name: "p".into(),
        parent: None,
        epoch: 0,
        encrypted: false,
        rotation_pending: false,
        my_role: my_role.map(str::to_owned),
    }
}

/// Asserts that `key` opens what `expected` seals.
fn assert_same_key(key: &ChannelKey, expected: &ChannelKey) {
    let blob = expected.seal(b"probe", b"payload").unwrap();
    assert_eq!(key.open(b"probe", &blob).unwrap(), b"payload".to_vec());
}

/// Asserts that `res` is the outsider answer for `channel`: NotMember naming that id.
fn assert_not_member<T>(res: Result<T, StoreError>, channel: &str) {
    match res {
        Err(StoreError::NotMember(id)) => assert_eq!(id, channel),
        Err(other) => panic!("expected NotMember({channel}), got {other:?}"),
        Ok(_) => panic!("expected NotMember({channel}), got Ok"),
    }
}

#[test]
fn an_admin_promotes_a_member_to_admin() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let arg = role_arg(&w.bob, "admin");
    w.s.set_channel_role(NOW, &op(&w.alice, "channel.role", "c", &arg))
        .unwrap();
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "admin")])
    );
}

#[test]
fn an_admin_demotes_another_admin_to_member() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "admin")]);
    let arg = role_arg(&w.bob, "member");
    w.s.set_channel_role(NOW, &op(&w.alice, "channel.role", "c", &arg))
        .unwrap();
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn a_worker_that_is_not_an_admin_cannot_change_roles() {
    let mut w = world();
    create_plain(
        &mut w.s,
        &w.alice,
        "c",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let before = w.s.event_count().unwrap();
    let arg = role_arg(&w.carol, "admin");
    let res =
        w.s.set_channel_role(NOW, &op(&w.bob, "channel.role", "c", &arg));
    assert!(matches!(
        res,
        Err(StoreError::Transition(TransitionError::Forbidden(_)))
    ));
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[
            (&w.alice, "admin"),
            (&w.bob, "member"),
            (&w.carol, "member")
        ])
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn a_worker_outside_a_missing_channel_gets_forbidden_not_not_found() {
    let mut w = world();
    let arg = role_arg(&w.carol, "admin");
    let res =
        w.s.set_channel_role(NOW, &op(&w.bob, "channel.role", "missing", &arg));
    assert!(matches!(
        res,
        Err(StoreError::Transition(TransitionError::Forbidden(_)))
    ));
}

#[test]
fn a_supervisor_outside_the_channel_may_change_its_roles() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let arg = role_arg(&w.bob, "admin");
    w.s.set_channel_role(NOW, &op(&w.sup, "channel.role", "c", &arg))
        .unwrap();
    // The supervisor changed the role without joining the channel.
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "admin")])
    );
}

#[test]
fn changing_the_role_of_a_non_member_is_refused_as_not_a_member() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let before = w.s.event_count().unwrap();
    let arg = role_arg(&w.carol, "admin");
    let res =
        w.s.set_channel_role(NOW, &op(&w.alice, "channel.role", "c", &arg));
    assert_not_member(res, "c");
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn the_last_admin_cannot_demote_itself() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let arg = role_arg(&w.alice, "member");
    let res =
        w.s.set_channel_role(NOW, &op(&w.alice, "channel.role", "c", &arg));
    assert!(matches!(
        res,
        Err(StoreError::Invalid("cannot demote the last channel admin"))
    ));
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn restating_the_last_admins_own_role_is_a_no_op() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let before = w.s.event_count().unwrap();
    let arg = role_arg(&w.alice, "admin");
    w.s.set_channel_role(NOW, &op(&w.alice, "channel.role", "c", &arg))
        .unwrap();
    assert_eq!(w.s.event_count().unwrap(), before);
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn the_only_admin_cannot_leave_while_others_remain() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let before = w.s.event_count().unwrap();
    let arg = remove_arg(&w.alice);
    let res =
        w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "c", &arg), &[]);
    assert!(matches!(
        res,
        Err(StoreError::Invalid("cannot remove the last channel admin"))
    ));
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn a_supervisor_cannot_remove_the_only_admin_either() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "member")]);
    let arg = remove_arg(&w.alice);
    let res =
        w.s.remove_member(NOW, &op(&w.sup, "channel.remove", "c", &arg), &[]);
    assert!(matches!(
        res,
        Err(StoreError::Invalid("cannot remove the last channel admin"))
    ));
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.alice, "admin"), (&w.bob, "member")])
    );
}

#[test]
fn the_last_member_may_leave_an_empty_channel() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "solo", &[]);
    let arg = remove_arg(&w.alice);
    w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "solo", &arg), &[])
        .unwrap();
    assert!(w.s.channel_members("solo").unwrap().is_empty());
}

#[test]
fn the_only_admin_may_leave_once_another_admin_exists() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "c", &[(&w.bob, "admin")]);
    let arg = remove_arg(&w.alice);
    w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "c", &arg), &[])
        .unwrap();
    assert_eq!(
        w.s.channel_members("c").unwrap(),
        roster(&[(&w.bob, "admin")])
    );
}

#[test]
fn rotation_gives_every_member_the_new_key_at_the_next_epoch() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let (key1, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob, &w.carol]);
    let epoch =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys)
            .unwrap();
    assert_eq!(epoch, 1);
    assert_eq!(w.s.channel_epoch("e").unwrap(), 1);
    for member in [&w.alice, &w.bob, &w.carol] {
        let (epoch, wrapper, wrapped) = w.s.member_key(&member.public(), "e", Some(1)).unwrap();
        assert_eq!(epoch, 1);
        assert_eq!(wrapper, w.alice.public());
        let unwrapped = member.unwrap_key(&wrapper, "e", 1, &wrapped).unwrap();
        assert_same_key(&unwrapped, &key1);
    }
}

#[test]
fn rotation_refuses_keys_that_leave_out_a_member() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let (_, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob]);
    let before = w.s.event_count().unwrap();
    let res =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys);
    assert!(matches!(res, Err(StoreError::Invalid(_))));
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn rotation_refuses_keys_for_a_non_member() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    // Every member is covered, but sup, who is not in the channel, is also given a key.
    let (_, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob, &w.carol, &w.sup]);
    let before = w.s.event_count().unwrap();
    let res =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys);
    assert!(matches!(res, Err(StoreError::Invalid(_))));
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn rotation_refuses_a_digest_bound_to_the_old_epoch() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let (_, keys, _) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob, &w.carol]);
    // The keys are for epoch 1, but the signed digest names epoch 0.
    let arg = json!({"keys": keys_digest(0, &keys)}).to_string();
    let res =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys);
    assert!(matches!(
        res,
        Err(StoreError::Invalid("keys digest mismatch"))
    ));
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
}

#[test]
fn rotation_refuses_a_plain_channel() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[(&w.bob, "member")]);
    let before = w.s.event_count().unwrap();
    let arg = json!({"keys": keys_digest(1, &[])}).to_string();
    let res =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "p", &arg), &[]);
    assert!(matches!(res, Err(StoreError::Invalid(_))));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn a_worker_that_is_not_an_admin_cannot_rotate() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let (_, keys, arg) = rotation(&w.bob, "e", 1, &[&w.alice, &w.bob, &w.carol]);
    let before = w.s.event_count().unwrap();
    let res =
        w.s.rotate_channel_key(NOW, &op(&w.bob, "channel.rotate", "e", &arg), &keys);
    assert!(matches!(
        res,
        Err(StoreError::Transition(TransitionError::Forbidden(_)))
    ));
    assert_eq!(w.s.channel_epoch("e").unwrap(), 0);
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn revoking_a_member_flags_the_channel_until_an_admin_rotates_it() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let alice = w.alice.public();
    assert!(!w.s.channel_info(&alice, "e").unwrap().0.rotation_pending);
    let revoke = op(&w.owner, "identity.revoke", &pk_hex(&w.carol), "");
    w.s.revoke_identity(NOW, &revoke).unwrap();
    assert!(w.s.channel_info(&alice, "e").unwrap().0.rotation_pending);
    let (_, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob]);
    w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys)
        .unwrap();
    assert!(!w.s.channel_info(&alice, "e").unwrap().0.rotation_pending);
}

#[test]
fn an_admin_removal_that_rotates_clears_a_pending_rotation() {
    let mut w = world();
    create_encrypted(
        &mut w.s,
        &w.alice,
        "e",
        &[(&w.bob, "member"), (&w.carol, "member")],
    );
    let revoke = op(&w.owner, "identity.revoke", &pk_hex(&w.carol), "");
    w.s.revoke_identity(NOW, &revoke).unwrap();
    let alice = w.alice.public();
    assert!(w.s.channel_info(&alice, "e").unwrap().0.rotation_pending);

    // Alice removes bob, which is a full rotation to epoch 1 for the remaining member, alice.
    let key = ChannelKey::generate().unwrap();
    let keys = wraps(&w.alice, "e", 1, &key, &[&w.alice]);
    let arg = json!({"pk": pk_hex(&w.bob), "keys": keys_digest(1, &keys)}).to_string();
    w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "e", &arg), &keys)
        .unwrap();
    assert_eq!(w.s.channel_epoch("e").unwrap(), 1);
    assert!(!w.s.channel_info(&alice, "e").unwrap().0.rotation_pending);
}

#[test]
fn list_channels_returns_only_my_channels_in_id_order() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "b", &[]);
    create_plain(&mut w.s, &w.alice, "a", &[(&w.bob, "member")]);
    create_plain(&mut w.s, &w.bob, "x", &[]);
    let mine = w.s.list_channels(&w.alice.public()).unwrap();
    let ids: Vec<&str> = mine.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn list_channels_reports_my_own_role_in_each_channel() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "a", &[(&w.bob, "member")]);
    create_plain(&mut w.s, &w.bob, "x", &[]);
    let mine = w.s.list_channels(&w.bob.public()).unwrap();
    let roles: Vec<(&str, Option<&str>)> = mine
        .iter()
        .map(|c| (c.id.as_str(), c.my_role.as_deref()))
        .collect();
    assert_eq!(roles, [("a", Some("member")), ("x", Some("admin"))]);
}

#[test]
fn list_channels_refuses_an_unregistered_reader() {
    let w = world();
    let res = w.s.list_channels(&w.eve.public());
    assert!(matches!(res, Err(StoreError::Invalid(_))));
}

#[test]
fn a_member_sees_the_channel_with_its_own_role_and_the_full_roster() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[(&w.bob, "member")]);
    let (info, members) = w.s.channel_info(&w.bob.public(), "p").unwrap();
    assert_eq!(info, plain_info(Some("member")));
    assert_eq!(members, roster(&[(&w.alice, "admin"), (&w.bob, "member")]));
}

#[test]
fn a_supervisor_outside_the_channel_sees_it_with_no_role_of_its_own() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[(&w.bob, "member")]);
    let (info, members) = w.s.channel_info(&w.sup.public(), "p").unwrap();
    assert_eq!(info, plain_info(None));
    assert_eq!(members, roster(&[(&w.alice, "admin"), (&w.bob, "member")]));
}

#[test]
fn a_worker_outside_gets_not_member_for_an_existing_and_a_missing_channel() {
    let mut w = world();
    create_plain(&mut w.s, &w.alice, "p", &[(&w.bob, "member")]);
    assert_not_member(w.s.channel_info(&w.carol.public(), "p"), "p");
    assert_not_member(w.s.channel_info(&w.carol.public(), "missing"), "missing");
}

#[test]
fn member_key_without_an_epoch_returns_the_current_epochs_key() {
    let mut w = world();
    create_encrypted(&mut w.s, &w.alice, "e", &[(&w.bob, "member")]);
    let (key1, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob]);
    w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys)
        .unwrap();
    let (epoch, wrapper, wrapped) = w.s.member_key(&w.bob.public(), "e", None).unwrap();
    assert_eq!(epoch, 1);
    let unwrapped = w.bob.unwrap_key(&wrapper, "e", 1, &wrapped).unwrap();
    assert_same_key(&unwrapped, &key1);
}

#[test]
fn member_key_with_an_epoch_returns_the_key_of_that_epoch() {
    let mut w = world();
    let key0 = create_encrypted(&mut w.s, &w.alice, "e", &[(&w.bob, "member")]);
    let (_, keys, arg) = rotation(&w.alice, "e", 1, &[&w.alice, &w.bob]);
    w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys)
        .unwrap();
    let (epoch, wrapper, wrapped) = w.s.member_key(&w.bob.public(), "e", Some(0)).unwrap();
    assert_eq!(epoch, 0);
    let unwrapped = w.bob.unwrap_key(&wrapper, "e", 0, &wrapped).unwrap();
    assert_same_key(&unwrapped, &key0);
}

#[test]
fn member_key_refuses_a_non_member_for_an_existing_and_a_missing_channel() {
    let mut w = world();
    create_encrypted(&mut w.s, &w.alice, "e", &[(&w.bob, "member")]);
    assert_not_member(w.s.member_key(&w.carol.public(), "e", None), "e");
    assert_not_member(
        w.s.member_key(&w.carol.public(), "missing", None),
        "missing",
    );
}
