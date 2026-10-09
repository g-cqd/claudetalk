//! The MCP tool table and its dispatch.
//!
//! Handler signature for every tool: `pub async fn <tool>(cx: &ToolCx<'_>, a: <Camel>Args) ->
//! ToolResult` (in `chat`, `org`, `board`, `diag`, `work`, `sched`). Argument types live in `args` and, for the
//! work-queue and scheduling tools, in `work_args`.
//! Errors: `ClientError` -> `ToolError{code: e.code(), message: e.to_string()}`; usage
//! problems -> code "usage"; undecodable arguments -> code "invalid_arguments" (a tool result
//! with isError, so the model can self-correct).

pub mod args;
pub mod board;
pub mod chat;
pub mod diag;
pub mod org;
pub mod sched;
mod table_board;
mod table_chat;
mod table_sched;
mod table_work;
pub mod work;
pub mod work_args;

use crate::engine::{Engine, Notifier, Peer};
use crate::framing::{UNTRUSTED_DATA_NOTICE, frame_text};
use agora_client::{ClientError, Out};
use args::*;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock};
use tokio_util::sync::CancellationToken;
use work_args::*;

/// Everything a tool handler needs.
pub struct ToolCx<'a> {
    pub engine: &'a Arc<Engine>,
    pub peer: &'a Arc<Peer>,
    /// Some iff the request carried a `_meta.progressToken`.
    pub progress: Option<Progress>,
    /// Cancelled when the request is cancelled or the peer goes away; long tools stop waiting.
    pub cancel: CancellationToken,
}

/// Progress reporting for one request.
#[derive(Clone)]
pub struct Progress {
    pub token: Value,
    pub notifier: Notifier,
}

impl Progress {
    /// Send `notifications/progress` `{"progressToken":token,"progress":progress,"total":total
    /// (omitted when None),"message":message}`. Callers pass strictly increasing `progress`.
    /// Best effort: a full or closed queue drops the notification.
    pub fn report(&self, progress: f64, total: Option<f64>, message: &str) {
        let mut params = serde_json::json!({
            "progressToken": self.token,
            "progress": progress,
            "message": message,
        });
        if let (Some(total), Some(obj)) = (total, params.as_object_mut()) {
            obj.insert("total".into(), Value::from(total));
        }
        self.notifier.send("notifications/progress", params);
    }
}

/// A successful tool call: `structured` is always a JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub structured: Value,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    /// `ClientError::code()` or "usage" / "invalid_arguments".
    pub code: String,
    pub message: String,
}

impl ToolError {
    /// A "usage" error.
    pub fn usage(msg: impl Into<String>) -> Self {
        Self {
            code: "usage".into(),
            message: msg.into(),
        }
    }
}

impl From<ClientError> for ToolError {
    fn from(e: ClientError) -> Self {
        Self {
            code: e.code(),
            message: e.to_string(),
        }
    }
}

pub type ToolResult = Result<ToolOutput, ToolError>;

/// The api result as a tool result: its JSON is `structured`, its text is kept as is.
pub(crate) fn output(out: Out) -> ToolOutput {
    ToolOutput {
        structured: out.json,
        text: out.text,
    }
}

/// The api result as a tool result whose text echoes other agents' titles: the text is framed
/// as untrusted data, and the JSON gets the same `notice` field when it is an object.
pub(crate) fn framed_output(out: Out) -> ToolOutput {
    let mut structured = out.json;
    if let Value::Object(map) = &mut structured {
        map.insert(
            "notice".to_owned(),
            Value::String(UNTRUSTED_DATA_NOTICE.to_owned()),
        );
    }
    ToolOutput {
        structured,
        text: frame_text(&out.text),
    }
}

/// One row of the tool table.
pub struct ToolMeta {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// readOnlyHint
    pub read_only: bool,
    /// destructiveHint
    pub destructive: bool,
    /// idempotentHint
    pub idempotent: bool,
    /// inputSchema of the tool (`schema_of::<Args>()`).
    pub schema: fn() -> Value,
}

