//! Public-API tests for messages: signature and clock rules, membership and observer
//! rules, thread parents, blobs and size limits, the inbox and read cursors, encrypted
//! bodies, key rotation, and rejected posts.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::{ChannelKey, Identity, body_aad, content_hash};
use agora_store::*;

/// Unsigned message with the given fields. Callers sign it with `.sign(&identity)`.
fn msg(id: &str, ch: &str, ts: i64, encrypted: bool, epoch: i64, body: &[u8]) -> SignedMessage {
    SignedMessage {
        id: id.into(),
        channel: ch.into(),
        thread_of: None,
        sender: [0; 32],
        ts,
        encrypted,
        epoch,
        body: body.to_vec(),
        attachments: Vec::new(),
        sig: Vec::new(),
    }
}

/// Plaintext epoch-0 message signed by `who` at NOW.
fn plain(who: &Identity, id: &str, ch: &str, body: &[u8]) -> SignedMessage {
    msg(id, ch, NOW, false, 0, body).sign(who)
}

/// Plaintext reply to `parent`, signed by `who` at NOW.
fn reply(who: &Identity, id: &str, ch: &str, parent: &str, body: &[u8]) -> SignedMessage {
    let mut m = msg(id, ch, NOW, false, 0, body);
    m.thread_of = Some(parent.into());
    m.sign(who)
}

/// The `arg` of a channel.create for `members` (plain members), with an optional
/// `"keys"` digest for an encrypted channel.
fn members_arg(members: &[&Identity], keys: Option<String>) -> String {
    let list: Vec<String> = members
        .iter()
        .map(|m| format!(r#"{{"pk":"{}","role":"member"}}"#, pk_hex(m)))
        .collect();
    let keys_field = keys
        .map(|d| format!(r#","keys":"{d}""#))
        .unwrap_or_default();
    format!(
        r#"{{"kind":"group","members":[{}]{keys_field}}}"#,
        list.join(",")
    )
}

/// Plaintext group channel `id`, created by `creator`, with `members` added as plain members.
fn create_group(s: &mut Store, creator: &Identity, id: &str, members: &[&Identity]) {
    s.create_channel(
        NOW,
        &op(creator, "channel.create", id, &members_arg(members, None)),
        &[],
    )
    .unwrap();
}

/// Wraps `key` for each of `to` as `sender`, at channel `ch` and key epoch `epoch`.
fn wrap_all(
    sender: &Identity,
    ch: &str,
    epoch: i64,
    key: &ChannelKey,
    to: &[&Identity],
) -> Vec<(Pub, Vec<u8>)> {
    to.iter()
        .map(|m| {
            (
                m.public(),
                sender.wrap_key(&m.public(), ch, epoch, key).unwrap(),
            )
        })
        .collect()
}

/// Encrypted group channel `id`, created by `creator` (who is always a member), with
/// `members` also keyed at epoch 0. Returns the epoch-0 key.
fn create_encrypted(
    s: &mut Store,
    creator: &Identity,
    id: &str,
    members: &[&Identity],
) -> ChannelKey {
    let key = ChannelKey::generate().unwrap();
    let everyone: Vec<&Identity> = std::iter::once(creator)
        .chain(members.iter().copied())
        .collect();
    let keys = wrap_all(creator, id, 0, &key, &everyone);
    let arg = members_arg(members, Some(keys_digest(0, &keys)));
    s.create_channel(NOW, &op(creator, "channel.create", id, &arg), &keys)
        .unwrap();
    key
}

/// Encrypted channel "e" with alice (admin), bob and carol at epoch 0. Alice then removes
/// carol, rotating to epoch 1 for alice and bob. Returns the epoch-1 key.
fn rotated(w: &mut World) -> ChannelKey {
    create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob, &w.carol]);
    remove_carol(w)
}

/// Alice removes carol from the encrypted channel "e", rotating to epoch 1 for alice and
/// bob. Returns the epoch-1 key.
fn remove_carol(w: &mut World) -> ChannelKey {
    let next = ChannelKey::generate().unwrap();
    let keys = wrap_all(&w.alice, "e", 1, &next, &[&w.alice, &w.bob]);
    let arg = format!(
        r#"{{"pk":"{}","keys":"{}"}}"#,
        pk_hex(&w.carol),
        keys_digest(1, &keys)
    );
    w.s.remove_member(NOW, &op(&w.alice, "channel.remove", "e", &arg), &keys)
        .unwrap();
    next
}

/// Stand-in ciphertext of the shortest length the store accepts for an encrypted body
/// (24-byte nonce plus 16-byte tag). The store never opens it, so its content is irrelevant.
fn opaque_ciphertext() -> Vec<u8> {
    vec![0u8; 40]
}

/// Message ids a member reads from `ch`, oldest first.
fn read_ids(s: &Store, who: &Identity, ch: &str) -> Vec<String> {
    s.read_channel(&who.public(), ch, 0, 100)
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect()
}

/// (channel, message id) pairs in `who`'s inbox, oldest first.
fn inbox_of(s: &Store, who: &Identity) -> Vec<(String, String)> {
    s.inbox(&who.public(), 100)
        .unwrap()
        .into_iter()
        .map(|(ch, row)| (ch, row.id))
        .collect()
}

/// Message ids of inbox rows, in order.
fn ids_of(rows: Vec<(String, MessageRow)>) -> Vec<String> {
    rows.into_iter().map(|(_, row)| row.id).collect()
}

/// Encrypted channel "e" with alice (admin), bob and carol at epoch 0, after the owner
/// revokes bob. The revocation flags "e" for key rotation.
fn revoked_bob(w: &mut World) {
    create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob, &w.carol]);
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn posted_message_reads_back_exactly_as_posted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let posted = plain(&w.alice, "m1", "c", b"hello bob");
    let seq = w.s.post_message(NOW, &posted).unwrap();
    let rows = w.s.read_channel(&w.bob.public(), "c", 0, 10).unwrap();
    assert_eq!(
        rows,
        vec![MessageRow {
            seq,
            id: "m1".into(),
            sender: w.alice.public(),
            body: b"hello bob".to_vec(),
            encrypted: false,
            epoch: 0,
            channel: "c".into(),
            thread_of: None,
            ts: NOW,
            attachments: vec![],
            sig: posted.sig.clone(),
        }]
    );
}

