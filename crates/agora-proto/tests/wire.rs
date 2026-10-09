//! Wire-format contract of agora-proto, through its public API only: the exact JSON
//! shapes peers exchange, round trips for every request and reply variant, the
//! conversions from signed store types, the framing limits, and the handshake
//! signature. Identities come from fixed seeds and timestamps are literals, so every
//! test is deterministic and touches no clock, filesystem or network.

use agora_crypto::{CryptoError, Identity};
use agora_proto::*;
use agora_sched::model::{LeaseKind, NoticeRecord, PoolRecord, TaskFilter, TaskState, WaitSpec};
use agora_sched::order::OrderPolicy;
use agora_store::{Attachment, SignedMessage, SignedOp, StoreError, hex};
use serde_json::json;
use tokio::io::{AsyncWriteExt, duplex};

const CHALLENGE: [u8; 32] = [5; 32];

fn id(seed: u8) -> Identity {
    Identity::from_seed([seed; 32])
}

fn sender_hex() -> String {
    hex(&[0xab; 32])
}

fn hash_hex() -> String {
    "cd".repeat(32)
}

fn op_wire() -> OpWire {
    OpWire {
        kind: "card.create".into(),
        subject: "card-1".into(),
        arg: "{\"title\":\"x\"}".into(),
        ts: 1_000,
        nonce: "n-1".into(),
        actor: sender_hex(),
        sig: b64(&[9; 64]),
    }
}

fn key_wire() -> KeyWire {
    KeyWire {
        pk: sender_hex(),
        wrapped: b64(&[3; 72]),
    }
}

fn message_wire() -> MessageWire {
    MessageWire {
        id: "m-1".into(),
        channel: "ch-1".into(),
        thread_of: None,
        sender: sender_hex(),
        ts: 42,
        encrypted: false,
        epoch: 0,
        body: b64(b"hi"),
        attachments: vec![AttachmentWire {
            hash: hash_hex(),
            name: "a.txt".into(),
            mime: "text/plain".into(),
        }],
        sig: b64(&[7; 64]),
    }
}

fn reaction_wire() -> ReactionWire {
    ReactionWire {
        kind: "message".into(),
        target: "m-1".into(),
        by: sender_hex(),
        emoji: "+1".into(),
        remove: false,
        ts: 5,
        sig: b64(&[8; 64]),
    }
}

fn note_wire() -> NoteWire {
    NoteWire {
        id: "n-1".into(),
        author: sender_hex(),
        ts: 6,
        title: "t".into(),
        body: "b".into(),
        tags: "x,y".into(),
        supersedes: None,
        sig: b64(&[6; 64]),
    }
}

fn channel_view() -> ChannelView {
    ChannelView {
        id: "ch-1".into(),
        kind: "chat".into(),
        name: "general".into(),
        parent: None,
        epoch: 2,
        encrypted: true,
        rotation_pending: false,
        my_role: Some("admin".into()),
    }
}

