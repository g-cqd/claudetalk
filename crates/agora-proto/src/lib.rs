//! The wire between `agora` clients and the daemon.
//!
//! * Framing: 4-byte big-endian length, then that many bytes of UTF-8 JSON;
//!   a frame longer than [`MAX_FRAME`] is a protocol error.
//! * Encoding rule: public keys and content hashes are lowercase hex (64 chars);
//!   every other byte string (bodies, blobs, signatures, wrapped keys, challenges)
//!   is standard base64 with padding.
//! * Session: server sends [`ServerFrame::Hello`] with a random challenge; the
//!   client answers with a [`ClientHello`] signed by its identity key; the server
//!   answers [`ServerFrame::Welcome`] (or `Err{id:0}` and closes). After that the
//!   client sends [`Request`] frames and the server answers each with
//!   `Ok{id,..}` / `Err{id,..}` carrying the same id, in any order.
//! * Concurrency: requests on one connection are processed concurrently, so replies
//!   may arrive in any order. A client must await a reply before it sends a request
//!   that depends on that reply.
//! * Subscriptions: after [`Req::Subscribe`] the server also pushes
//!   [`ServerFrame::Event`] frames (no id) on that connection, each only after the
//!   commit it reports. Events are notifications, not the source of truth: a slow
//!   subscriber gets [`Event::Lagged`] and re-reads its inbox, whose cursor never
//!   loses a message.

mod convert;
mod frame;
mod reply;
mod request;
pub use convert::*;
pub use frame::*;
pub use reply::*;
pub use request::*;

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;
/// Largest frame payload in bytes (the 4 length bytes are not counted).
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Domain of the handshake signature: `canonical(HELLO_DOMAIN, [challenge, pubkey])`.
pub const HELLO_DOMAIN: &str = "hello/v1";
/// Longest wait `inbox_wait` honours; larger requests are clamped.
pub const MAX_WAIT_MS: u64 = 55_000;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    /// A field that is not valid hex/base64 or has the wrong length; names the field.
    #[error("invalid {0}")]
    Encoding(&'static str),
    #[error(transparent)]
    Crypto(#[from] agora_crypto::CryptoError),
}

// ---------------------------------------------------------------- handshake

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u32,
    /// base64 of 32 random bytes.
    pub challenge: String,
    /// e.g. "agora/0.2.0".
    pub daemon: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u32,
    /// hex64 Ed25519 public key.
    pub pubkey: String,
    /// base64 signature over `hello_signing_bytes(challenge, pubkey)`.
    pub sig: String,
    /// Free text shown in logs (folder name); at most 64 bytes is used.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub pubkey: String,
    pub pseudonym: String,
}

// ---------------------------------------------------------------- signed inputs

/// `agora_store::SignedOp` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpWire {
    pub kind: String,
    pub subject: String,
    pub arg: String,
    pub ts: i64,
    pub nonce: String,
    /// hex64
    pub actor: String,
    /// base64
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentWire {
    /// hex64 blake3 of the stored (possibly encrypted) blob.
    pub hash: String,
    pub name: String,
    pub mime: String,
}

/// `agora_store::SignedMessage` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageWire {
    pub id: String,
    pub channel: String,
    pub thread_of: Option<String>,
    /// hex64
    pub sender: String,
    pub ts: i64,
    pub encrypted: bool,
    pub epoch: i64,
    /// base64 of plaintext, or of nonce||ciphertext when `encrypted`.
    pub body: String,
    pub attachments: Vec<AttachmentWire>,
    /// base64
    pub sig: String,
}

/// A stored message: its global sequence number plus everything needed to re-verify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageView {
    pub seq: i64,
    pub msg: MessageWire,
}

/// `agora_store::SignedReaction` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionWire {
    /// "message" | "file" | "card"
    pub kind: String,
    pub target: String,
    /// hex64
    pub by: String,
    pub emoji: String,
    pub remove: bool,
    pub ts: i64,
    /// base64
    pub sig: String,
}

/// `agora_store::SignedNote` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteWire {
    pub id: String,
    /// hex64
    pub author: String,
    pub ts: i64,
    pub title: String,
    pub body: String,
    pub tags: String,
    pub supersedes: Option<String>,
    /// base64
    pub sig: String,
}

/// One wrapped channel key for one member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyWire {
    /// hex64 member public key.
    pub pk: String,
    /// base64 of the 72-byte wrapped key.
    pub wrapped: String,
}