#[test]
fn read_channel_fills_thread_ts_sig_and_attachments_as_posted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let parent_seq =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"parent"))
            .unwrap();
    // Attachments are posted in an order that is neither hash order nor name order, so
    // a row that sorted them would fail.
    let mut blobs: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|t| w.s.put_blob(t.as_bytes()).unwrap())
        .collect();
    blobs.sort_by(|a, b| b.cmp(a));
    let names = ["c.bin", "b.bin", "a.bin"];
    let attachments: Vec<Attachment> = blobs
        .into_iter()
        .zip(names)
        .map(|(hash, name)| Attachment {
            hash,
            name: name.into(),
            mime: "application/octet-stream".into(),
        })
        .collect();
    let mut unsigned = msg("r1", "c", NOW, false, 0, b"see attached");
    unsigned.thread_of = Some("m1".into());
    unsigned.attachments = attachments.clone();
    let reply = unsigned.sign(&w.bob);
    let seq = w.s.post_message(NOW, &reply).unwrap();

    let rows =
        w.s.read_channel(&w.alice.public(), "c", parent_seq, 10)
            .unwrap();
    assert_eq!(
        rows,
        vec![MessageRow {
            seq,
            id: "r1".into(),
            sender: w.bob.public(),
            body: b"see attached".to_vec(),
            encrypted: false,
            epoch: 0,
            channel: "c".into(),
            thread_of: Some("m1".into()),
            ts: NOW,
            attachments,
            sig: reply.sig.clone(),
        }]
    );
}

#[test]
fn read_channel_pages_by_cursor_and_limit() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    let s2 =
        w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
            .unwrap();
    let s3 =
        w.s.post_message(NOW, &plain(&w.alice, "m3", "c", b"three"))
            .unwrap();
    let bob = w.bob.public();
    let ids = |rows: Vec<MessageRow>| rows.into_iter().map(|r| r.id).collect::<Vec<String>>();
    // The cursor is exclusive and the limit caps the page.
    assert_eq!(ids(w.s.read_channel(&bob, "c", s1, 1).unwrap()), vec!["m2"]);
    assert_eq!(
        ids(w.s.read_channel(&bob, "c", s2, 10).unwrap()),
        vec!["m3"]
    );
    assert_eq!(
        ids(w.s.read_channel(&bob, "c", s3, 10).unwrap()),
        Vec::<String>::new()
    );
}

#[test]
fn read_channel_and_inbox_clamp_the_page_to_500() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    for i in 0..501 {
        w.s.post_message(NOW, &plain(&w.alice, &format!("m{i}"), "c", b"page"))
            .unwrap();
    }
    let bob = w.bob.public();
    let page = w.s.read_channel(&bob, "c", 0, i64::MAX).unwrap();
    assert_eq!(page.len(), 500);
    assert_eq!((page[0].id.as_str(), page[499].id.as_str()), ("m0", "m499"));
    assert_eq!(w.s.inbox(&bob, i64::MAX).unwrap().len(), 500);
    assert_eq!(
        w.s.read_channel(&bob, "c", 0, -1).unwrap(),
        Vec::<MessageRow>::new()
    );
    assert_eq!(
        w.s.inbox(&bob, -1).unwrap(),
        Vec::<(String, MessageRow)>::new()
    );
}

#[test]
fn tampered_body_fails_signature_check() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let mut m = plain(&w.alice, "m1", "c", b"pay 10");
    m.body = b"pay 1000".to_vec();
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Crypto(_))
    ));
}

#[test]
fn spoofed_sender_fails_signature_check() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob, &w.carol]);
    let mut m = plain(&w.alice, "m1", "c", b"from alice");
    m.sender = w.bob.public();
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Crypto(_))
    ));
}

#[test]
fn tampered_epoch_fails_signature_check() {
    // The epoch is signed, so editing it must fail the signature before any epoch rule runs.
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let mut m = plain(&w.alice, "m1", "c", b"hi");
    m.epoch = 1;
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Crypto(_))
    ));
}

