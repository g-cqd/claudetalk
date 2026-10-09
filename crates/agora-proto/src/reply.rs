use crate::{
    CardView, ChannelView, EmojiCount, MemberView, MessageView, NoteHit, ServerHello,
    SprintSummaryView, Welcome,
};
use agora_sched::model::{
    BoardExtras, CardExt as CardExtRecord, LeaseRecord, MetricsReport, NextOutcome, NoticeRecord,
    PolicyRecord, PoolRecord, ScopeRecord, StatusReport, TaskRecord, TimerRecord, WaitResult,
    WatchdogReport, WorkEvent,
};
use agora_sched::settings::WorkSettings;
use serde::{Deserialize, Serialize};

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