/// Every request variant with one representative value, tagged with its wire name.
fn every_request() -> Vec<(&'static str, Req)> {
    vec![
        ("ping", Req::Ping),
        ("whoami", Req::Whoami),
        (
            "register",
            Req::Register {
                label: "laptop".into(),
            },
        ),
        ("bootstrap_owner", Req::BootstrapOwner),
        ("shutdown", Req::Shutdown),
        (
            "put_blob",
            Req::PutBlob {
                bytes: b64(b"hello"),
            },
        ),
        ("get_blob", Req::GetBlob { hash: hash_hex() }),
        (
            "post",
            Req::Post {
                msg: message_wire(),
            },
        ),
        (
            "read",
            Req::Read {
                channel: "ch-1".into(),
                after: 3,
                limit: 50,
            },
        ),
        ("inbox", Req::Inbox { limit: 20 }),
        (
            "inbox_wait",
            Req::InboxWait {
                after: 3,
                timeout_ms: 1_000,
                limit: 20,
            },
        ),
        (
            "ack",
            Req::Ack {
                channel: "ch-1".into(),
                seq: 9,
            },
        ),
        (
            "react",
            Req::React {
                reaction: reaction_wire(),
            },
        ),
        (
            "reaction_counts",
            Req::ReactionCounts {
                kind: "message".into(),
                target: "m-1".into(),
            },
        ),
        (
            "channel_create",
            Req::ChannelCreate {
                op: op_wire(),
                keys: vec![key_wire()],
            },
        ),
        (
            "channel_add",
            Req::ChannelAdd {
                op: op_wire(),
                wrapped: Some(b64(&[3; 72])),
            },
        ),
        (
            "channel_remove",
            Req::ChannelRemove {
                op: op_wire(),
                keys: vec![],
            },
        ),
        ("channel_role", Req::ChannelRole { op: op_wire() }),
        (
            "channel_rotate",
            Req::ChannelRotate {
                op: op_wire(),
                keys: vec![key_wire()],
            },
        ),
        ("channel_list", Req::ChannelList),
        (
            "channel_info",
            Req::ChannelInfo {
                channel: "ch-1".into(),
            },
        ),
        (
            "channel_key",
            Req::ChannelKey {
                channel: "ch-1".into(),
                epoch: Some(2),
            },
        ),
        ("role_grant", Req::RoleGrant { op: op_wire() }),
        ("revoke", Req::Revoke { op: op_wire() }),
        ("card_create", Req::CardCreate { op: op_wire() }),
        ("card_move", Req::CardMove { op: op_wire() }),
        ("card_evidence", Req::CardEvidence { op: op_wire() }),
        ("card_approve", Req::CardApprove { op: op_wire() }),
        ("card_reclaim", Req::CardReclaim { op: op_wire() }),
        (
            "card_list",
            Req::CardList {
                state: Some("ready".into()),
                lane: None,
                limit: 10,
            },
        ),
        ("sprint_create", Req::SprintCreate { op: op_wire() }),
        ("sprint_add", Req::SprintAdd { op: op_wire() }),
        ("sprint_start", Req::SprintStart { op: op_wire() }),
        ("sprint_close", Req::SprintClose { op: op_wire() }),
        (
            "sprint_summary",
            Req::SprintSummary {
                sprint: "s-1".into(),
            },
        ),
        ("note_publish", Req::NotePublish { note: note_wire() }),
        (
            "note_search",
            Req::NoteSearch {
                query: "q".into(),
                limit: 5,
            },
        ),
        (
            "subscribe",
            Req::Subscribe {
                channels: vec!["ch-1".into()],
            },
        ),
        ("online", Req::Online),
        ("work_scopes", Req::WorkScopes { root: None }),
        ("work_add", Req::WorkAdd { op: op_wire() }),
        (
            "work_show",
            Req::WorkShow {
                task: "task-1".into(),
            },
        ),
        (
            "work_run",
            Req::WorkRun {
                op: op_wire(),
                env: [("PATH".to_string(), "/usr/bin".to_string())].into(),
            },
        ),
        (
            "work_list",
            Req::WorkList {
                filter: TaskFilter {
                    scope: Some("p".into()),
                    states: vec![TaskState::Queued, TaskState::Running],
                    ids: vec![],
                    limit: 20,
                },
            },
        ),
        (
            "work_plan",
            Req::WorkPlan {
                scope: None,
                policy: OrderPolicy::CriticalPath,
                agents: 2,
                assign: true,
                conflict_cost: Some(1.5),
            },
        ),
        (
            "work_wait",
            Req::WorkWait {
                spec: WaitSpec::Tasks {
                    ids: vec!["a".into()],
                    any: true,
                },
                timeout_ms: 5_000,
                cursor: Some("abc".into()),
            },
        ),
        ("work_settings", Req::WorkSettings { op: None }),
        (
            "lease_list",
            Req::LeaseList {
                kind: Some(LeaseKind::Path),
                holder: None,
                active_only: true,
            },
        ),
        ("pool_list", Req::PoolList),
        ("card_update", Req::CardUpdate { op: op_wire() }),
        (
            "card_ext",
            Req::CardExt {
                ids: vec!["c-1".into()],
            },
        ),
        ("watchdog_report", Req::WatchdogReport),
        (
            "metrics",
            Req::Metrics {
                team: None,
                lane: Some("main".into()),
                since_ms: Some(10),
            },
        ),
        ("board_extras", Req::BoardExtras),
        (
            "notices",
            Req::Notices {
                after: 3,
                limit: 50,
            },
        ),
    ]
}