#[test]
fn tampered_channel_thread_or_attachment_fails_signature() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    create_group(&mut w.s, &w.alice, "d", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"parent"))
        .unwrap();
    let hash = w.s.put_blob(b"log contents").unwrap();

    // Each message is signed as valid, then one signed field is edited.
    let mut retargeted = plain(&w.alice, "m2", "c", b"signed for c");
    retargeted.channel = "d".into();
    let mut rethreaded = plain(&w.alice, "m3", "c", b"signed top-level");
    rethreaded.thread_of = Some("m1".into());
    let mut renamed = msg("m4", "c", NOW, false, 0, b"signed with run.log");
    renamed.attachments = vec![Attachment {
        hash,
        name: "run.log".into(),
        mime: "text/plain".into(),
    }];
    let mut renamed = renamed.sign(&w.alice);
    renamed.attachments[0].name = "other.log".into();

    for (what, m) in [
        ("channel", retargeted),
        ("thread", rethreaded),
        ("attachment name", renamed),
    ] {
        assert!(
            matches!(w.s.post_message(NOW, &m), Err(StoreError::Crypto(_))),
            "{what} edit"
        );
    }
    assert_eq!(read_ids(&w.s, &w.bob, "c"), vec!["m1"]);
    assert_eq!(read_ids(&w.s, &w.bob, "d"), Vec::<String>::new());
}

#[test]
fn timestamps_beyond_clock_skew_are_rejected() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    for (i, ts) in [NOW - MAX_SKEW_MS - 1, NOW + MAX_SKEW_MS + 1]
        .into_iter()
        .enumerate()
    {
        let m = msg(&format!("m{i}"), "c", ts, false, 0, b"skewed").sign(&w.alice);
        assert!(
            matches!(w.s.post_message(NOW, &m), Err(StoreError::Invalid(reason)) if reason.contains("clock skew")),
            "ts {ts}"
        );
    }
}

#[test]
fn timestamps_at_the_clock_skew_bound_are_accepted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    for (i, ts) in [NOW - MAX_SKEW_MS, NOW + MAX_SKEW_MS]
        .into_iter()
        .enumerate()
    {
        let m = msg(&format!("m{i}"), "c", ts, false, 0, b"on the edge").sign(&w.alice);
        let result = w.s.post_message(NOW, &m);
        assert!(result.is_ok(), "ts {ts}: {result:?}");
    }
}

#[test]
fn extreme_timestamps_are_rejected_for_messages() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    for (i, ts) in [i64::MIN, i64::MAX].into_iter().enumerate() {
        let m = msg(&format!("m{i}"), "c", ts, false, 0, b"far from the clock").sign(&w.alice);
        assert!(
            matches!(w.s.post_message(NOW, &m), Err(StoreError::Invalid(reason)) if reason.contains("clock skew")),
            "ts {ts}"
        );
    }
}

#[test]
fn unknown_identity_cannot_post() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let err =
        w.s.post_message(NOW, &plain(&w.eve, "m1", "c", b"who am i"))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("unknown identity")));
}

#[test]
fn revoked_member_cannot_post() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.bob, "m1", "c", b"before revoke"))
        .unwrap();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let err =
        w.s.post_message(NOW, &plain(&w.bob, "m2", "c", b"after revoke"))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
}

#[test]
fn revoked_reader_cannot_read_inbox_or_ack() {
    // Bob is still a channel member; only his identity is revoked.
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let seq =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"for bob"))
            .unwrap();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let bob = w.bob.public();
    assert!(matches!(
        w.s.read_channel(&bob, "c", 0, 10),
        Err(StoreError::Invalid("identity revoked"))
    ));
    assert!(matches!(
        w.s.inbox(&bob, 10),
        Err(StoreError::Invalid("identity revoked"))
    ));
    assert!(matches!(
        w.s.ack(&bob, "c", seq),
        Err(StoreError::Invalid("identity revoked"))
    ));
}

#[test]
fn unknown_identity_cannot_read_inbox_or_ack() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let seq =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"for bob"))
            .unwrap();
    let eve = w.eve.public();
    assert!(matches!(
        w.s.read_channel(&eve, "c", 0, 10),
        Err(StoreError::Invalid("unknown identity"))
    ));
    assert!(matches!(
        w.s.inbox(&eve, 10),
        Err(StoreError::Invalid("unknown identity"))
    ));
    assert!(matches!(
        w.s.ack(&eve, "c", seq),
        Err(StoreError::Invalid("unknown identity"))
    ));
}

#[test]
fn observer_member_cannot_post() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.olive]);
    let err =
        w.s.post_message(NOW, &plain(&w.olive, "m1", "c", b"watching"))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
}

#[test]
fn non_members_cannot_post_read_or_ack() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let seq =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"members only"))
            .unwrap();
    let carol = w.carol.public();
    assert!(matches!(
        w.s.post_message(NOW, &plain(&w.carol, "m2", "c", b"sneaky")),
        Err(StoreError::NotMember(_))
    ));
    assert!(matches!(
        w.s.read_channel(&carol, "c", 0, 10),
        Err(StoreError::NotMember(_))
    ));
    assert!(matches!(
        w.s.ack(&carol, "c", seq),
        Err(StoreError::NotMember(_))
    ));
}

