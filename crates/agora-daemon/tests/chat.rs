//! Chat over the daemon socket: signed posts, encrypted channels and key rotation,
//! inbox acks, reactions, and attachment blobs (see SPEC "Handlers: chat").

mod common;

use agora_crypto::{ChannelKey, Identity, body_aad, content_hash};
use agora_daemon::now_ms;
use agora_proto::{
    ChannelView, EmojiCount, ErrorBody, ErrorCode, MessageView, MessageWire, ReactionWire, Reply,
    Req, b64, keys_to_wire, unb64,
};
use agora_store::{Pub, SignedMessage, SignedReaction, hex, keys_digest};
use common::*;

const SEC: &str = "sec";

/// A message signed by `author` (sender and signature are its own), ready to post.
fn wire_message(
    author: &TestClient,
    id: &str,
    channel: &str,
    body: &[u8],
    encrypted: bool,
    epoch: i64,
) -> MessageWire {
    let msg = SignedMessage {
        id: id.to_string(),
        channel: channel.to_string(),
        thread_of: None,
        sender: [0; 32],
        ts: now_ms(),
        encrypted,
        epoch,
        body: body.to_vec(),
        attachments: vec![],
        sig: vec![],
    }
    .sign(&author.id);
    MessageWire::from_message(&msg)
}

/// Post a plaintext message and return its seq; panics on any error.
async fn post_plain(c: &mut TestClient, id: &str, channel: &str, body: &[u8]) -> i64 {
    let msg = wire_message(c, id, channel, body, false, 0);
    match c.ok(Req::Post { msg }).await {
        Reply::Posted { seq } => seq,
        other => panic!("post {id}: expected Posted, got {other:?}"),
    }
}

/// Post `plain` sealed under `key` at `epoch`; the reply is returned for the caller to judge.
async fn post_sealed(
    c: &mut TestClient,
    id: &str,
    epoch: i64,
    key: &ChannelKey,
    plain: &[u8],
) -> Result<Reply, ErrorBody> {
    let sealed = key
        .seal(&body_aad(SEC, epoch, id), plain)
        .expect("seal body");
    let msg = wire_message(c, id, SEC, &sealed, true, epoch);
    c.call(Req::Post { msg }).await
}

/// Decrypt a stored sealed message with `key`.
fn open_message(key: &ChannelKey, view: &MessageView) -> Vec<u8> {
    let sealed = unb64(&view.msg.body, "body").expect("body is base64");
    key.open(
        &body_aad(&view.msg.channel, view.msg.epoch, &view.msg.id),
        &sealed,
    )
    .expect("open sealed body")
}

fn code_of(result: Result<Reply, ErrorBody>) -> ErrorCode {
    match result {
        Err(error) => error.code,
        Ok(reply) => panic!("expected an error reply, got {reply:?}"),
    }
}

fn messages_of(reply: Reply) -> Vec<MessageView> {
    match reply {
        Reply::Messages { messages } => messages,
        other => panic!("expected Messages, got {other:?}"),
    }
}

async fn channel_view(c: &mut TestClient, channel: &str) -> ChannelView {
    match c
        .ok(Req::ChannelInfo {
            channel: channel.to_string(),
        })
        .await
    {
        Reply::Channel { channel, .. } => channel,
        other => panic!("expected Channel, got {other:?}"),
    }
}

/// `sender` wraps `key` for each of `recipients` at `epoch`, bound to `channel`.
fn wrap_all(
    sender: &Identity,
    recipients: &[Pub],
    channel: &str,
    epoch: i64,
    key: &ChannelKey,
) -> Vec<(Pub, Vec<u8>)> {
    recipients
        .iter()
        .map(|pk| {
            let wrapped = sender
                .wrap_key(pk, channel, epoch, key)
                .expect("wrap channel key");
            (*pk, wrapped)
        })
        .collect()
}

/// Create encrypted group `channel` at epoch 0 with `creator` plus `others`; returns the keys sent.
async fn create_sealed(
    creator: &mut TestClient,
    channel: &str,
    others: &[Pub],
    key: &ChannelKey,
) -> Vec<(Pub, Vec<u8>)> {
    let mut recipients = vec![creator.id.public()];
    recipients.extend_from_slice(others);
    let keys = wrap_all(&creator.id, &recipients, channel, 0, key);
    let listed: Vec<serde_json::Value> = others
        .iter()
        .map(|pk| serde_json::json!({"pk": hex(pk), "role": "member"}))
        .collect();
    let arg = serde_json::json!({
        "kind": "group",
        "members": listed,
        "keys": keys_digest(0, &keys),
    })
    .to_string();
    let op = creator.op("channel.create", channel, &arg);
    match creator
        .ok(Req::ChannelCreate {
            op,
            keys: keys_to_wire(&keys),
        })
        .await
    {
        Reply::Unit => keys,
        other => panic!("create {channel}: expected Unit, got {other:?}"),
    }
}

