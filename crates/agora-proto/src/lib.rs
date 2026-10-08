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
pub use convert::*;
pub use frame::*;

use agora_sched::model::{
    BoardExtras, CardExt as CardExtRecord, LeaseKind, LeaseRecord, MetricsReport, NextOutcome,
    NoticeRecord, PolicyRecord, PoolRecord, ScopeRecord, StatusReport, TaskFilter, TaskRecord,
    TimerRecord, WaitResult, WaitSpec, WatchdogReport, WorkEvent,
};
use agora_sched::order::OrderPolicy;
use agora_sched::settings::WorkSettings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the client, echoed in the answer. 0 is reserved for connection-level errors.
    pub id: u64,
    #[serde(flatten)]
    pub req: Req,
}

/// Every operation. The reader identity of every read is the connection's
/// authenticated key; it is never a request field. Every signed input must be
/// signed by the connection's key (else `forbidden`).
///
/// Not `Eq`: the scheduling payloads carry floating-point numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Start receiving [`ServerFrame::Event`] frames on this connection. Empty
    /// `channels` means every channel the connection key is a member of, now and
    /// later; otherwise exactly the listed channels, each of which must be a current
    /// membership (else `not_member` naming the first offending id, the same error
    /// for a missing channel). The key must be active. A connection subscribes at
    /// most once (a second request is `invalid`). Answered with [`Reply::Subscribed`]
    /// before any event; every event committed after that reply is delivered.
    Subscribe {
        channels: Vec<String>,
    },
    /// Hex keys that are online (hold a subscribed connection) and share at least one
    /// channel with the caller, plus the caller itself if online. Never anyone else.
    Online,
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
    // ---- M5: work queue (agora-sched). Every mutation carries a signed op.
    WorkScope {
        op: OpWire,
    },
    WorkScopes {
        root: Option<String>,
    },
    WorkAdd {
        op: OpWire,
    },
    /// `task`, not `id`: the request's own `id` is flattened next to the variant's fields.
    WorkShow {
        task: String,
    },
    WorkNext {
        op: OpWire,
    },
    WorkClaim {
        op: OpWire,
    },
    WorkAccept {
        op: OpWire,
    },
    /// `env` is the caller's scrubbed environment; it is kept in memory until the job
    /// spawns and is never stored.
    WorkRun {
        op: OpWire,
        env: BTreeMap<String, String>,
    },
    WorkFinish {
        op: OpWire,
    },
    WorkCancel {
        op: OpWire,
    },
    WorkUpdate {
        op: OpWire,
    },
    WorkReclaim {
        op: OpWire,
    },
    WorkStatus {
        scope: Option<String>,
    },
    WorkList {
        filter: TaskFilter,
    },
    WorkLog {
        task: Option<String>,
        actor: Option<String>,
        scope: Option<String>,
        limit: u32,
    },
    WorkPlan {
        scope: Option<String>,
        policy: OrderPolicy,
        agents: u32,
        assign: bool,
        conflict_cost: Option<f64>,
    },
    /// Returns before `min(timeout_ms, MAX_WAIT_MS)`; `cursor` resumes a previous wait.
    WorkWait {
        spec: WaitSpec,
        timeout_ms: u64,
        cursor: Option<String>,
    },
    /// `op: None` reads the settings; `Some` is a signed `work.settings` (Controller+).
    WorkSettings {
        op: Option<OpWire>,
    },
    TimerCreate {
        op: OpWire,
    },
    TimerDelete {
        op: OpWire,
    },
    TimerList {
        owner: Option<String>,
    },
    PoolDefine {
        op: OpWire,
    },
    PoolList,
    LeaseAcquire {
        op: OpWire,
    },
    LeaseRenew {
        op: OpWire,
    },
    LeaseRelease {
        op: OpWire,
    },
    LeaseList {
        kind: Option<LeaseKind>,
        holder: Option<String>,
        active_only: bool,
    },
    QuietOpen {
        op: OpWire,
    },
    QuietClose {
        op: OpWire,
    },
    PolicySet {
        op: OpWire,
    },
    PolicyList,
    CardUpdate {
        op: OpWire,
    },
    CardDep {
        op: OpWire,
    },
    CardLink {
        op: OpWire,
    },
    CardEscalate {
        op: OpWire,
    },
    CardExt {
        ids: Vec<String>,
    },
    WatchdogReport,
    Metrics {
        team: Option<String>,
        lane: Option<String>,
        since_ms: Option<i64>,
    },
    BoardExtras,
    Notices {
        after: i64,
        limit: u32,
    },
}

// ---------------------------------------------------------------- replies

/// Not `Eq`: the scheduling payloads carry floating-point numbers. Replies are built, serialized and dropped,
/// never stored in bulk, so the size of the `TaskRecord` variants is not worth a `Box` in every constructor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
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
    /// The effective channel set of the subscription, sorted.
    Subscribed {
        channels: Vec<String>,
    },
    /// Sorted hex keys, see [`Req::Online`].
    Online {
        pubkeys: Vec<String>,
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
    // ---- M5: work queue (agora-sched)
    Task {
        task: TaskRecord,
    },
    Tasks {
        tasks: Vec<TaskRecord>,
    },
    Next {
        outcome: NextOutcome,
    },
    Accepted {
        task: TaskRecord,
        first: bool,
    },
    Scope {
        scope: ScopeRecord,
    },
    Scopes {
        scopes: Vec<ScopeRecord>,
    },
    WorkStatus {
        report: StatusReport,
    },
    WorkLog {
        events: Vec<WorkEvent>,
    },
    WorkPlan {
        json: String,
        text: String,
    },
    Wait {
        result: WaitResult,
    },
    WorkSettings {
        settings: WorkSettings,
        view: serde_json::Value,
        text: String,
    },
    Timer {
        timer: TimerRecord,
    },
    Timers {
        timers: Vec<TimerRecord>,
    },
    Pool {
        pool: PoolRecord,
    },
    Pools {
        pools: Vec<PoolRecord>,
    },
    Lease {
        lease: LeaseRecord,
    },
    Leases {
        leases: Vec<LeaseRecord>,
    },
    Policy {
        policy: PolicyRecord,
    },
    Policies {
        policies: Vec<PolicyRecord>,
    },
    CardExts {
        cards: Vec<CardExtRecord>,
    },
    Escalated {
        notified: Vec<String>,
    },
    Watchdog {
        report: WatchdogReport,
    },
    Metrics {
        report: MetricsReport,
    },
    BoardExtras {
        extras: BoardExtras,
    },
    Notices {
        notices: Vec<NoticeRecord>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
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
    /// Sent only on a subscribed connection (see [`Req::Subscribe`]), and only after
    /// the commit the event reports. Carries no request id.
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
    /// A work task changed (added, claimed, started, finished, cancelled ...).
    WorkChanged {
        task: String,
    },
    LeaseChanged {
        lease: String,
    },
    TimerChanged {
        timer: String,
    },
    /// A notice was stored for `target` (hex key).
    NoticePosted {
        target: String,
        seq: i64,
    },
    WorkSettingsChanged,
    PolicyChanged,
    /// Never published on the bus; sent only to a subscriber whose events were
    /// dropped (slow consumer or bus overflow). Means "you missed some
    /// notifications; re-read your inbox". The inbox cursor is the source of truth,
    /// so dropped notifications never lose messages.
    Lagged {
        dropped: u64,
    },
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
