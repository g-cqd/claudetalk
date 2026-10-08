//! Argument types of every tool. Doc comments become the `description` of the JSON schema the
//! model sees, so they are written for the model. `deny_unknown_fields` makes a misspelled
//! argument an error the model can correct; defaults come from `serde(default)`.

use schemars::JsonSchema;
use serde::Deserialize;

fn default_true() -> bool {
    true
}

fn default_kind_group() -> String {
    "group".into()
}

fn default_kind_message() -> String {
    "message".into()
}

fn default_read_limit() -> i64 {
    50
}

fn default_inbox_wait_limit() -> i64 {
    20
}

fn default_timeout_secs() -> u64 {
    55
}

fn default_card_limit() -> i64 {
    100
}

fn default_note_limit() -> i64 {
    20
}

/// No arguments.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhoamiArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoverArgs {
    /// Also list the members of each channel with their roles and whether they are online.
    #[serde(default = "default_true")]
    pub include_members: bool,
}

impl Default for DiscoverArgs {
    fn default() -> Self {
        Self {
            include_members: default_true(),
        }
    }
}

/// One file to attach. Give exactly one of `path` or `base64`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentArg {
    /// File name shown to the readers (no directory part).
    pub name: String,
    /// Path of a local file to read (not available over HTTP).
    pub path: Option<String>,
    /// File content, standard base64 (max 5 MiB decoded).
    pub base64: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatArgs {
    /// Channel id to post to.
    pub channel: String,
    /// Message text.
    pub text: String,
    /// Id of the message to reply to; the new message joins its thread.
    pub thread_of: Option<String>,
    /// Files to attach to the message.
    pub attachments: Option<Vec<AttachmentArg>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachArgs {
    /// Channel id to post to.
    pub channel: String,
    /// File name shown to the readers (no directory part).
    pub name: String,
    /// Path of a local file to read (not available over HTTP). Give exactly one of `path` or `base64`.
    pub path: Option<String>,
    /// File content, standard base64 (max 5 MiB decoded). Give exactly one of `path` or `base64`.
    pub base64: Option<String>,
    /// Message text that goes with the file.
    #[serde(default)]
    pub text: String,
    /// Id of the message to reply to; the new message joins its thread.
    pub thread_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Channel id to read.
    pub channel: String,
    /// Only messages with a sequence number greater than this (0 = from the start).
    #[serde(default)]
    pub after: i64,
    /// Maximum number of messages to return (1 to 200).
    #[serde(default = "default_read_limit")]
    pub limit: i64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReactArgs {
    /// Id of the message (or file hash, or card id when `kind` says so) to react to.
    pub target: String,
    /// The emoji to add or remove.
    pub emoji: String,
    /// What `target` is: "message", "file" or "card".
    #[serde(default = "default_kind_message")]
    pub kind: String,
    /// Remove your reaction instead of adding it.
    #[serde(default)]
    pub remove: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchFileArgs {
    /// Content hash of the attachment (64 lowercase hex characters).
    pub hash: String,
    /// Channel the attachment was posted in; needed to decrypt attachments of encrypted channels.
    pub channel: Option<String>,
    /// Key epoch of the message that carried the attachment (default: the current epoch).
    pub epoch: Option<i64>,
    /// Save the file to this path instead of returning it inline (not available over HTTP; refuses to overwrite).
    pub save_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxArgs {
    /// Only messages with a sequence number greater than this cursor (0 = everything unacked).
    #[serde(default)]
    pub after: i64,
    /// Maximum number of messages to return (1 to 200).
    #[serde(default = "default_read_limit")]
    pub limit: i64,
}

impl Default for InboxArgs {
    fn default() -> Self {
        Self {
            after: 0,
            limit: default_read_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxWaitArgs {
    /// Cursor from your previous call (0 on the first call); only newer messages are returned.
    #[serde(default)]
    pub cursor: i64,
    /// Seconds to wait for a message when none is waiting (max 55).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum number of messages to return (1 to 200).
    #[serde(default = "default_inbox_wait_limit")]
    pub limit: i64,
}

impl Default for InboxWaitArgs {
    fn default() -> Self {
        Self {
            cursor: 0,
            timeout_secs: default_timeout_secs(),
            limit: default_inbox_wait_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AckArgs {
    /// Channel id.
    pub channel: String,
    /// Mark the channel read up to and including this sequence number.
    pub seq: i64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelCreateArgs {
    /// New channel id.
    pub id: String,
    /// Channel kind: "dm", "group", "thread", "squad" or "team".
    #[serde(default = "default_kind_group")]
    pub kind: String,
    /// Display name of the channel.
    #[serde(default)]
    pub name: String,
    /// Id of the parent channel (for threads and squads).
    pub parent: Option<String>,
    /// Members as a 64-character hex public key, or "PK:admin" to make the member an admin. You are always a member.
    pub members: Option<Vec<String>>,
    /// Create an end-to-end encrypted channel.
    #[serde(default)]
    pub encrypt: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelAddArgs {
    /// Channel id.
    pub channel: String,
    /// Public key of the new member (64 lowercase hex characters).
    pub pk: String,
    /// Add the member as a channel admin.
    #[serde(default)]
    pub admin: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelRemoveArgs {
    /// Channel id.
    pub channel: String,
    /// Public key of the member to remove (64 lowercase hex characters). Encrypted channels get a new key.
    pub pk: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelRotateArgs {
    /// Id of an encrypted channel to give a new key epoch for all current members.
    pub channel: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelPromoteArgs {
    /// Channel id.
    pub channel: String,
    /// Public key of the member to make an admin (64 lowercase hex characters).
    pub pk: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelDemoteArgs {
    /// Channel id.
    pub channel: String,
    /// Public key of the admin to make a plain member (64 lowercase hex characters).
    pub pk: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleGrantArgs {
    /// Public key to grant the role to (64 lowercase hex characters).
    pub pk: String,
    /// Role: "observer", "worker", "supervisor", "controller" or "owner".
    pub role: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeArgs {
    /// Public key of the identity to revoke (64 lowercase hex characters).
    pub pk: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardCreateArgs {
    /// New card id.
    pub id: String,
    /// Card title.
    pub title: String,
    /// Lane (workstream) the card belongs to.
    pub lane: Option<String>,
    /// Priority: higher = more urgent.
    pub priority: Option<i64>,
    /// Estimated value of the card.
    pub value: Option<f64>,
    /// Estimated size of the card.
    pub size: Option<f64>,
    /// Team that works the card (for per-lane WIP limits).
    pub team: Option<String>,
    /// Ids of cards this one waits for.
    #[serde(default)]
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardMoveArgs {
    /// Card id.
    pub id: String,
    /// New state: backlog, ready, claimed, doing, review, done, blocked.
    pub state: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardEvidenceArgs {
    /// Card id.
    pub id: String,
    /// Proof of the work: a commit hash, test output summary, URL or file hash.
    pub reference: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardApproveArgs {
    /// Id of a card in review to approve.
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardReclaimArgs {
    /// Id of a stuck card to take back from its current owner.
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardListArgs {
    /// Only cards in this state.
    pub state: Option<String>,
    /// Only cards in this lane.
    pub lane: Option<String>,
    /// Maximum number of cards to return (1 to 200).
    #[serde(default = "default_card_limit")]
    pub limit: i64,
}

impl Default for CardListArgs {
    fn default() -> Self {
        Self {
            state: None,
            lane: None,
            limit: default_card_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SprintCreateArgs {
    /// New sprint id.
    pub id: String,
    /// Sprint name.
    pub name: String,
    /// What the sprint should achieve.
    #[serde(default)]
    pub goal: String,
    /// Start of the sprint, milliseconds since the Unix epoch.
    pub starts: i64,
    /// End of the sprint, milliseconds since the Unix epoch (after `starts`).
    pub ends: i64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SprintAddArgs {
    /// Sprint id.
    pub sprint: String,
    /// Id of the card to add to the sprint.
    pub card: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SprintStartArgs {
    /// Sprint id.
    pub sprint: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SprintCloseArgs {
    /// Sprint id.
    pub sprint: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SprintSummaryArgs {
    /// Sprint id.
    pub sprint: String,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardSummaryArgs {
    /// Only cards in this lane.
    pub lane: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotePublishArgs {
    /// Note id.
    pub id: String,
    /// Note title.
    pub title: String,
    /// Note body. Never put secrets here.
    pub body: String,
    /// Space-separated tags.
    #[serde(default)]
    pub tags: String,
    /// Id of an older note this one replaces.
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteSearchArgs {
    /// Full-text query.
    pub query: String,
    /// Maximum number of notes to return (1 to 200).
    #[serde(default = "default_note_limit")]
    pub limit: i64,
}

/// No arguments.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryStatusArgs {}

#[cfg(test)]
mod tests {
    use super::super::work_args::*;
    use super::*;
    use serde_json::{Value, json};

    fn schema<T: JsonSchema>() -> Value {
        serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes")
    }

    macro_rules! assert_object_schemas {
        ($($t:ty),+ $(,)?) => {
            $(
                let s = schema::<$t>();
                assert_eq!(s["type"], "object", "{}: {s}", stringify!($t));
            )+
        };
    }

    #[test]
    fn every_args_type_has_an_object_schema() {
        assert_object_schemas!(
            WhoamiArgs,
            DiscoverArgs,
            AttachmentArg,
            ChatArgs,
            AttachArgs,
            ReadArgs,
            ReactArgs,
            FetchFileArgs,
            InboxArgs,
            InboxWaitArgs,
            AckArgs,
            ChannelCreateArgs,
            ChannelAddArgs,
            ChannelRemoveArgs,
            ChannelRotateArgs,
            ChannelPromoteArgs,
            ChannelDemoteArgs,
            RoleGrantArgs,
            RevokeArgs,
            CardCreateArgs,
            CardMoveArgs,
            CardEvidenceArgs,
            CardApproveArgs,
            CardReclaimArgs,
            CardListArgs,
            SprintCreateArgs,
            SprintAddArgs,
            SprintStartArgs,
            SprintCloseArgs,
            SprintSummaryArgs,
            BoardSummaryArgs,
            NotePublishArgs,
            NoteSearchArgs,
            DeliveryStatusArgs,
            WorkScopeArgs,
            WorkAddArgs,
            WorkShowArgs,
            WorkNextArgs,
            WorkClaimArgs,
            WorkAcceptArgs,
            WorkRunArgs,
            WorkWaitArgs,
            WorkStatusArgs,
            WorkCancelArgs,
            WorkFinishArgs,
            WorkScheduleArgs,
            WorkSettingsArgs,
            WorkPlanArgs,
            PoolDefineArgs,
            PoolLeaseArgs,
            PoolReleaseArgs,
            QuietWindowArgs,
            WatchdogReportArgs,
            ReclaimArgs,
            EscalateArgs,
            PolicySetArgs,
            MetricsArgs,
            CardUpdateArgs,
        );
    }

    #[test]
    fn defaults_apply_and_unknown_fields_are_refused() {
        let a: InboxWaitArgs = serde_json::from_value(json!({})).expect("all defaults");
        assert_eq!((a.cursor, a.timeout_secs, a.limit), (0, 55, 20));
        let a: ChannelCreateArgs = serde_json::from_value(json!({"id": "c"})).expect("defaults");
        assert_eq!(
            (a.kind.as_str(), a.name.as_str(), a.encrypt),
            ("group", "", false)
        );
        let d: DiscoverArgs = serde_json::from_value(json!({})).expect("defaults");
        assert!(d.include_members);
        assert!(
            serde_json::from_value::<AckArgs>(json!({"channel": "c", "seq": 1, "extra": 1}))
                .is_err()
        );
    }

    #[test]
    fn fields_carry_descriptions() {
        let s = schema::<InboxWaitArgs>();
        assert!(
            s["properties"]["timeout_secs"]["description"].is_string(),
            "{s}"
        );
    }
}
