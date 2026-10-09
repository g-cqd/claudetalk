//! Copilot dialect: the SHARED Copilot hooks file is run by BOTH Copilot CLI and VS Code, so the
//! producer is detected per invocation (SPEC 3.4).
//!
//! Copilot CLI 0.0.369 (VERIFIED-LOCAL) sends camelCase payloads with NO session id and ignores
//! all hook output. Such a payload maps to `copilot:<native>`, which nothing runs for (SPEC 2.2,
//! LEGACY RULE). Newer CLIs (DOCS-ONLY, Copilot CLI >= 1.0.72 for `stop_hook_active`) send
//! `sessionId` and honor `{"additionalContext":T}` and `{"decision":"block","reason":R}`.

use serde::Deserialize;
use serde_json::Value;

use crate::schema::{HookEvent, HookInput, block_json};
use crate::{HookError, HookKind};

use super::{ClaudeOut, mcp_tool_name, vscode};

#[cfg(test)]
mod tests;

/// The Copilot CLI stdin object (camelCase). Every field is optional and unknown fields are
/// ignored. Only the fields Agora reads are declared; each is kept as a raw `Value` so a null or
/// wrong-typed field reads as absent instead of silencing the whole payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct CopilotCliInput {
    /// Missing on 0.0.369; present on newer CLIs (DOCS-ONLY).
    pub session_id: Value,
    pub cwd: Value,
    pub transcript_path: Value,
    /// sessionStart: `new` on 0.0.369; `startup`, `resume` or `new` in the docs.
    pub source: Value,
    /// postToolUse: `<server>-<tool>`, e.g. `agora-inbox`.
    pub tool_name: Value,
    /// postToolUse: `{"resultType":..,"textResultForLlm":..}`, kept as the JSON value it arrived as.
    pub tool_result: Value,
    /// Stop: the docs keep this snake_case even in the camelCase format; `stopHookActive` is
    /// accepted too.
    #[serde(alias = "stopHookActive", rename = "stop_hook_active")]
    pub stop_hook_active: Value,
}

/// A JSON string as `Some`; any other JSON value (including null) as `None`.
fn text(v: Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// Copilot CLI's `<server>-<tool>` form of an MCP tool becomes `mcp__agora__<tool>` for `agora`
/// (the form `context::is_inbox_tool` understands). Any other name, or `agora-` with no tool, is
/// kept as the CLI sent it.
fn normalize_tool_name(name: String) -> String {
    let agora_tool = name
        .strip_prefix("agora-")
        .filter(|tool| !tool.is_empty())
        .map(|tool| mcp_tool_name("agora", tool));
    agora_tool.unwrap_or(name)
}

/// A string `hook_event_name` in the payload means VS Code: delegate to `vscode::parse` (origin
/// `"vscode"`). Otherwise it is a Copilot CLI payload (origin `"copilot-cli"`).
///
/// For a CLI payload the native event comes from `event`. With no non-empty `sessionId` (0.0.369),
/// or with an event Agora does not map, the name is `copilot:<native>`, which nothing runs for.
/// `sessionStart`, `postToolUse`, `agentStop` and `subagentStop` map to their Claude events.
pub(crate) fn parse(event: Option<&str>, v: Value) -> Result<HookInput, HookError> {
    if v.get("hook_event_name").is_some_and(Value::is_string) {
        return vscode::parse(event, v);
    }
    let raw: CopilotCliInput =
        serde_json::from_value(v).map_err(|e| HookError::Input(format!("copilot payload: {e}")))?;
    let native = event.unwrap_or_default();
    let session_id = text(raw.session_id).unwrap_or_default();
    let hook_event_name = if session_id.is_empty() {
        format!("copilot:{native}")
    } else {
        match native {
            "sessionStart" => HookEvent::SessionStart.as_str().to_owned(),
            "postToolUse" => HookEvent::PostToolUse.as_str().to_owned(),
            "agentStop" => HookEvent::Stop.as_str().to_owned(),
            "subagentStop" => HookEvent::SubagentStop.as_str().to_owned(),
            other => format!("copilot:{other}"),
        }
    };
    Ok(HookInput {
        session_id,
        transcript_path: text(raw.transcript_path),
        cwd: text(raw.cwd),
        hook_event_name,
        source: text(raw.source),
        tool_name: text(raw.tool_name).map(normalize_tool_name),
        tool_response: Some(raw.tool_result).filter(|v| !v.is_null()),
        stop_hook_active: raw.stop_hook_active.as_bool().unwrap_or(false),
        origin: Some("copilot-cli".to_owned()),
        ..HookInput::default()
    })
}

/// `input.origin == Some("vscode")` -> `vscode::render`. Otherwise the Copilot CLI shape: context
/// -> `{"additionalContext":T}` on SessionStart and PostToolUse; block -> `{"decision":"block",
/// "reason":R}` on Stop. Anything else is silent (`""`).
pub(crate) fn render(kind: HookKind, input: &HookInput, out: &ClaudeOut) -> String {
    if input.origin.as_deref() == Some("vscode") {
        return vscode::render(kind, input, out);
    }
    let event = HookEvent::parse(&input.hook_event_name);
    if let Some(reason) = out.block_reason.as_deref()
        && event == HookEvent::Stop
    {
        return block_json(reason);
    }
    if let Some(text) = out.context.as_deref()
        && matches!(event, HookEvent::SessionStart | HookEvent::PostToolUse)
    {
        return serde_json::json!({ "additionalContext": text }).to_string();
    }
    String::new()
}
