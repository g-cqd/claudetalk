//! VS Code agent-mode dialect (VS Code 1.141.0 "Local" hook schema, VERIFIED-LOCAL by bundle read; SPEC 3.4).
//!
//! Fixtures: `tests/fixtures/vscode/*.json` are DOCS-DERIVED, not captured (see the README there).
//! Hook output: `{"hookSpecificOutput":{"hookEventName":E,"additionalContext":T}}` on SessionStart,
//! UserPromptSubmit and PostToolUse; a Stop continues only with the `hookSpecificOutput` shape
//! that also carries `decision` and `reason` (SPEC 2.3).
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::schema::{HookEvent, HookInput, context_json};
use crate::{HookError, HookKind};

use super::ClaudeOut;

#[cfg(test)]
mod tests;

/// The VS Code "Local" hook stdin object: snake_case keys, PascalCase event names. Unknown fields
/// are ignored (serde), and so are the fields Agora never reads: `timestamp`, `model`, `prompt`,
/// `tool_input` and `tool_use_id` (SPEC 2.3).
///
/// Every field that is read is tolerant: a null or wrong-typed value reads as absent, so it never
/// fails the payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct VsCodeHookInput {
    /// PascalCase event name. Empty when absent or not a string.
    #[serde(deserialize_with = "lenient_string")]
    pub hook_event_name: String,
    /// Empty when absent or not a string; the central guard in `dialect` ignores an empty session.
    #[serde(deserialize_with = "lenient_string")]
    pub session_id: String,
    #[serde(deserialize_with = "lenient_opt")]
    pub transcript_path: Option<String>,
    #[serde(deserialize_with = "lenient_opt")]
    pub cwd: Option<String>,
    /// SessionStart: `new` (mapped to `startup`) or another value kept as-is.
    #[serde(deserialize_with = "lenient_opt")]
    pub source: Option<String>,
    #[serde(deserialize_with = "lenient_opt")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "lenient_opt")]
    pub agent_id: Option<String>,
    /// PostToolUse: the tool name as VS Code reports it.
    #[serde(deserialize_with = "lenient_opt")]
    pub tool_name: Option<String>,
    /// PostToolUse: a string or an object, kept as the JSON value it arrived as.
    pub tool_response: Option<Value>,
    /// Stop / SubagentStop: true when the agent is already continuing because of a Stop hook.
    #[serde(deserialize_with = "lenient_bool")]
    pub stop_hook_active: bool,
}

/// A string field: any other JSON value (null, a number, an object) reads as an empty string.
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(lenient_opt(d)?.unwrap_or_default())
}

/// An optional string field: any other JSON value (null, a number, an object) reads as absent.
fn lenient_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(str::to_owned))
}

/// A bool field: anything other than `true` reads as false.
fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)?.as_bool() == Some(true))
}

/// `source` value VS Code sends for a fresh session, mapped to the Claude value `startup`; any
/// other value passes through unchanged.
fn map_source(source: Option<String>) -> Option<String> {
    if source.as_deref() == Some("new") {
        Some("startup".to_owned())
    } else {
        source
    }
}

/// `mcp_agora_<tool>` (the name VS Code gives an Agora MCP tool) becomes `mcp__agora__<tool>`, the
/// form `context::is_inbox_tool` matches. Any other name, and `mcp_agora_` with no tool, is kept.
fn normalize_tool_name(name: Option<String>) -> Option<String> {
    if let Some(tool) = name
        .as_deref()
        .and_then(|n| n.strip_prefix("mcp_agora_"))
        .filter(|t| !t.is_empty())
    {
        return Some(super::mcp_tool_name("agora", tool));
    }
    name
}

/// Snake_case VS Code payload (`session_id`, `transcript_path`, `cwd`, `hook_event_name`, `source`, `tool_name`, `tool_response` string-or-object kept as a Value, `stop_hook_active`); `source:"new"` becomes "startup" and `mcp_agora_<tool>` becomes `mcp__agora__<tool>`. Sets `origin = "vscode"`. An empty `hook_event_name` takes `event`. A field that is null or has the wrong type reads as absent; only a payload that is not a JSON object is `HookError::Input`.
pub(crate) fn parse(event: Option<&str>, v: Value) -> Result<HookInput, HookError> {
    // serde reads a struct from a JSON array by position, so the object check is explicit.
    if !v.is_object() {
        return Err(HookError::Input(
            "vscode payload is not a JSON object".into(),
        ));
    }
    let raw: VsCodeHookInput =
        serde_json::from_value(v).map_err(|e| HookError::Input(format!("vscode payload: {e}")))?;
    let hook_event_name = if raw.hook_event_name.is_empty() {
        event.unwrap_or_default().to_owned()
    } else {
        raw.hook_event_name
    };
    Ok(HookInput {
        session_id: raw.session_id,
        transcript_path: raw.transcript_path,
        cwd: raw.cwd,
        hook_event_name,
        source: map_source(raw.source),
        tool_name: normalize_tool_name(raw.tool_name),
        tool_response: raw.tool_response,
        stop_hook_active: raw.stop_hook_active,
        agent_id: raw.agent_id,
        agent_type: raw.agent_type,
        origin: Some("vscode".to_owned()),
        ..HookInput::default()
    })
}

/// context -> `{"hookSpecificOutput":{"hookEventName":E,"additionalContext":T}}` for SessionStart/UserPromptSubmit/PostToolUse (E = the input event); block -> both shapes: `{"decision":"block","reason":R,"hookSpecificOutput":{"hookEventName":"Stop","decision":"block","reason":R}}`. "" = silent.
///
/// The shape is chosen by the event the entry point ran for (`input.hook_event_name`). A block
/// renders only for a Stop; a context renders only for the three events above. Anything else is
/// silent. A block takes precedence over a context on a Stop.
pub(crate) fn render(_kind: HookKind, input: &HookInput, out: &ClaudeOut) -> String {
    let event = HookEvent::parse(&input.hook_event_name);
    if let Some(reason) = out.block_reason.as_deref()
        && event == HookEvent::Stop
    {
        return serde_json::json!({
            "decision": "block",
            "reason": reason,
            "hookSpecificOutput": {
                "hookEventName": "Stop",
                "decision": "block",
                "reason": reason,
            },
        })
        .to_string();
    }
    if let Some(text) = out.context.as_deref()
        && matches!(
            event,
            HookEvent::SessionStart | HookEvent::UserPromptSubmit | HookEvent::PostToolUse
        )
    {
        return context_json(event.as_str(), text);
    }
    String::new()
}