#[test]
fn thread_parent_must_be_in_the_same_channel() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    create_group(&mut w.s, &w.alice, "d", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "d", b"parent lives in d"))
        .unwrap();
    let cross = reply(&w.alice, "r1", "c", "m1", b"reply in c");
    assert!(matches!(
        w.s.post_message(NOW, &cross),
        Err(StoreError::Invalid("thread parent not in this channel"))
    ));
    let dangling = reply(&w.alice, "r2", "c", "no-such-message", b"orphan");
    assert!(matches!(
        w.s.post_message(NOW, &dangling),
        Err(StoreError::Invalid("thread parent not in this channel"))
    ));
}

#[test]
fn thread_reply_to_a_message_in_the_same_channel_is_accepted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"parent"))
        .unwrap();
    w.s.post_message(NOW, &reply(&w.bob, "r1", "c", "m1", b"reply"))
        .unwrap();
    assert_eq!(read_ids(&w.s, &w.alice, "c"), vec!["m1", "r1"]);
}

#[test]
fn blob_round_trips_under_its_content_hash() {
    let mut w = world();
    let bytes = b"attachment bytes";
    let hash = w.s.put_blob(bytes).unwrap();
    assert_eq!(hash, content_hash(bytes));
    assert_eq!(w.s.get_blob(&hash).unwrap(), Some(bytes.to_vec()));
}

#[test]
fn message_with_a_stored_blob_attachment_is_accepted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let hash = w.s.put_blob(b"log contents").unwrap();
    let mut m = msg("m1", "c", NOW, false, 0, b"see attached");
    m.attachments = vec![Attachment {
        hash,
        name: "run.log".into(),
        mime: "text/plain".into(),
    }];
    w.s.post_message(NOW, &m.sign(&w.alice)).unwrap();
    assert_eq!(read_ids(&w.s, &w.bob, "c"), vec!["m1"]);
}

#[test]
fn message_naming_a_missing_blob_is_rejected() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let mut m = msg("m1", "c", NOW, false, 0, b"see attached");
    m.attachments = vec![Attachment {
        hash: content_hash(b"never uploaded"),
        name: "ghost.bin".into(),
        mime: "application/octet-stream".into(),
    }];
    let err = w.s.post_message(NOW, &m.sign(&w.alice)).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("attachment blob missing")
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "c"), Vec::<String>::new());
}

#[test]
fn message_id_is_bounded_at_256_bytes() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let longest = "x".repeat(256);
    assert!(
        w.s.post_message(NOW, &plain(&w.alice, &longest, "c", b"at the cap"))
            .is_ok()
    );
    for id in ["".to_string(), "x".repeat(257)] {
        let m = plain(&w.alice, &id, "c", b"over the cap");
        assert!(
            matches!(
                w.s.post_message(NOW, &m),
                Err(StoreError::Invalid("message id must be 1 to 256 bytes"))
            ),
            "id length {}",
            id.len()
        );
    }
}

#[test]
fn channel_and_thread_ids_are_bounded_at_256_bytes() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let long_channel = plain(&w.alice, "m1", &"c".repeat(257), b"channel over the cap");
    assert!(matches!(
        w.s.post_message(NOW, &long_channel),
        Err(StoreError::Invalid("channel id too long"))
    ));
    let mut long_thread = msg("m2", "c", NOW, false, 0, b"thread over the cap");
    long_thread.thread_of = Some("t".repeat(257));
    assert!(matches!(
        w.s.post_message(NOW, &long_thread.sign(&w.bob)),
        Err(StoreError::Invalid("thread id too long"))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "c"), Vec::<String>::new());
}

#[test]
fn attachment_count_is_bounded_at_32() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let mut parts = Vec::new();
    for i in 0..33 {
        let hash = w.s.put_blob(format!("part {i}").as_bytes()).unwrap();
        parts.push(Attachment {
            hash,
            name: format!("part{i}.bin"),
            mime: "application/octet-stream".into(),
        });
    }
    let mut at_cap = msg("m1", "c", NOW, false, 0, b"many parts");
    at_cap.attachments = parts[..32].to_vec();
    assert!(w.s.post_message(NOW, &at_cap.sign(&w.alice)).is_ok());
    let mut over = msg("m2", "c", NOW, false, 0, b"many parts");
    over.attachments = parts;
    assert!(matches!(
        w.s.post_message(NOW, &over.sign(&w.alice)),
        Err(StoreError::Invalid("too many attachments"))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "c"), vec!["m1"]);
}

#[test]
fn attachment_name_and_mime_are_bounded_at_255_bytes() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let hash = w.s.put_blob(b"named").unwrap();
    let fields = |n: usize| Attachment {
        hash: hash.clone(),
        name: "n".repeat(n),
        mime: "m".repeat(n),
    };
    let mut at_cap = msg("m1", "c", NOW, false, 0, b"long fields");
    at_cap.attachments = vec![fields(255)];
    assert!(w.s.post_message(NOW, &at_cap.sign(&w.alice)).is_ok());

    let mut long_name = msg("m2", "c", NOW, false, 0, b"long name");
    long_name.attachments = vec![Attachment {
        mime: "text/plain".into(),
        ..fields(256)
    }];
    assert!(matches!(
        w.s.post_message(NOW, &long_name.sign(&w.alice)),
        Err(StoreError::Invalid("attachment name too long"))
    ));
    let mut long_mime = msg("m3", "c", NOW, false, 0, b"long mime");
    long_mime.attachments = vec![Attachment {
        name: "run.log".into(),
        ..fields(256)
    }];
    assert!(matches!(
        w.s.post_message(NOW, &long_mime.sign(&w.alice)),
        Err(StoreError::Invalid("attachment mime too long"))
    ));
}