/// Every reply variant with one representative value, tagged with its wire name.
fn every_reply() -> Vec<(&'static str, Reply)> {
    let message = MessageView {
        seq: 9,
        msg: message_wire(),
    };
    vec![
        ("pong", Reply::Pong),
        ("unit", Reply::Unit),
        (
            "whoami",
            Reply::Whoami {
                pubkey: sender_hex(),
                pseudonym: "amber-otter".into(),
                role: Some("owner".into()),
                registered: true,
                revoked: false,
                owner_exists: true,
            },
        ),
        ("blob_hash", Reply::BlobHash { hash: hash_hex() }),
        (
            "blob",
            Reply::Blob {
                bytes: b64(b"hello"),
            },
        ),
        ("posted", Reply::Posted { seq: 9 }),
        (
            "messages",
            Reply::Messages {
                messages: vec![message.clone()],
            },
        ),
        (
            "inbox",
            Reply::Inbox {
                messages: vec![message],
                cursor: 9,
                timed_out: false,
            },
        ),
        (
            "counts",
            Reply::Counts {
                counts: vec![EmojiCount {
                    emoji: "+1".into(),
                    count: 2,
                }],
            },
        ),
        (
            "channels",
            Reply::Channels {
                channels: vec![channel_view()],
            },
        ),
        (
            "channel",
            Reply::Channel {
                channel: channel_view(),
                members: vec![MemberView {
                    pk: sender_hex(),
                    pseudonym: "amber-otter".into(),
                    role: "admin".into(),
                }],
            },
        ),
        (
            "key",
            Reply::Key {
                epoch: 2,
                wrapper: sender_hex(),
                wrapped: b64(&[3; 72]),
            },
        ),
        ("epoch", Reply::Epoch { epoch: 3 }),
        (
            "cards",
            Reply::Cards {
                cards: vec![CardView {
                    id: "card-1".into(),
                    title: "t".into(),
                    lane: "main".into(),
                    state: "ready".into(),
                    owner: None,
                    updated: 5,
                }],
            },
        ),
        (
            "sprint",
            Reply::Sprint {
                summary: SprintSummaryView {
                    total: 4,
                    done: 1,
                    in_flight: 2,
                    blocked: 1,
                },
            },
        ),
        (
            "notes",
            Reply::Notes {
                notes: vec![NoteHit {
                    id: "n-1".into(),
                    title: "t".into(),
                }],
            },
        ),
        (
            "subscribed",
            Reply::Subscribed {
                channels: vec!["ch-1".into()],
            },
        ),
        (
            "online",
            Reply::Online {
                pubkeys: vec![sender_hex()],
            },
        ),
        (
            "work_plan",
            Reply::WorkPlan {
                json: "{}".into(),
                text: "order".into(),
            },
        ),
        (
            "pools",
            Reply::Pools {
                pools: vec![PoolRecord {
                    name: "gpu".into(),
                    capacity: 2.0,
                    unit: "cards".into(),
                    used: 0.5,
                    note: String::new(),
                    created_ms: 7,
                    created_by: sender_hex(),
                }],
            },
        ),
        (
            "escalated",
            Reply::Escalated {
                notified: vec![sender_hex()],
            },
        ),
        (
            "notices",
            Reply::Notices {
                notices: vec![NoticeRecord {
                    seq: 1,
                    ts_ms: 2,
                    target: sender_hex(),
                    kind: "stale".into(),
                    subject: "c-1".into(),
                    text: "t".into(),
                }],
            },
        ),
    ]
}

/// Exact JSON in both directions: the value serializes to `expected`, and `expected`
/// parses back to the value.
fn assert_wire<T>(value: &T, expected: serde_json::Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(value).expect("serializes"), expected);
    let back: T = serde_json::from_value(expected).expect("deserializes");
    assert_eq!(&back, value);
}

// ---------------------------------------------------------------- exact shapes

#[test]
fn ping_request_is_flat_with_id_and_snake_case_req() {
    let frame = Request {
        id: 7,
        req: Req::Ping,
    };
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({"id": 7, "req": "ping"})
    );
}

#[test]
fn post_request_carries_its_message_as_a_msg_object() {
    let frame = Request {
        id: 8,
        req: Req::Post {
            msg: message_wire(),
        },
    };
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({
            "id": 8,
            "req": "post",
            "msg": {
                "id": "m-1",
                "channel": "ch-1",
                "thread_of": null,
                "sender": sender_hex(),
                "ts": 42,
                "encrypted": false,
                "epoch": 0,
                "body": "aGk=",
                "attachments": [{"hash": hash_hex(), "name": "a.txt", "mime": "text/plain"}],
                "sig": b64(&[7; 64]),
            }
        })
    );
}