/// Alice creates the encrypted channel `SEC` with bob and carol at epoch 0.
async fn create_sec(w: &mut World, key: &ChannelKey) {
    let others = [w.bob.id.public(), w.carol.id.public()];
    create_sealed(&mut w.alice, SEC, &others, key).await;
}

/// Alice removes carol from `SEC`; the new key at epoch 1 is wrapped for alice and bob.
async fn remove_carol(
    alice: &mut TestClient,
    bob: &TestClient,
    carol: &TestClient,
    key1: &ChannelKey,
) {
    let keys1 = wrap_all(
        &alice.id,
        &[alice.id.public(), bob.id.public()],
        SEC,
        1,
        key1,
    );
    let arg = serde_json::json!({
        "pk": carol.pk_hex(),
        "keys": keys_digest(1, &keys1),
    })
    .to_string();
    let op = alice.op("channel.remove", SEC, &arg);
    match alice
        .call(Req::ChannelRemove {
            op,
            keys: keys_to_wire(&keys1),
        })
        .await
    {
        Ok(Reply::Unit) => {}
        other => panic!("remove carol: expected Unit, got {other:?}"),
    }
}

/// The caller's own key for `channel` (current epoch), unwrapped and checked against `sender`.
async fn current_key(
    reader: &mut TestClient,
    channel: &str,
    epoch: i64,
    sender: &Pub,
) -> ChannelKey {
    match reader
        .ok(Req::ChannelKey {
            channel: channel.to_string(),
            epoch: None,
        })
        .await
    {
        Reply::Key {
            epoch: found,
            wrapper,
            wrapped,
        } => {
            assert_eq!(found, epoch, "current key epoch");
            assert_eq!(
                wrapper,
                hex(sender),
                "key wrapped by the channel creator or rotator"
            );
            let wrapped = unb64(&wrapped, "wrapped").expect("wrapped key is base64");
            reader
                .id
                .unwrap_key(sender, channel, epoch, &wrapped)
                .expect("unwrap channel key")
        }
        other => panic!("expected Key, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_post_reaches_a_member_with_its_signature_intact() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;

    let seq = post_plain(&mut w.alice, "m1", "g", b"hello bob").await;

    let messages = messages_of(
        w.bob
            .ok(Req::Read {
                channel: "g".to_string(),
                after: 0,
                limit: 10,
            })
            .await,
    );
    assert_eq!(messages.len(), 1, "bob sees alice's one message");
    let view = &messages[0];
    assert_eq!(view.seq, seq);
    assert_eq!(view.msg.sender, w.alice.pk_hex());
    let signed = view.msg.to_message().expect("stored message decodes");
    signed
        .verify()
        .expect("signature verifies over the stored fields");
    assert_eq!(signed.body, b"hello bob".to_vec());

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_signed_by_another_identity_on_this_connection_is_forbidden() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;

    // Bob is a member and the signature is valid for bob; only the connection is wrong.
    let forged = wire_message(&w.bob, "m1", "g", b"forged", false, 0);
    let result = w.alice.call(Req::Post { msg: forged }).await;
    assert_eq!(code_of(result), ErrorCode::Forbidden);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_op_signed_by_another_identity_on_this_connection_is_forbidden() {
    let d = start().await;
    let mut w = world(&d).await;

    let op = w
        .bob
        .op("channel.create", "x", r#"{"kind":"group","members":[]}"#);
    let result = w.alice.call(Req::ChannelCreate { op, keys: vec![] }).await;
    assert_eq!(code_of(result), ErrorCode::Forbidden);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_by_a_non_member_is_not_member() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;

    let msg = wire_message(&w.carol, "m1", "g", b"let me in", false, 0);
    let result = w.carol.call(Req::Post { msg }).await;
    assert_eq!(code_of(result), ErrorCode::NotMember);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_of_a_channel_one_is_not_in_and_of_a_missing_channel_give_the_same_error() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;

    let existing = w
        .carol
        .call(Req::Read {
            channel: "g".to_string(),
            after: 0,
            limit: 10,
        })
        .await;
    let missing = w
        .carol
        .call(Req::Read {
            channel: "nope".to_string(),
            after: 0,
            limit: 10,
        })
        .await;
    assert_eq!(code_of(existing), ErrorCode::NotMember);
    assert_eq!(code_of(missing), ErrorCode::NotMember);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_fetches_its_wrapped_key_and_opens_a_sealed_message() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;

    let posted = post_sealed(&mut w.alice, "m1", 0, &key0, b"meet at noon").await;
    assert!(
        matches!(posted, Ok(Reply::Posted { .. })),
        "sealed post accepted"
    );

    let alice_pk = w.alice.id.public();
    let bob_key = current_key(&mut w.bob, SEC, 0, &alice_pk).await;
    let messages = messages_of(
        w.bob
            .ok(Req::Read {
                channel: SEC.to_string(),
                after: 0,
                limit: 10,
            })
            .await,
    );
    assert_eq!(messages.len(), 1);
    assert!(messages[0].msg.encrypted);
    assert_eq!(
        open_message(&bob_key, &messages[0]),
        b"meet at noon".to_vec()
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_member_advances_the_channel_epoch() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;
    let key1 = ChannelKey::generate().expect("generate channel key");

    remove_carol(&mut w.alice, &w.bob, &w.carol, &key1).await;

    let info = channel_view(&mut w.alice, SEC).await;
    assert_eq!(info.epoch, 1, "removal rotates to epoch 1");

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_member_cannot_fetch_the_key_or_read_the_channel() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;
    let key1 = ChannelKey::generate().expect("generate channel key");
    remove_carol(&mut w.alice, &w.bob, &w.carol, &key1).await;

    let key = w
        .carol
        .call(Req::ChannelKey {
            channel: SEC.to_string(),
            epoch: None,
        })
        .await;
    assert_eq!(code_of(key), ErrorCode::NotMember);
    let read = w
        .carol
        .call(Req::Read {
            channel: SEC.to_string(),
            after: 0,
            limit: 10,
        })
        .await;
    assert_eq!(code_of(read), ErrorCode::NotMember);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remaining_member_opens_a_message_sealed_at_the_new_epoch() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;
    let key1 = ChannelKey::generate().expect("generate channel key");
    remove_carol(&mut w.alice, &w.bob, &w.carol, &key1).await;

    let posted = post_sealed(&mut w.alice, "m2", 1, &key1, b"after removal").await;
    assert!(
        matches!(posted, Ok(Reply::Posted { .. })),
        "epoch 1 post accepted"
    );

    let alice_pk = w.alice.id.public();
    let bob_key = current_key(&mut w.bob, SEC, 1, &alice_pk).await;
    let messages = messages_of(
        w.bob
            .ok(Req::Read {
                channel: SEC.to_string(),
                after: 0,
                limit: 10,
            })
            .await,
    );
    let m2 = messages
        .iter()
        .find(|v| v.msg.id == "m2")
        .expect("bob reads the epoch 1 message");
    assert_eq!(m2.msg.epoch, 1);
    assert_eq!(open_message(&bob_key, m2), b"after removal".to_vec());

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_at_the_superseded_epoch_is_invalid_after_removal() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;
    let key1 = ChannelKey::generate().expect("generate channel key");
    remove_carol(&mut w.alice, &w.bob, &w.carol, &key1).await;

    let stale = post_sealed(&mut w.alice, "m3", 0, &key0, b"old key").await;
    assert_eq!(code_of(stale), ErrorCode::Invalid);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_with_keys_that_do_not_match_the_digest_is_invalid_and_leaves_no_channel() {
    let d = start().await;
    let mut w = world(&d).await;
    let key_a = ChannelKey::generate().expect("generate channel key");
    let key_b = ChannelKey::generate().expect("generate channel key");
    let recipients = [w.alice.id.public(), w.bob.id.public(), w.carol.id.public()];
    let keys_a = wrap_all(&w.alice.id, &recipients, "sec2", 0, &key_a);
    let keys_b = wrap_all(&w.alice.id, &recipients, "sec2", 0, &key_b);
    // The digest commits to keys A; the request carries keys B for the same members.
    let arg = serde_json::json!({
        "kind": "group",
        "members": [
            {"pk": w.bob.pk_hex(), "role": "member"},
            {"pk": w.carol.pk_hex(), "role": "member"},
        ],
        "keys": keys_digest(0, &keys_a),
    })
    .to_string();
    let op = w.alice.op("channel.create", "sec2", &arg);
    let result = w
        .alice
        .call(Req::ChannelCreate {
            op,
            keys: keys_to_wire(&keys_b),
        })
        .await;
    assert_eq!(code_of(result), ErrorCode::Invalid);

    let channels = match w.alice.ok(Req::ChannelList).await {
        Reply::Channels { channels } => channels,
        other => panic!("expected Channels, got {other:?}"),
    };
    assert!(
        !channels.iter().any(|c| c.id == "sec2"),
        "a rejected create leaves no channel behind"
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_member_marks_the_encrypted_channel_for_rotation_and_refuses_posts() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;

    let op = w.owner.op("identity.revoke", &w.carol.pk_hex(), "");
    match w.owner.ok(Req::Revoke { op }).await {
        Reply::Unit => {}
        other => panic!("revoke carol: expected Unit, got {other:?}"),
    }

    let info = channel_view(&mut w.alice, SEC).await;
    assert!(info.rotation_pending, "revocation requires a key rotation");
    let refused = post_sealed(&mut w.alice, "m2", 0, &key0, b"blocked").await;
    assert_eq!(code_of(refused), ErrorCode::Invalid);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotate_clears_the_pending_rotation_and_accepts_posts_at_the_new_epoch() {
    let d = start().await;
    let mut w = world(&d).await;
    let key0 = ChannelKey::generate().expect("generate channel key");
    create_sec(&mut w, &key0).await;
    let op = w.owner.op("identity.revoke", &w.carol.pk_hex(), "");
    w.owner.ok(Req::Revoke { op }).await;

    let key1 = ChannelKey::generate().expect("generate channel key");
    let keys1 = wrap_all(
        &w.alice.id,
        &[w.alice.id.public(), w.bob.id.public()],
        SEC,
        1,
        &key1,
    );
    let arg = serde_json::json!({"keys": keys_digest(1, &keys1)}).to_string();
    let op = w.alice.op("channel.rotate", SEC, &arg);
    match w
        .alice
        .ok(Req::ChannelRotate {
            op,
            keys: keys_to_wire(&keys1),
        })
        .await
    {
        Reply::Epoch { epoch } => assert_eq!(epoch, 1, "rotation returns the new epoch"),
        other => panic!("rotate: expected Epoch, got {other:?}"),
    }

    let info = channel_view(&mut w.alice, SEC).await;
    assert!(!info.rotation_pending, "rotation clears the pending flag");
    let posted = post_sealed(&mut w.alice, "m2", 1, &key1, b"new key").await;
    assert!(
        matches!(posted, Ok(Reply::Posted { .. })),
        "epoch 1 post accepted"
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_put_then_get_returns_the_same_bytes() {
    let d = start().await;
    let mut w = world(&d).await;
    let data = b"attachment bytes".to_vec();

    let hash = match w.alice.ok(Req::PutBlob { bytes: b64(&data) }).await {
        Reply::BlobHash { hash } => hash,
        other => panic!("PutBlob: expected BlobHash, got {other:?}"),
    };
    assert_eq!(hash, content_hash(&data), "the hash is the content hash");

    let got = match w.alice.ok(Req::GetBlob { hash }).await {
        Reply::Blob { bytes } => unb64(&bytes, "bytes").expect("blob is base64"),
        other => panic!("GetBlob: expected Blob, got {other:?}"),
    };
    assert_eq!(got, data);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_over_the_size_cap_is_too_large() {
    let d = start_with(|c| c.max_blob_bytes = 1024).await;
    let mut w = world(&d).await;

    let result = w
        .alice
        .call(Req::PutBlob {
            bytes: b64(&vec![7u8; 1025]),
        })
        .await;
    assert_eq!(code_of(result), ErrorCode::TooLarge);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_exactly_at_the_size_cap_is_stored() {
    let d = start_with(|c| c.max_blob_bytes = 1024).await;
    let mut w = world(&d).await;

    let reply = w
        .alice
        .ok(Req::PutBlob {
            bytes: b64(&vec![7u8; 1024]),
        })
        .await;
    assert!(
        matches!(reply, Reply::BlobHash { .. }),
        "a 1024-byte blob fits the cap"
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_over_one_mebibyte_spills_to_a_file_under_blobs_and_reads_back() {
    let d = start().await;
    let mut w = world(&d).await;
    let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let hash = match w.alice.ok(Req::PutBlob { bytes: b64(&data) }).await {
        Reply::BlobHash { hash } => hash,
        other => panic!("PutBlob: expected BlobHash, got {other:?}"),
    };
    let path = d.home.path().join("blobs").join(&hash[0..2]).join(&hash);
    let on_disk = std::fs::read(&path).expect("spilled blob file exists");
    assert!(on_disk == data, "the spilled file holds the uploaded bytes");

    let got = match w.alice.ok(Req::GetBlob { hash }).await {
        Reply::Blob { bytes } => unb64(&bytes, "bytes").expect("blob is base64"),
        other => panic!("GetBlob: expected Blob, got {other:?}"),
    };
    assert!(got == data, "GetBlob returns the identical bytes");

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_upload_past_the_identity_quota_is_quota() {
    let d = start_with(|c| c.blob_quota_bytes = 1000).await;
    let mut w = world(&d).await;

    let first = w
        .alice
        .call(Req::PutBlob {
            bytes: b64(&vec![1u8; 600]),
        })
        .await;
    assert!(
        matches!(first, Ok(Reply::BlobHash { .. })),
        "first 600 bytes fit the quota"
    );
    let second = w
        .alice
        .call(Req::PutBlob {
            bytes: b64(&vec![2u8; 600]),
        })
        .await;
    assert_eq!(code_of(second), ErrorCode::Quota);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_blob_of_an_unknown_hash_is_not_found() {
    let d = start().await;
    let mut w = world(&d).await;

    let result = w
        .alice
        .call(Req::GetBlob {
            hash: "0".repeat(64),
        })
        .await;
    assert_eq!(code_of(result), ErrorCode::NotFound);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_moves_the_inbox_past_the_acknowledged_message() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = post_plain(&mut w.alice, "m1", "g", b"read me").await;

    let before = match w.bob.ok(Req::Inbox { limit: 10 }).await {
        Reply::Inbox {
            messages,
            cursor,
            timed_out,
        } => (messages, cursor, timed_out),
        other => panic!("expected Inbox, got {other:?}"),
    };
    assert_eq!(before.0.len(), 1, "bob's inbox holds alice's message");
    assert_eq!(before.0[0].seq, seq);
    assert_eq!(before.1, seq, "cursor is the largest seq returned");
    assert!(!before.2);

    w.bob
        .ok(Req::Ack {
            channel: "g".to_string(),
            seq,
        })
        .await;

    match w.bob.ok(Req::Inbox { limit: 10 }).await {
        Reply::Inbox {
            messages, cursor, ..
        } => {
            assert!(messages.is_empty(), "an acked message leaves the inbox");
            assert_eq!(cursor, 0);
        }
        other => panic!("expected Inbox, got {other:?}"),
    }

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reaction_is_counted_against_the_message_it_targets() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    post_plain(&mut w.alice, "m1", "g", b"react to me").await;

    let reaction = SignedReaction {
        kind: "message".to_string(),
        target: "m1".to_string(),
        by: [0; 32],
        emoji: "👍".to_string(),
        remove: false,
        ts: now_ms(),
        sig: vec![],
    }
    .sign(&w.bob.id);
    let react = w
        .bob
        .ok(Req::React {
            reaction: ReactionWire::from_reaction(&reaction),
        })
        .await;
    assert!(matches!(react, Reply::Unit));

    let counts = match w
        .bob
        .ok(Req::ReactionCounts {
            kind: "message".to_string(),
            target: "m1".to_string(),
        })
        .await
    {
        Reply::Counts { counts } => counts,
        other => panic!("expected Counts, got {other:?}"),
    };
    assert_eq!(
        counts,
        vec![EmojiCount {
            emoji: "👍".to_string(),
            count: 1,
        }]
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reaction_counts_on_a_message_outside_the_reader_channel_are_not_found() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    post_plain(&mut w.alice, "m1", "g", b"private").await;

    let result = w
        .carol
        .call(Req::ReactionCounts {
            kind: "message".to_string(),
            target: "m1".to_string(),
        })
        .await;
    assert_eq!(code_of(result), ErrorCode::NotFound);

    d.stop().await;
}