#[test]
fn attachment_hash_must_be_64_lowercase_hex() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let hash = w.s.put_blob(b"hashed").unwrap();
    let malformed = [hash[..63].to_string(), hash.to_uppercase()];
    for (i, bad) in malformed.into_iter().enumerate() {
        let mut m = msg(&format!("m{i}"), "c", NOW, false, 0, b"bad hash");
        m.attachments = vec![Attachment {
            hash: bad,
            name: "h.bin".into(),
            mime: "text/plain".into(),
        }];
        assert!(
            matches!(
                w.s.post_message(NOW, &m.sign(&w.alice)),
                Err(StoreError::Invalid(
                    "attachment hash must be 64 lowercase hex chars"
                ))
            ),
            "case {i}"
        );
    }
    assert_eq!(read_ids(&w.s, &w.bob, "c"), Vec::<String>::new());
}

#[test]
fn duplicate_attachment_hash_in_one_message_is_rejected() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let hash = w.s.put_blob(b"same bytes twice").unwrap();
    let mut m = msg("m1", "c", NOW, false, 0, b"twice");
    m.attachments = vec![
        Attachment {
            hash: hash.clone(),
            name: "a.log".into(),
            mime: "text/plain".into(),
        },
        Attachment {
            hash,
            name: "b.log".into(),
            mime: "text/plain".into(),
        },
    ];
    assert!(matches!(
        w.s.post_message(NOW, &m.sign(&w.alice)),
        Err(StoreError::Invalid("duplicate attachment in message"))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "c"), Vec::<String>::new());
}

#[test]
fn blob_over_the_size_limit_is_rejected() {
    let mut w = world();
    let big = vec![0u8; MAX_BLOB + 1];
    assert!(matches!(
        w.s.put_blob(&big),
        Err(StoreError::Invalid("blob too large"))
    ));
}

#[test]
fn blob_at_the_size_limit_is_accepted() {
    let mut w = world();
    let big = vec![0u8; MAX_BLOB];
    assert_eq!(w.s.put_blob(&big).unwrap(), content_hash(&big));
}

#[test]
fn body_over_the_size_limit_is_rejected() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let big = vec![b'x'; MAX_BODY + 1];
    let err =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", &big))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("body too large")));
}

#[test]
fn body_at_the_size_limit_is_accepted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let big = vec![b'x'; MAX_BODY];
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", &big))
        .unwrap();
    assert_eq!(
        w.s.read_channel(&w.bob.public(), "c", 0, 10).unwrap()[0].body,
        big
    );
}

#[test]
fn inbox_lists_unacked_messages_from_other_members() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
        .unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
        .unwrap();
    w.s.post_message(NOW, &plain(&w.bob, "m3", "c", b"three"))
        .unwrap();
    assert_eq!(
        inbox_of(&w.s, &w.alice),
        vec![("c".to_string(), "m3".to_string())]
    );
    assert_eq!(
        inbox_of(&w.s, &w.bob),
        vec![
            ("c".to_string(), "m1".to_string()),
            ("c".to_string(), "m2".to_string())
        ]
    );
}

#[test]
fn inbox_row_fills_thread_ts_sig_and_attachments_as_posted() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let hash = w.s.put_blob(b"run log").unwrap();
    let parent = plain(&w.alice, "m1", "c", b"parent");
    let parent_seq = w.s.post_message(NOW, &parent).unwrap();
    let mut unsigned = msg("m2", "c", NOW, false, 0, b"see log");
    unsigned.thread_of = Some("m1".into());
    unsigned.attachments = vec![Attachment {
        hash: hash.clone(),
        name: "run.log".into(),
        mime: "text/plain".into(),
    }];
    let reply = unsigned.sign(&w.alice);
    let reply_seq = w.s.post_message(NOW, &reply).unwrap();
    assert_eq!(
        w.s.inbox(&w.bob.public(), 100).unwrap(),
        vec![
            (
                "c".to_string(),
                MessageRow {
                    seq: parent_seq,
                    id: "m1".into(),
                    sender: w.alice.public(),
                    body: b"parent".to_vec(),
                    encrypted: false,
                    epoch: 0,
                    channel: "c".into(),
                    thread_of: None,
                    ts: NOW,
                    attachments: vec![],
                    sig: parent.sig.clone(),
                }
            ),
            (
                "c".to_string(),
                MessageRow {
                    seq: reply_seq,
                    id: "m2".into(),
                    sender: w.alice.public(),
                    body: b"see log".to_vec(),
                    encrypted: false,
                    epoch: 0,
                    channel: "c".into(),
                    thread_of: Some("m1".into()),
                    ts: NOW,
                    attachments: vec![Attachment {
                        hash,
                        name: "run.log".into(),
                        mime: "text/plain".into(),
                    }],
                    sig: reply.sig.clone(),
                }
            ),
        ]
    );
}