#[test]
fn ok_frame_carries_the_reply_tagged_by_type() {
    let frame = ServerFrame::Ok {
        id: 3,
        reply: Reply::Posted { seq: 9 },
    };
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({"t": "ok", "id": 3, "reply": {"type": "posted", "seq": 9}})
    );
}

#[test]
fn err_frame_carries_the_error_code_in_snake_case() {
    let frame = ServerFrame::Err {
        id: 4,
        error: ErrorBody::new(ErrorCode::NotMember, "not a member of ch-1"),
    };
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({
            "t": "err",
            "id": 4,
            "error": {"code": "not_member", "message": "not a member of ch-1"}
        })
    );
}

#[test]
fn every_error_code_serializes_in_snake_case() {
    let cases = [
        (ErrorCode::BadRequest, "bad_request"),
        (ErrorCode::TooLarge, "too_large"),
        (ErrorCode::Unauthenticated, "unauthenticated"),
        (ErrorCode::Forbidden, "forbidden"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::NotMember, "not_member"),
        (ErrorCode::Invalid, "invalid"),
        (ErrorCode::Crypto, "crypto"),
        (ErrorCode::Quota, "quota"),
        (ErrorCode::RegistrationClosed, "registration_closed"),
        (ErrorCode::ShuttingDown, "shutting_down"),
        (ErrorCode::Internal, "internal"),
    ];
    for (code, wire) in cases {
        assert_eq!(
            serde_json::to_value(code).expect("serializes"),
            json!(wire),
            "{code:?}"
        );
    }
}

#[test]
fn message_appended_event_is_tagged_by_event_name() {
    let event = Event::MessageAppended {
        channel: "ch-1".into(),
        seq: 5,
    };
    assert_eq!(
        serde_json::to_value(&event).expect("serializes"),
        json!({"event": "message_appended", "channel": "ch-1", "seq": 5})
    );
}

#[test]
fn hello_frame_is_tagged_hello() {
    let frame = ServerFrame::Hello(ServerHello {
        version: PROTO_VERSION,
        challenge: b64(&CHALLENGE),
        daemon: "agora/0.2.0".into(),
    });
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({
            "t": "hello",
            "version": 1,
            "challenge": b64(&CHALLENGE),
            "daemon": "agora/0.2.0",
        })
    );
}

#[test]
fn welcome_frame_is_tagged_welcome() {
    let frame = ServerFrame::Welcome(Welcome {
        pubkey: sender_hex(),
        pseudonym: "amber-otter".into(),
    });
    assert_eq!(
        serde_json::to_value(&frame).expect("serializes"),
        json!({"t": "welcome", "pubkey": sender_hex(), "pseudonym": "amber-otter"})
    );
}

#[test]
fn subscribe_request_with_no_channels_is_flat_with_an_empty_list() {
    assert_wire(
        &Request {
            id: 3,
            req: Req::Subscribe { channels: vec![] },
        },
        json!({"id": 3, "req": "subscribe", "channels": []}),
    );
}

#[test]
fn subscribe_request_lists_its_channels_as_a_json_array() {
    assert_wire(
        &Request {
            id: 5,
            req: Req::Subscribe {
                channels: vec!["general".into(), "ops".into()],
            },
        },
        json!({"id": 5, "req": "subscribe", "channels": ["general", "ops"]}),
    );
}

#[test]
fn online_request_is_flat_with_only_its_tag() {
    assert_wire(
        &Request {
            id: 4,
            req: Req::Online,
        },
        json!({"id": 4, "req": "online"}),
    );
}

#[test]
fn subscribed_reply_is_tagged_inside_an_ok_frame() {
    assert_wire(
        &ServerFrame::Ok {
            id: 3,
            reply: Reply::Subscribed {
                channels: vec!["general".into(), "ops".into()],
            },
        },
        json!({
            "t": "ok",
            "id": 3,
            "reply": {"type": "subscribed", "channels": ["general", "ops"]}
        }),
    );
}

#[test]
fn online_reply_is_tagged_inside_an_ok_frame() {
    assert_wire(
        &ServerFrame::Ok {
            id: 4,
            reply: Reply::Online {
                pubkeys: vec![sender_hex()],
            },
        },
        json!({
            "t": "ok",
            "id": 4,
            "reply": {"type": "online", "pubkeys": [sender_hex()]}
        }),
    );
}