// ---------------------------------------------------------------- views

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub parent: Option<String>,
    pub epoch: i64,
    pub encrypted: bool,
    pub rotation_pending: bool,
    pub my_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberView {
    /// hex64
    pub pk: String,
    pub pseudonym: String,
    /// "admin" | "member"
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardView {
    pub id: String,
    pub title: String,
    pub lane: String,
    /// agora_core::CardState::as_str()
    pub state: String,
    /// hex64 owner, if claimed.
    pub owner: Option<String>,
    pub updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SprintSummaryView {
    pub total: i64,
    pub done: i64,
    pub in_flight: i64,
    pub blocked: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteHit {
    pub id: String,
    pub title: String,
}

/// One status row of [`Reply::Statuses`]: what one identity on one harness is
/// doing, as that identity last signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusView {
    /// hex64
    pub pk: String,
    pub pseudonym: String,
    /// Short free text, e.g. online | busy | idle | away.
    pub state: String,
    /// Harness name, e.g. codex, claude, vibe, cli. Two harnesses sharing one
    /// folder key keep one row each.
    pub harness: String,
    /// The folder the sender works in.
    pub folder: String,
    /// What the sender is working on.
    pub matter: String,
    /// ms since epoch of the last `status.set`.
    pub ts: i64,
    /// The daemon's presence flag: a subscribed connection is held for this key.
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T>(value: &T, expected: serde_json::Value)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_value(value).expect("serialize"), expected);
        let back: T = serde_json::from_value(expected).expect("deserialize");
        assert_eq!(&back, value);
    }

    #[test]
    fn subscribe_request_wire_shape() {
        roundtrip(
            &Request {
                id: 1,
                req: Req::Subscribe { channels: vec![] },
            },
            json!({"id": 1, "req": "subscribe", "channels": []}),
        );
        roundtrip(
            &Request {
                id: 2,
                req: Req::Subscribe {
                    channels: vec!["general".into(), "ops".into()],
                },
            },
            json!({"id": 2, "req": "subscribe", "channels": ["general", "ops"]}),
        );
    }

    #[test]
    fn online_request_wire_shape() {
        roundtrip(
            &Request {
                id: 3,
                req: Req::Online,
            },
            json!({"id": 3, "req": "online"}),
        );
    }

    #[test]
    fn status_request_wire_shapes() {
        roundtrip(
            &Request {
                id: 4,
                req: Req::Statuses,
            },
            json!({"id": 4, "req": "statuses"}),
        );
        roundtrip(
            &Request {
                id: 5,
                req: Req::StatusSet {
                    op: OpWire {
                        kind: "status.set".into(),
                        subject: "ab".repeat(32),
                        arg: r#"{"state":"busy"}"#.into(),
                        ts: 7,
                        nonce: "n".into(),
                        actor: "ab".repeat(32),
                        sig: "s".into(),
                    },
                },
            },
            json!({"id": 5, "req": "status_set", "op": {
                "kind": "status.set", "subject": "ab".repeat(32),
                "arg": r#"{"state":"busy"}"#, "ts": 7, "nonce": "n",
                "actor": "ab".repeat(32), "sig": "s"}}),
        );
    }

    #[test]
    fn statuses_reply_wire_shape() {
        roundtrip(
            &Reply::Statuses {
                statuses: vec![StatusView {
                    pk: "ab".repeat(32),
                    pseudonym: "abcd0123".into(),
                    state: "busy".into(),
                    harness: "codex".into(),
                    folder: "~/x".into(),
                    matter: "card-1".into(),
                    ts: 7,
                    online: true,
                }],
            },
            json!({"type": "statuses", "statuses": [{
                "pk": "ab".repeat(32), "pseudonym": "abcd0123", "state": "busy",
                "harness": "codex", "folder": "~/x", "matter": "card-1",
                "ts": 7, "online": true}]}),
        );
    }

    #[test]
    fn subscribed_and_online_reply_wire_shapes() {
        roundtrip(
            &Reply::Subscribed {
                channels: vec!["a".into(), "b".into()],
            },
            json!({"type": "subscribed", "channels": ["a", "b"]}),
        );
        roundtrip(
            &Reply::Online {
                pubkeys: vec!["ab".repeat(32)],
            },
            json!({"type": "online", "pubkeys": ["ab".repeat(32)]}),
        );
    }

    #[test]
    fn lagged_event_wire_shape_inside_a_frame() {
        roundtrip(
            &Event::Lagged { dropped: 3 },
            json!({"event": "lagged", "dropped": 3}),
        );
        roundtrip(
            &ServerFrame::Event {
                event: Event::Lagged { dropped: 7 },
            },
            json!({"t": "event", "event": {"event": "lagged", "dropped": 7}}),
        );
    }
}