#[test]
fn stored_row_reverifies_and_a_flipped_body_byte_fails_verification() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"pay 10"))
        .unwrap();
    let row =
        w.s.read_channel(&w.bob.public(), "c", 0, 10)
            .unwrap()
            .remove(0);
    let signed = row.to_signed();
    signed.verify().unwrap();
    let mut tampered = signed;
    tampered.body[0] ^= 0x01;
    assert!(matches!(tampered.verify(), Err(StoreError::Crypto(_))));
}

#[test]
fn inbox_after_returns_only_rows_after_the_cursor() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
        .unwrap();
    let s3 =
        w.s.post_message(NOW, &plain(&w.alice, "m3", "c", b"three"))
            .unwrap();
    let bob = w.bob.public();
    assert_eq!(
        ids_of(w.s.inbox_after(&bob, s1, 100).unwrap()),
        vec!["m2", "m3"]
    );
    assert_eq!(
        ids_of(w.s.inbox_after(&bob, s3, 100).unwrap()),
        Vec::<String>::new()
    );
}

#[test]
fn inbox_after_excludes_the_readers_own_messages() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
        .unwrap();
    w.s.post_message(NOW, &plain(&w.bob, "m2", "c", b"bob's own"))
        .unwrap();
    assert_eq!(
        ids_of(w.s.inbox_after(&w.bob.public(), 0, 100).unwrap()),
        vec!["m1"]
    );
}

#[test]
fn inbox_after_excludes_messages_the_reader_has_acked() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
        .unwrap();
    let bob = w.bob.public();
    w.s.ack(&bob, "c", s1).unwrap();
    assert_eq!(ids_of(w.s.inbox_after(&bob, 0, 100).unwrap()), vec!["m2"]);
}

#[test]
fn inbox_after_caps_the_page_at_the_limit_after_the_cursor() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
        .unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m3", "c", b"three"))
        .unwrap();
    let bob = w.bob.public();
    assert_eq!(
        ids_of(w.s.inbox_after(&bob, 0, 2).unwrap()),
        vec!["m1", "m2"]
    );
    assert_eq!(ids_of(w.s.inbox_after(&bob, s1, 1).unwrap()), vec!["m2"]);
    assert_eq!(
        ids_of(w.s.inbox_after(&bob, 0, 0).unwrap()),
        Vec::<String>::new()
    );
}

#[test]
fn ack_advances_past_acknowledged_messages() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    let s2 =
        w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
            .unwrap();
    w.s.ack(&w.bob.public(), "c", s1).unwrap();
    assert_eq!(
        inbox_of(&w.s, &w.bob),
        vec![("c".to_string(), "m2".to_string())]
    );
    w.s.ack(&w.bob.public(), "c", s2).unwrap();
    assert_eq!(inbox_of(&w.s, &w.bob), Vec::<(String, String)>::new());
}

#[test]
fn ack_never_moves_the_cursor_backwards() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let s1 =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"one"))
            .unwrap();
    let s2 =
        w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"two"))
            .unwrap();
    let bob = w.bob.public();
    w.s.ack(&bob, "c", s1).unwrap();
    w.s.ack(&bob, "c", 0).unwrap();
    assert_eq!(
        inbox_of(&w.s, &w.bob),
        vec![("c".to_string(), "m2".to_string())]
    );
    w.s.ack(&bob, "c", s2).unwrap();
    w.s.ack(&bob, "c", s1).unwrap();
    assert_eq!(inbox_of(&w.s, &w.bob), Vec::<(String, String)>::new());
}

#[test]
fn ack_beyond_head_cannot_hide_future_messages() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"before the ack"))
        .unwrap();
    let bob = w.bob.public();
    w.s.ack(&bob, "c", i64::MAX).unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m2", "c", b"after the ack"))
        .unwrap();
    assert_eq!(
        inbox_of(&w.s, &w.bob),
        vec![("c".to_string(), "m2".to_string())]
    );
}

#[test]
fn sequence_numbers_increase_across_channels() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    create_group(&mut w.s, &w.alice, "d", &[&w.bob]);
    let a =
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"first"))
            .unwrap();
    let b =
        w.s.post_message(NOW, &plain(&w.alice, "m2", "d", b"second"))
            .unwrap();
    let c =
        w.s.post_message(NOW, &plain(&w.alice, "m3", "c", b"third"))
            .unwrap();
    assert!(a < b && b < c, "seqs {a} {b} {c}");
}

