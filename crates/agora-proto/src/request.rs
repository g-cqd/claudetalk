use crate::{KeyWire, MessageWire, NoteWire, OpWire, ReactionWire};
use agora_sched::model::{LeaseKind, TaskFilter, WaitSpec};
use agora_sched::order::OrderPolicy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
