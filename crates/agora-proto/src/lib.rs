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

mod convert;
mod frame;
pub use convert::*;
pub use frame::*;

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
    /// e.g. "agora/0.1.0".
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: i64,
}

// ---------------------------------------------------------------- requests

/// A client request: `{"id": 7, "req": "post", "msg": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the client, echoed in the answer. 0 is reserved for connection-level errors.
    pub id: u64,
    #[serde(flatten)]
    pub req: Req,
}

/// Every operation. The reader identity of every read is the connection's
/// authenticated key; it is never a request field. Every signed input must be
/// signed by the connection's key (else `forbidden`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum Req {
    Ping,
    Whoami,
    /// Register the connection key (local-mode policy: `open_registration`).
    Register {
        label: String,
    },
    /// Make the connection key the owner: only when no owner exists and the peer uid
    /// equals the daemon uid.
    BootstrapOwner,
    /// Stop the daemon gracefully (any authenticated same-uid connection).
    Shutdown,
    /// base64 bytes -> BlobHash.
    PutBlob {
        bytes: String,
    },
    GetBlob {
        hash: String,
    },
    Post {
        msg: MessageWire,
    },
    /// Messages with seq > after, oldest first.
    Read {
        channel: String,
        after: i64,
        limit: i64,
    },
    /// All unacked messages for the connection key, oldest first.
    Inbox {
        limit: i64,
    },
    /// Unacked messages with seq > after; if none, wait (event-driven) up to
    /// min(timeout_ms, MAX_WAIT_MS) for one.
    InboxWait {
        after: i64,
        timeout_ms: u64,
        limit: i64,
    },
    Ack {
        channel: String,
        seq: i64,
    },
    React {
        reaction: ReactionWire,
    },
    ReactionCounts {
        kind: String,
        target: String,
    },
    ChannelCreate {
        op: OpWire,
        keys: Vec<KeyWire>,
    },
    /// `wrapped`: base64 wrapped current-epoch key for the new member (encrypted channels).
    ChannelAdd {
        op: OpWire,
        wrapped: Option<String>,
    },
    ChannelRemove {
        op: OpWire,
        keys: Vec<KeyWire>,
    },
    ChannelRole {
        op: OpWire,
    },
    ChannelRotate {
        op: OpWire,
        keys: Vec<KeyWire>,
    },
    ChannelList,
    ChannelInfo {
        channel: String,
    },
    /// The connection key's own wrapped key; epoch None = current.
    ChannelKey {
        channel: String,
        epoch: Option<i64>,
    },
    RoleGrant {
        op: OpWire,
    },
    Revoke {
        op: OpWire,
    },
    CardCreate {
        op: OpWire,
    },
    CardMove {
        op: OpWire,
    },
    CardEvidence {
        op: OpWire,
    },
    CardApprove {
        op: OpWire,
    },
    CardReclaim {
        op: OpWire,
    },
    CardList {
        state: Option<String>,
        lane: Option<String>,
        limit: i64,
    },
    SprintCreate {
        op: OpWire,
    },
    SprintAdd {
        op: OpWire,
    },
    SprintStart {
        op: OpWire,
    },
    SprintClose {
        op: OpWire,
    },
    SprintSummary {
        sprint: String,
    },
    NotePublish {
        note: NoteWire,
    },
    NoteSearch {
        query: String,
        limit: i64,
    },
}

// ---------------------------------------------------------------- replies

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Pong,
    Unit,
    Whoami {
        pubkey: String,
        pseudonym: String,
        /// Role name when registered and active.
        role: Option<String>,
        registered: bool,
        revoked: bool,
        owner_exists: bool,
    },
    BlobHash {
        hash: String,
    },
    /// base64
    Blob {
        bytes: String,
    },
    Posted {
        seq: i64,
    },
    Messages {
        messages: Vec<MessageView>,
    },
    /// `cursor` = max(after, largest seq returned); pass it as the next `after`.
    Inbox {
        messages: Vec<MessageView>,
        cursor: i64,
        timed_out: bool,
    },
    Counts {
        counts: Vec<EmojiCount>,
    },
    Channels {
        channels: Vec<ChannelView>,
    },
    Channel {
        channel: ChannelView,
        members: Vec<MemberView>,
    },
    /// The caller's wrapped key: unwrap with `unwrap_key(wrapper, channel, epoch, wrapped)`.
    Key {
        epoch: i64,
        wrapper: String,
        wrapped: String,
    },
    Epoch {
        epoch: i64,
    },
    Cards {
        cards: Vec<CardView>,
    },
    Sprint {
        summary: SprintSummaryView,
    },
    Notes {
        notes: Vec<NoteHit>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed frame/JSON/field encoding.
    BadRequest,
    /// Frame or blob over its limit.
    TooLarge,
    /// Handshake failed (bad proof, wrong uid, timeout).
    Unauthenticated,
    Forbidden,
    NotFound,
    NotMember,
    Invalid,
    Crypto,
    Quota,
    RegistrationClosed,
    ShuttingDown,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

/// Everything the server sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello(ServerHello),
    Welcome(Welcome),
    Ok {
        id: u64,
        reply: Reply,
    },
    Err {
        id: u64,
        error: ErrorBody,
    },
    /// Reserved for subscriptions (M3); the M2 daemon never sends it.
    Event {
        event: Event,
    },
}

// ---------------------------------------------------------------- events

/// In-process bus events, published by the writer after each successful commit,
/// in commit order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    MessageAppended {
        channel: String,
        seq: i64,
    },
    ReactionChanged {
        kind: String,
        target: String,
    },
    /// Create, membership, role or key change.
    ChannelChanged {
        channel: String,
    },
    CardChanged {
        card: String,
    },
    SprintChanged {
        sprint: String,
    },
    NotePublished {
        id: String,
    },
    /// Registration, owner bootstrap, role grant, revoke (hex64).
    IdentityChanged {
        pubkey: String,
    },
    Acked {
        reader: String,
        channel: String,
        seq: i64,
    },
    BlobStored {
        hash: String,
    },
}