#[test]
fn encrypted_message_opens_with_the_key_wrapped_for_the_member() {
    let mut w = world();
    let key = create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob]);
    let plaintext: &[u8] = b"deploy window is 02:00 UTC";
    let aad = body_aad("e", 0, "m1");
    let sealed = key.seal(&aad, plaintext).unwrap();
    let posted = msg("m1", "e", NOW, true, 0, &sealed).sign(&w.alice);
    let seq = w.s.post_message(NOW, &posted).unwrap();

    let rows = w.s.read_channel(&w.bob.public(), "e", 0, 10).unwrap();
    assert_eq!(
        rows,
        vec![MessageRow {
            seq,
            id: "m1".into(),
            sender: w.alice.public(),
            body: sealed.clone(),
            encrypted: true,
            epoch: 0,
            channel: "e".into(),
            thread_of: None,
            ts: NOW,
            attachments: vec![],
            sig: posted.sig.clone(),
        }]
    );
    // The store keeps only the ciphertext.
    assert!(!contains(&rows[0].body, plaintext));

    let wrapped =
        w.s.wrapped_key("e", &w.bob.public(), 0)
            .unwrap()
            .expect("bob holds the epoch-0 key");
    let bob_key = w
        .bob
        .unwrap_key(&w.alice.public(), "e", 0, &wrapped)
        .unwrap();
    assert_eq!(bob_key.open(&aad, &rows[0].body).unwrap(), plaintext);
}

#[test]
fn short_encrypted_body_is_rejected() {
    let mut w = world();
    create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob]);
    let m = msg("m1", "e", NOW, true, 0, &[0u8; 39]).sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid("encrypted body too short"))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "e"), Vec::<String>::new());
}

#[test]
fn plaintext_is_rejected_in_a_freshly_encrypted_channel() {
    let mut w = world();
    create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob]);
    let m = plain(&w.alice, "p0", "e", b"plain");
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid(
            "plaintext message in encrypted channel"
        ))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "e"), Vec::<String>::new());
}

#[test]
fn stale_epoch_message_is_rejected_after_rotation() {
    let mut w = world();
    rotated(&mut w);
    let stale = msg("m1", "e", NOW, true, 0, &opaque_ciphertext()).sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &stale),
        Err(StoreError::Invalid("stale or future key epoch"))
    ));
}

#[test]
fn plaintext_is_rejected_in_an_encrypted_channel_after_rotation() {
    let mut w = world();
    rotated(&mut w);
    for epoch in [0, 1] {
        let m = msg(&format!("p{epoch}"), "e", NOW, false, epoch, b"plain").sign(&w.alice);
        assert!(
            matches!(
                w.s.post_message(NOW, &m),
                Err(StoreError::Invalid(
                    "plaintext message in encrypted channel"
                ))
            ),
            "epoch {epoch}"
        );
    }
}

#[test]
fn current_epoch_message_is_accepted_after_rotation() {
    let mut w = world();
    rotated(&mut w);
    let body = opaque_ciphertext();
    let m = msg("m1", "e", NOW, true, 1, &body).sign(&w.alice);
    let seq = w.s.post_message(NOW, &m).unwrap();
    assert_eq!(
        w.s.read_channel(&w.bob.public(), "e", 0, 10).unwrap(),
        vec![MessageRow {
            seq,
            id: "m1".into(),
            sender: w.alice.public(),
            body,
            encrypted: true,
            epoch: 1,
            channel: "e".into(),
            thread_of: None,
            ts: NOW,
            attachments: vec![],
            sig: m.sig.clone(),
        }]
    );
}

#[test]
fn future_epoch_message_is_rejected() {
    let mut w = world();
    rotated(&mut w);
    let m = msg("m1", "e", NOW, true, 2, &opaque_ciphertext()).sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid("stale or future key epoch"))
    ));
}

#[test]
fn removed_member_cannot_post_ack_or_see_inbox_after_rotation() {
    let mut w = world();
    create_encrypted(&mut w.s, &w.alice, "e", &[&w.bob, &w.carol]);
    let before = msg("m1", "e", NOW, true, 0, &opaque_ciphertext()).sign(&w.alice);
    let seq = w.s.post_message(NOW, &before).unwrap();
    remove_carol(&mut w);

    let carol = w.carol.public();
    let at_epoch_0 = msg("c0", "e", NOW, true, 0, &opaque_ciphertext()).sign(&w.carol);
    assert!(matches!(
        w.s.post_message(NOW, &at_epoch_0),
        Err(StoreError::NotMember(_))
    ));
    let at_epoch_1 = msg("c1", "e", NOW, true, 1, &opaque_ciphertext()).sign(&w.carol);
    assert!(matches!(
        w.s.post_message(NOW, &at_epoch_1),
        Err(StoreError::NotMember(_))
    ));
    assert!(matches!(
        w.s.ack(&carol, "e", seq),
        Err(StoreError::NotMember(_))
    ));
    assert_eq!(inbox_of(&w.s, &w.carol), Vec::<(String, String)>::new());
}

#[test]
fn removed_member_cannot_read_or_fetch_the_new_key() {
    let mut w = world();
    rotated(&mut w);
    assert!(matches!(
        w.s.read_channel(&w.carol.public(), "e", 0, 10),
        Err(StoreError::NotMember(_))
    ));
    assert_eq!(w.s.wrapped_key("e", &w.carol.public(), 1).unwrap(), None);
}

#[test]
fn post_is_refused_while_a_revoked_members_channel_awaits_rotation() {
    let mut w = world();
    revoked_bob(&mut w);
    let m = msg("m1", "e", NOW, true, 0, &opaque_ciphertext()).sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid("channel key rotation pending"))
    ));
    assert_eq!(read_ids(&w.s, &w.carol, "e"), Vec::<String>::new());
}