/// The tools in table order (the order of the SPEC tool table): whoami, discover, chat, attach,
/// read, react, fetch_file, inbox, inbox_wait, ack, channel_*, role_grant, revoke, card_*,
/// sprint_*, board_summary, note_*, delivery_status, then the work-queue and scheduling tools (work_scope ...
/// card_update). Annotations per the SPEC table.
static TABLE: LazyLock<Vec<&'static ToolMeta>> = LazyLock::new(|| {
    [
        table_chat::ROWS,
        table_board::ROWS,
        table_work::ROWS,
        table_sched::ROWS,
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// The tool table in SPEC order, with annotations per the SPEC table.
pub fn table() -> &'static [&'static ToolMeta] {
    TABLE.as_slice()
}

/// `{"tools":[...]}` in table order; each `{"name","title","description","inputSchema",
/// "annotations":{"title","readOnlyHint","destructiveHint","idempotentHint","openWorldHint":false}}`.
pub fn list_json() -> Value {
    let tools: Vec<Value> = table()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": {
                    "title": t.title,
                    "readOnlyHint": t.read_only,
                    "destructiveHint": t.destructive,
                    "idempotentHint": t.idempotent,
                    "openWorldHint": false,
                },
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `serde_json::to_value(schemars::schema_for!(T))` with the top-level "$schema" key removed;
/// always `"type":"object"` for the Args types.
pub fn schema_of<T: JsonSchema>() -> Value {
    let mut obj = match serde_json::to_value(schemars::schema_for!(T)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    obj.remove("$schema");
    obj.entry("type").or_insert_with(|| Value::from("object"));
    obj.entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    Value::Object(obj)
}

/// Decode tool arguments (`Value::Null` is treated as `{}`); failure -> `ToolError` with code
/// "invalid_arguments" and the serde message.
pub fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() {
        Value::Object(Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|e| ToolError {
        code: "invalid_arguments".into(),
        message: format!("invalid arguments: {e}"),
    })
}

/// Decode the arguments of one tool and run its handler.
macro_rules! route {
    ($cx:ident, $args:ident, $handler:path, $args_ty:ty) => {
        match parse_args::<$args_ty>($args) {
            Ok(a) => $handler($cx, a).await,
            Err(e) => Err(e),
        }
    };
}

async fn dispatch(cx: &ToolCx<'_>, name: &str, args: Value) -> ToolResult {
    match name {
        "whoami" => route!(cx, args, diag::whoami, WhoamiArgs),
        "discover" => route!(cx, args, diag::discover, DiscoverArgs),
        "chat" => route!(cx, args, chat::chat, ChatArgs),
        "attach" => route!(cx, args, chat::attach, AttachArgs),
        "read" => route!(cx, args, chat::read, ReadArgs),
        "react" => route!(cx, args, chat::react, ReactArgs),
        "fetch_file" => route!(cx, args, chat::fetch_file, FetchFileArgs),
        "inbox" => route!(cx, args, chat::inbox, InboxArgs),
        "inbox_wait" => route!(cx, args, chat::inbox_wait, InboxWaitArgs),
        "ack" => route!(cx, args, chat::ack, AckArgs),
        "channel_create" => route!(cx, args, org::channel_create, ChannelCreateArgs),
        "channel_add" => route!(cx, args, org::channel_add, ChannelAddArgs),
        "channel_remove" => route!(cx, args, org::channel_remove, ChannelRemoveArgs),
        "channel_rotate" => route!(cx, args, org::channel_rotate, ChannelRotateArgs),
        "channel_promote" => route!(cx, args, org::channel_promote, ChannelPromoteArgs),
        "channel_demote" => route!(cx, args, org::channel_demote, ChannelDemoteArgs),
        "role_grant" => route!(cx, args, org::role_grant, RoleGrantArgs),
        "revoke" => route!(cx, args, org::revoke, RevokeArgs),
        "card_create" => route!(cx, args, board::card_create, CardCreateArgs),
        "card_move" => route!(cx, args, board::card_move, CardMoveArgs),
        "card_evidence" => route!(cx, args, board::card_evidence, CardEvidenceArgs),
        "card_approve" => route!(cx, args, board::card_approve, CardApproveArgs),
        "card_reclaim" => route!(cx, args, board::card_reclaim, CardReclaimArgs),
        "card_list" => route!(cx, args, board::card_list, CardListArgs),
        "sprint_create" => route!(cx, args, board::sprint_create, SprintCreateArgs),
        "sprint_add" => route!(cx, args, board::sprint_add, SprintAddArgs),
        "sprint_start" => route!(cx, args, board::sprint_start, SprintStartArgs),
        "sprint_close" => route!(cx, args, board::sprint_close, SprintCloseArgs),
        "sprint_summary" => route!(cx, args, board::sprint_summary, SprintSummaryArgs),
        "board_summary" => route!(cx, args, board::board_summary, BoardSummaryArgs),
        "note_publish" => route!(cx, args, board::note_publish, NotePublishArgs),
        "note_search" => route!(cx, args, board::note_search, NoteSearchArgs),
        "delivery_status" => route!(cx, args, diag::delivery_status, DeliveryStatusArgs),
        "work_scope" => route!(cx, args, work::work_scope, WorkScopeArgs),
        "work_add" => route!(cx, args, work::work_add, WorkAddArgs),
        "work_show" => route!(cx, args, work::work_show, WorkShowArgs),
        "work_next" => route!(cx, args, work::work_next, WorkNextArgs),
        "work_claim" => route!(cx, args, work::work_claim, WorkClaimArgs),
        "work_accept" => route!(cx, args, work::work_accept, WorkAcceptArgs),
        "work_run" => route!(cx, args, work::work_run, WorkRunArgs),
        "work_wait" => route!(cx, args, work::work_wait, WorkWaitArgs),
        "work_status" => route!(cx, args, work::work_status, WorkStatusArgs),
        "work_cancel" => route!(cx, args, work::work_cancel, WorkCancelArgs),
        "work_finish" => route!(cx, args, work::work_finish, WorkFinishArgs),
        "work_schedule" => route!(cx, args, sched::work_schedule, WorkScheduleArgs),
        "work_settings" => route!(cx, args, work::work_settings, WorkSettingsArgs),
        "work_plan" => route!(cx, args, work::work_plan, WorkPlanArgs),
        "pool_define" => route!(cx, args, sched::pool_define, PoolDefineArgs),
        "pool_lease" => route!(cx, args, sched::pool_lease, PoolLeaseArgs),
        "pool_release" => route!(cx, args, sched::pool_release, PoolReleaseArgs),
        "quiet_window" => route!(cx, args, sched::quiet_window, QuietWindowArgs),
        "watchdog_report" => route!(cx, args, sched::watchdog_report, WatchdogReportArgs),
        "reclaim" => route!(cx, args, sched::reclaim, ReclaimArgs),
        "escalate" => route!(cx, args, sched::escalate, EscalateArgs),
        "policy_set" => route!(cx, args, sched::policy_set, PolicySetArgs),
        "metrics" => route!(cx, args, sched::metrics, MetricsArgs),
        "card_update" => route!(cx, args, sched::card_update, CardUpdateArgs),
        _ => Err(ToolError::usage(format!("unknown tool: {name}"))),
    }
}

/// Run tool `name`: `None` = unknown tool. Otherwise decode `args` (`parse_args`), call
/// `cx.peer.ensure_registered()` first (its error is the result), then the handler.
pub async fn call(cx: &ToolCx<'_>, name: &str, args: Value) -> Option<ToolResult> {
    if !table().iter().any(|t| t.name == name) {
        return None;
    }
    if let Err(e) = cx.peer.ensure_registered().await {
        return Some(Err(e));
    }
    Some(dispatch(cx, name, args).await)
}

/// The MCP `tools/call` result: success `{"content":[{"type":"text","text":...}],
/// "structuredContent":{...},"isError":false}`; error `{"content":[{"type":"text","text":
/// "error <code>: <message>"}],"structuredContent":{"error":{"code","message"}},"isError":true}`.
pub fn result_json(r: &ToolResult) -> Value {
    match r {
        Ok(out) => {
            let structured = match &out.structured {
                Value::Object(_) => out.structured.clone(),
                other => json!({ "value": other }),
            };
            json!({
                "content": [{ "type": "text", "text": out.text }],
                "structuredContent": structured,
                "isError": false,
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("error {}: {}", e.code, e.message) }],
            "structuredContent": { "error": { "code": e.code, "message": e.message } },
            "isError": true,
        }),
    }
}

#[cfg(test)]
mod tests;