#[test]
fn lagged_event_frame_has_the_exact_shape() {
    assert_wire(
        &ServerFrame::Event {
            event: Event::Lagged { dropped: 3 },
        },
        json!({"t": "event", "event": {"event": "lagged", "dropped": 3}}),
    );
}

#[test]
fn message_appended_event_frame_has_the_exact_shape() {
    assert_wire(
        &ServerFrame::Event {
            event: Event::MessageAppended {
                channel: "ch-1".into(),
                seq: 5,
            },
        },
        json!({
            "t": "event",
            "event": {"event": "message_appended", "channel": "ch-1", "seq": 5}
        }),
    );
}

#[test]
fn work_events_have_the_exact_shapes() {
    assert_wire(
        &Event::WorkChanged {
            task: "task-1".into(),
        },
        json!({"event": "work_changed", "task": "task-1"}),
    );
    assert_wire(
        &Event::NoticePosted {
            target: sender_hex(),
            seq: 4,
        },
        json!({"event": "notice_posted", "target": sender_hex(), "seq": 4}),
    );
    assert_wire(
        &Event::WorkSettingsChanged,
        json!({"event": "work_settings_changed"}),
    );
}

#[test]
fn work_wait_request_nests_the_tagged_wait_spec() {
    assert_wire(
        &Request {
            id: 4,
            req: Req::WorkWait {
                spec: WaitSpec::Log {
                    path: "/tmp/x".into(),
                    pattern: "done".into(),
                },
                timeout_ms: 1_000,
                cursor: None,
            },
        },
        json!({
            "id": 4, "req": "work_wait", "timeout_ms": 1000, "cursor": null,
            "spec": {"kind": "log", "path": "/tmp/x", "pattern": "done"}
        }),
    );
}

#[test]
fn every_request_variant_is_tagged_with_its_snake_case_name() {
    for (tag, req) in every_request() {
        let value = serde_json::to_value(Request { id: 1, req }).expect("serializes");
        assert_eq!(value["req"], json!(tag));
    }
}

#[test]
fn every_reply_variant_is_tagged_with_its_snake_case_name() {
    for (tag, reply) in every_reply() {
        let value = serde_json::to_value(reply).expect("serializes");
        assert_eq!(value["type"], json!(tag));
    }
}

// ---------------------------------------------------------------- round trips

#[test]
fn every_request_variant_round_trips_through_json() {
    for (tag, req) in every_request() {
        let sent = Request { id: 11, req };
        let text = serde_json::to_string(&sent).expect("serializes");
        let back: Request = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, sent, "variant {tag}");
    }
}

#[test]
fn every_reply_variant_round_trips_through_json() {
    for (tag, reply) in every_reply() {
        let text = serde_json::to_string(&reply).expect("serializes");
        let back: Reply = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, reply, "variant {tag}");
    }
}

