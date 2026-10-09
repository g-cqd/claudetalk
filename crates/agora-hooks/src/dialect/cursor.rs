//! Cursor dialect (DOCS-ONLY, cursor.com/docs/hooks; SPEC 2.6, 3.4). Cursor is not installed on
//! the machine that wrote this module, so the payloads in `tests/fixtures/cursor/` are docs-derived
//! (see the README there), not captured.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::is_inbox_tool;
use crate::schema::{HookEvent, HookInput};
use crate::{HookError, HookKind};

use super::{ClaudeOut, mcp_tool_name};

/// Cursor's stop-hook `loop_limit` (`hooks.json`, SPEC 2.6). Cursor drops a follow-up sent past
/// it, so `stop` declines from this `loop_count` on.
pub const LOOP_LIMIT: i64 = 5;
/// The MCP server whose `inbox` tool is the agent's own inbox read.
const AGORA_SERVER: &str = "agora";

#[cfg(test)]
mod tests;

/// The Cursor hook stdin object (snake_case). Only the fields `parse` reads are declared; unknown
/// fields are ignored.
///
/// Each field is held as a raw `Value` and extracted by type, so a null or wrong-typed field reads
/// as absent and never makes the whole payload an error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CursorHookInput {
    pub conversation_id: Option<Value>,
    pub session_id: Option<Value>,
    /// camelCase native event, e.g. `sessionStart`, `afterMCPExecution`.
    pub hook_event_name: Option<Value>,
    pub workspace_roots: Option<Value>,
    pub transcript_path: Option<Value>,
    /// postToolUse: the built-in tool name. afterMCPExecution: the MCP tool name (`inbox`).
    pub tool_name: Option<Value>,
    /// afterMCPExecution: the MCP server name (`agora`).
    pub mcp_server_name: Option<Value>,
    /// postToolUse: the tool's output.
    pub tool_output: Option<Value>,
    /// afterMCPExecution: the MCP result, normally a JSON string.
    pub result_json: Option<Value>,
    /// stop: the follow-ups Cursor has already sent in this conversation.
    pub loop_count: Option<Value>,
}

/// Map the snake_case Cursor payload to the Claude-shaped `HookInput`.
///
/// The native event is the payload's `hook_event_name`, else the `event` argument. The session is
/// `conversation_id` (else `session_id`); `cwd` is `workspace_roots[0]`. Map: sessionStart ->
/// SessionStart; postToolUse -> PostToolUse (`tool_output` as the response); afterMCPExecution ->
/// PostToolUse only when the server is `agora` and `mcp__agora__<tool>` is an inbox tool
/// (`result_json` parsed, kept as a string when it is not JSON), else `cursor:afterMCPExecution`;
/// stop -> Stop, with `stop_hook_active` set once `loop_count` reaches `LOOP_LIMIT` (see the match);
/// any other event, including subagentStop ->
/// `cursor:<native>`, which the entry points ignore. Sets `origin = "cursor"`.
pub(crate) fn parse(event: Option<&str>, v: Value) -> Result<HookInput, HookError> {
    if !v.is_object() {
        return Err(HookError::Input(
            "cursor payload is not a JSON object".into(),
        ));
    }
    let raw: CursorHookInput =
        serde_json::from_value(v).map_err(|e| HookError::Input(format!("cursor payload: {e}")))?;
    let native = non_empty(&raw.hook_event_name)
        .or_else(|| event.map(str::to_owned))
        .unwrap_or_default();
    let session_id = non_empty(&raw.conversation_id)
        .or_else(|| text(&raw.session_id))
        .unwrap_or_default();
    let mut input = HookInput {
        session_id,
        transcript_path: text(&raw.transcript_path),
        cwd: first_root(&raw.workspace_roots),
        origin: Some("cursor".to_owned()),
        ..HookInput::default()
    };
    match native.as_str() {
        "sessionStart" => input.hook_event_name = "SessionStart".to_owned(),
        "postToolUse" => {
            input.hook_event_name = "PostToolUse".to_owned();
            input.tool_name = text(&raw.tool_name);
            input.tool_response = raw.tool_output;
        }
        "afterMCPExecution" => {
            // Another installed server may also expose an `inbox` tool; only agora's own counts.
            let from_agora = text(&raw.mcp_server_name).as_deref() == Some(AGORA_SERVER);
            let tool = mcp_tool(&raw.mcp_server_name, &raw.tool_name)
                .filter(|name| from_agora && is_inbox_tool(name));
            if let Some(name) = tool {
                input.hook_event_name = "PostToolUse".to_owned();
                input.tool_name = Some(name);
                input.tool_response = raw.result_json.as_ref().map(result_value);
            } else {
                input.hook_event_name = format!("cursor:{native}");
            }
        }
        // Cursor ignores a follow-up sent once loop_count reaches its loop_limit (LOOP_LIMIT). So
        // from there on the Stop is declined as already active: `stop::run` emits nothing and does
        // not advance the cursor, and the unread messages are delivered later, not lost.
        "stop" => {
            input.hook_event_name = "Stop".to_owned();
            let loops = raw.loop_count.as_ref().and_then(Value::as_i64).unwrap_or(0);
            input.stop_hook_active = loops >= LOOP_LIMIT;
        }
        other => input.hook_event_name = format!("cursor:{other}"),
    }
    Ok(input)
}

/// context -> `{"additional_context":T}` (SessionStart, PostToolUse); block -> `{"followup_message":R}`
/// (Stop). "" = silent.
///
/// The output shape is chosen by the event the entry point ran for (`input.hook_event_name`). A
/// Stop with context, or a context event with a block, renders silence.
pub(crate) fn render(_kind: HookKind, input: &HookInput, out: &ClaudeOut) -> String {
    let event = HookEvent::parse(&input.hook_event_name);
    if let Some(reason) = out.block_reason.as_deref()
        && event == HookEvent::Stop
    {
        return json!({"followup_message": reason}).to_string();
    }
    if let Some(text) = out.context.as_deref()
        && matches!(event, HookEvent::SessionStart | HookEvent::PostToolUse)
    {
        return json!({"additional_context": text}).to_string();
    }
    String::new()
}

/// The string a field holds; `None` for an absent, null or non-string field.
fn text(field: &Option<Value>) -> Option<String> {
    field.as_ref().and_then(Value::as_str).map(str::to_owned)
}

/// `text`, with an empty string counted as absent.
fn non_empty(field: &Option<Value>) -> Option<String> {
    text(field).filter(|s| !s.is_empty())
}

/// The first `workspace_roots` entry, when the field is an array whose first element is a string.
fn first_root(field: &Option<Value>) -> Option<String> {
    field
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|roots| roots.first())
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `mcp__<server>__<tool>` when both names are non-empty; `None` otherwise. An incomplete payload
/// therefore never names an MCP tool, so it can never look like an inbox read.
fn mcp_tool(server: &Option<Value>, tool: &Option<Value>) -> Option<String> {
    let server = non_empty(server)?;
    let tool = non_empty(tool)?;
    Some(mcp_tool_name(&server, &tool))
}

/// The `result_json` string parsed as JSON; the string itself when it is not JSON. A non-string
/// value is taken as the result as it is.
fn result_value(field: &Value) -> Value {
    match field {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| field.clone()),
        other => other.clone(),
    }
}