#[test]
fn post_succeeds_at_the_next_epoch_once_an_admin_rotates() {
    let mut w = world();
    revoked_bob(&mut w);
    let next = ChannelKey::generate().unwrap();
    let keys = wrap_all(&w.alice, "e", 1, &next, &[&w.alice, &w.carol]);
    let arg = format!(r#"{{"keys":"{}"}}"#, keys_digest(1, &keys));
    let epoch =
        w.s.rotate_channel_key(NOW, &op(&w.alice, "channel.rotate", "e", &arg), &keys)
            .unwrap();
    assert_eq!(epoch, 1);
    let m = msg("m1", "e", NOW, true, 1, &opaque_ciphertext()).sign(&w.alice);
    w.s.post_message(NOW, &m).unwrap();
    assert_eq!(read_ids(&w.s, &w.carol, "e"), vec!["m1"]);
}

#[test]
fn encrypted_message_is_rejected_in_an_unencrypted_channel() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"plain is fine"))
        .unwrap();
    let m = msg("m2", "c", NOW, true, 0, &opaque_ciphertext()).sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid(
            "encrypted message in unencrypted channel"
        ))
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "c"), vec!["m1"]);
}

#[test]
fn nonzero_epoch_is_rejected_in_an_unencrypted_channel() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"plain is fine"))
        .unwrap();
    let m = msg("m2", "c", NOW, false, 1, b"plain at epoch 1").sign(&w.alice);
    assert!(matches!(
        w.s.post_message(NOW, &m),
        Err(StoreError::Invalid(
            "epoch must be 0 in unencrypted channel"
        ))
    ));
}

#[test]
fn duplicate_message_id_is_rejected_and_the_original_is_kept() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    let first = plain(&w.alice, "m1", "c", b"first");
    let seq = w.s.post_message(NOW, &first).unwrap();
    assert!(matches!(
        w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"second")),
        Err(StoreError::Invalid("message id already exists"))
    ));
    assert_eq!(
        w.s.read_channel(&w.bob.public(), "c", 0, 10).unwrap(),
        vec![MessageRow {
            seq,
            id: "m1".into(),
            sender: w.alice.public(),
            body: b"first".to_vec(),
            encrypted: false,
            epoch: 0,
            channel: "c".into(),
            thread_of: None,
            ts: NOW,
            attachments: vec![],
            sig: first.sig.clone(),
        }]
    );
}

#[test]
fn duplicate_message_id_from_another_sender_in_another_channel_is_rejected() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);
    create_group(&mut w.s, &w.alice, "d", &[&w.bob]);
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"alice in c"))
        .unwrap();
    let err =
        w.s.post_message(NOW, &plain(&w.bob, "m1", "d", b"bob in d"))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("message id already exists")
    ));
    assert_eq!(read_ids(&w.s, &w.bob, "d"), Vec::<String>::new());
}

#[test]
fn rejected_posts_append_no_event() {
    let mut w = world();
    create_group(&mut w.s, &w.alice, "c", &[&w.bob]);

    let before = w.s.event_count().unwrap();
    w.s.post_message(NOW, &plain(&w.alice, "m1", "c", b"accepted"))
        .unwrap();
    let after_accept = w.s.event_count().unwrap();
    assert_eq!(
        after_accept,
        before + 1,
        "an accepted post appends one event"
    );

    let mut tampered = plain(&w.alice, "m2", "c", b"original");
    tampered.body = b"changed".to_vec();
    let mut missing_blob = msg("m3", "c", NOW, false, 0, b"x");
    missing_blob.attachments = vec![Attachment {
        hash: content_hash(b"never uploaded"),
        name: "ghost".into(),
        mime: "application/octet-stream".into(),
    }];
    type Expected = fn(&StoreError) -> bool;
    let failing: Vec<(&str, SignedMessage, Expected)> = vec![
        (
            "duplicate id",
            plain(&w.alice, "m1", "c", b"again"),
            (|e| matches!(e, StoreError::Invalid("message id already exists"))) as Expected,
        ),
        (
            "tampered body",
            tampered,
            (|e| matches!(e, StoreError::Crypto(_))) as Expected,
        ),
        (
            "missing attachment",
            missing_blob.sign(&w.alice),
            (|e| matches!(e, StoreError::Invalid("attachment blob missing"))) as Expected,
        ),
        (
            "non-member",
            plain(&w.carol, "m4", "c", b"sneaky"),
            (|e| matches!(e, StoreError::NotMember(_))) as Expected,
        ),
        (
            "thread parent missing",
            reply(&w.alice, "r1", "c", "no-such-message", b"orphan"),
            (|e| matches!(e, StoreError::Invalid("thread parent not in this channel"))) as Expected,
        ),
    ];
    for (what, m, expected) in &failing {
        match w.s.post_message(NOW, m) {
            Err(e) => assert!(expected(&e), "{what}: wrong error {e:?}"),
            Ok(seq) => panic!("{what} was accepted as seq {seq}"),
        }
        assert_eq!(
            w.s.event_count().unwrap(),
            after_accept,
            "{what} must not append an event"
        );
    }
}