#[test]
fn request_with_an_unknown_req_tag_is_rejected() {
    serde_json::from_str::<Request>(r#"{"id":1,"req":"teleport"}"#).expect_err("unknown req");
}

#[test]
fn request_without_an_id_is_rejected() {
    serde_json::from_str::<Request>(r#"{"req":"ping"}"#).expect_err("missing id");
}

#[test]
fn subscribe_with_a_non_array_channels_field_is_rejected() {
    serde_json::from_str::<Request>(r#"{"id":1,"req":"subscribe","channels":"general"}"#)
        .expect_err("channels must be an array");
}

// ---------------------------------------------------------------- conversions

#[test]
fn signed_op_keeps_actor_signature_and_fields_across_the_wire() {
    let signer = id(1);
    let op = SignedOp::new("channel.add", "ch-1", "{\"pk\":\"x\"}", 1_000, "n-42").sign(&signer);
    let text = serde_json::to_string(&OpWire::from_op(&op)).expect("serializes");
    let wire: OpWire = serde_json::from_str(&text).expect("deserializes");
    let back = wire.to_op().expect("decodes");
    assert_eq!(back.kind, op.kind);
    assert_eq!(back.subject, op.subject);
    assert_eq!(back.arg, op.arg);
    assert_eq!(back.ts, op.ts);
    assert_eq!(back.nonce, op.nonce);
    assert_eq!(back.actor, signer.public());
    assert_eq!(back.sig, op.sig);
}

#[test]
fn signed_message_still_verifies_after_a_json_round_trip() {
    let signer = id(2);
    let msg = SignedMessage {
        id: "m-1".into(),
        channel: "ch-1".into(),
        thread_of: Some("m-0".into()),
        sender: [0; 32],
        ts: 42,
        encrypted: true,
        epoch: 3,
        body: vec![1, 2, 3, 4, 5, 6],
        attachments: vec![Attachment {
            hash: hash_hex(),
            name: "a.txt".into(),
            mime: "text/plain".into(),
        }],
        sig: vec![],
    }
    .sign(&signer);
    let text = serde_json::to_string(&MessageWire::from_message(&msg)).expect("serializes");
    let wire: MessageWire = serde_json::from_str(&text).expect("deserializes");
    let back = wire.to_message().expect("decodes");
    back.verify().expect("signature still holds");
}

#[test]
fn tampered_message_body_fails_verification() {
    let signer = id(2);
    let msg = SignedMessage {
        id: "m-1".into(),
        channel: "ch-1".into(),
        thread_of: None,
        sender: [0; 32],
        ts: 42,
        encrypted: false,
        epoch: 0,
        body: b"original".to_vec(),
        attachments: vec![],
        sig: vec![],
    }
    .sign(&signer);
    let mut wire = MessageWire::from_message(&msg);
    wire.body = b64(b"tampered");
    let back = wire.to_message().expect("still decodes");
    assert!(matches!(
        back.verify(),
        Err(StoreError::Crypto(CryptoError::BadSignature))
    ));
}

#[test]
fn every_hex_field_names_itself_when_it_is_not_lowercase_hex() {
    // Uppercase is rejected: the wire form is lowercase only.
    let upper = "AB".repeat(32);
    let cases = [
        (
            "actor",
            OpWire {
                actor: upper.clone(),
                ..op_wire()
            }
            .to_op()
            .map(|_| ()),
        ),
        (
            "sender",
            MessageWire {
                sender: upper.clone(),
                ..message_wire()
            }
            .to_message()
            .map(|_| ()),
        ),
        (
            "by",
            ReactionWire {
                by: upper.clone(),
                ..reaction_wire()
            }
            .to_reaction()
            .map(|_| ()),
        ),
        (
            "author",
            NoteWire {
                author: upper.clone(),
                ..note_wire()
            }
            .to_note()
            .map(|_| ()),
        ),
    ];
    for (field, result) in cases {
        match result {
            Err(ProtoError::Encoding(name)) => assert_eq!(name, field),
            other => panic!("{field}: expected Encoding, got {other:?}"),
        }
    }
}

#[test]
fn every_base64_field_names_itself_when_it_is_malformed() {
    let unpadded_sig = b64(&[9; 64]).trim_end_matches('=').to_string();
    let cases = [
        (
            "body",
            MessageWire {
                body: "not base64!".into(),
                ..message_wire()
            }
            .to_message()
            .map(|_| ()),
        ),
        (
            "sig",
            OpWire {
                sig: unpadded_sig,
                ..op_wire()
            }
            .to_op()
            .map(|_| ()),
        ),
        (
            "sig",
            ClientHello {
                version: PROTO_VERSION,
                pubkey: sender_hex(),
                sig: "!!".into(),
                label: "folder".into(),
            }
            .verify(&CHALLENGE)
            .map(|_| ()),
        ),
    ];
    for (field, result) in cases {
        match result {
            Err(ProtoError::Encoding(name)) => assert_eq!(name, field),
            other => panic!("{field}: expected Encoding, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------- framing

#[tokio::test]
async fn server_frames_round_trip_in_order_over_a_small_pipe() {
    let frames = vec![
        ServerFrame::Hello(ServerHello {
            version: PROTO_VERSION,
            challenge: b64(&CHALLENGE),
            daemon: "agora/0.2.0".into(),
        }),
        ServerFrame::Welcome(Welcome {
            pubkey: sender_hex(),
            pseudonym: "amber-otter".into(),
        }),
        ServerFrame::Ok {
            id: 1,
            reply: Reply::Pong,
        },
        ServerFrame::Err {
            id: 2,
            error: ErrorBody::new(ErrorCode::Forbidden, "no"),
        },
        ServerFrame::Event {
            event: Event::MessageAppended {
                channel: "ch-1".into(),
                seq: 5,
            },
        },
    ];
    // The pipe is smaller than the encoded frames, so the reader must drain it as the
    // writer fills it.
    let (mut a, mut b) = duplex(64);
    let to_write = frames.clone();
    let writer = tokio::spawn(async move {
        for frame in &to_write {
            write_json(&mut a, frame).await.expect("write frame");
        }
    });

    for expected in &frames {
        let got: Option<ServerFrame> = read_json(&mut b).await.expect("read frame");
        assert_eq!(got.as_ref(), Some(expected));
    }
    writer.await.expect("writer task");
    let end: Option<ServerFrame> = read_json(&mut b).await.expect("clean end");
    assert_eq!(end, None);
}

#[tokio::test]
async fn frame_declaring_more_than_max_frame_is_rejected_before_its_payload() {
    let (mut a, mut b) = duplex(64);
    let declared = u32::try_from(MAX_FRAME + 1).expect("fits u32");
    a.write_all(&declared.to_be_bytes())
        .await
        .expect("write header");
    drop(a);
    let err = read_json::<ServerFrame, _>(&mut b)
        .await
        .expect_err("over the limit");
    assert!(
        matches!(err, ProtoError::FrameTooLarge(n) if n == MAX_FRAME + 1),
        "got {err:?}"
    );
}

#[tokio::test]
async fn frame_whose_payload_is_not_json_is_a_json_error() {
    let (mut a, mut b) = duplex(64);
    write_frame(&mut a, b"{not json")
        .await
        .expect("write frame");
    let err = read_json::<ServerFrame, _>(&mut b)
        .await
        .expect_err("malformed payload");
    assert!(matches!(err, ProtoError::Json(_)), "got {err:?}");
}

#[tokio::test]
async fn lagged_event_frame_is_length_prefixed_json_and_decodes_back() {
    let frame = ServerFrame::Event {
        event: Event::Lagged { dropped: 3 },
    };
    let json: &[u8] = br#"{"t":"event","event":{"event":"lagged","dropped":3}}"#;
    let mut wire = Vec::new();
    write_json(&mut wire, &frame).await.expect("write frame");

    let mut expected = u32::try_from(json.len())
        .expect("fits u32")
        .to_be_bytes()
        .to_vec();
    expected.extend_from_slice(json);
    assert_eq!(wire, expected);

    let mut reader = wire.as_slice();
    let payload = read_frame(&mut reader, MAX_FRAME)
        .await
        .expect("read frame")
        .expect("one frame");
    let back: ServerFrame = serde_json::from_slice(&payload).expect("decodes");
    assert_eq!(back, frame);
}

// ---------------------------------------------------------------- handshake

#[test]
fn client_hello_verifies_for_the_challenge_it_signed() {
    let signer = id(9);
    let hello = ClientHello::sign(&signer, &CHALLENGE, "folder");
    assert_eq!(hello.verify(&CHALLENGE).expect("verifies"), signer.public());
}

#[test]
fn client_hello_is_rejected_for_a_different_challenge() {
    let hello = ClientHello::sign(&id(9), &CHALLENGE, "folder");
    assert!(matches!(
        hello.verify(&[6; 32]),
        Err(ProtoError::Crypto(CryptoError::BadSignature))
    ));
}

#[test]
fn client_hello_is_rejected_for_a_wrong_protocol_version() {
    let mut hello = ClientHello::sign(&id(9), &CHALLENGE, "folder");
    hello.version = PROTO_VERSION + 1;
    assert!(matches!(
        hello.verify(&CHALLENGE),
        Err(ProtoError::Encoding("version"))
    ));
}

#[test]
fn client_hello_is_rejected_for_a_garbage_pubkey() {
    let mut hello = ClientHello::sign(&id(9), &CHALLENGE, "folder");
    hello.pubkey = "not-a-key".into();
    assert!(matches!(
        hello.verify(&CHALLENGE),
        Err(ProtoError::Encoding("pubkey"))
    ));
}

#[test]
fn client_hello_signature_is_bound_to_the_pubkey_it_names() {
    // Claim another identity's key while keeping this identity's signature.
    let mut hello = ClientHello::sign(&id(9), &CHALLENGE, "folder");
    hello.pubkey = hex(&id(10).public());
    assert!(matches!(
        hello.verify(&CHALLENGE),
        Err(ProtoError::Crypto(CryptoError::BadSignature))
    ));
}
