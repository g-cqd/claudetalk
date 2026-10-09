//! Codex dialect (Codex CLI 0.157.1, VERIFIED-LOCAL; SPEC 3.4): the payload is Claude-shaped.
//!
//! Captured payloads: `tests/fixtures/codex/*.json` (real, Codex CLI 0.157.1). Hook output:
//! `{"hookSpecificOutput":{"hookEventName":E,"additionalContext":T}}` reaches the model on
//! SessionStart, UserPromptSubmit and PostToolUse; `{"decision":"block","reason":R}` on Stop
//! continues the turn with R as a prompt (VERIFIED-LOCAL).

use serde::Deserialize;
use serde_json::Value;

use crate::schema::{HookEvent, HookInput, block_json, context_json};
use crate::{HookError, HookKind};

use super::ClaudeOut;

#[cfg(test)]
mod tests;

/// The Codex hook stdin object. Only the fields Agora reads are declared; unknown fields
/// (`turn_id`, `model`, `prompt`, `tool_input`, `tool_use_id`, `last_assistant_message`, `reason`,
/// ...) are ignored by serde (SPEC 3.4). Every read field is an `Option`, so a JSON `null` is
/// treated as absent instead of failing the whole payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CodexHookInput {
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub hook_event_name: Option<String>,
    pub permission_mode: Option<String>,
    /// SessionStart: `startup` (VERIFIED-LOCAL).
    pub source: Option<String>,
    /// PostToolUse / PermissionRequest / PreToolUse: `mcp__<server>__<tool>` for MCP, `Bash` etc.
    pub tool_name: Option<String>,
    /// PostToolUse: an object `{"content":[{"type":"text","text":..}]}` for MCP tools, a plain
    /// string for Bash.
    pub tool_response: Option<Value>,
    /// Stop: true when the turn is already continuing because of a Stop hook.
    pub stop_hook_active: Option<bool>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
}

/// Map the Claude-shaped Codex payload 1:1 (`session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`, `source`, `tool_name`, `tool_response`, `stop_hook_active`, `agent_id`, `agent_type`); every other key is ignored. `event` only fills an empty `hook_event_name`. Sets `origin = "codex"`.
pub(crate) fn parse(event: Option<&str>, v: Value) -> Result<HookInput, HookError> {
    let raw: CodexHookInput =
        serde_json::from_value(v).map_err(|e| HookError::Input(format!("codex payload: {e}")))?;
    let hook_event_name = match raw.hook_event_name {
        Some(name) if !name.is_empty() => name,
        _ => event.unwrap_or_default().to_owned(),
    };
    Ok(HookInput {
        session_id: raw.session_id.unwrap_or_default(),
        transcript_path: raw.transcript_path,
        cwd: raw.cwd,
        permission_mode: raw.permission_mode,
        hook_event_name,
        source: raw.source,
        tool_name: raw.tool_name,
        tool_response: raw.tool_response,
        stop_hook_active: raw.stop_hook_active.unwrap_or(false),
        agent_id: raw.agent_id,
        agent_type: raw.agent_type,
        origin: Some("codex".to_owned()),
        ..HookInput::default()
    })
}

/// context -> `{"hookSpecificOutput":{"hookEventName":<input.hook_event_name>,"additionalContext":T}}` for SessionStart/UserPromptSubmit/PostToolUse; block -> `{"decision":"block","reason":R}` for Stop. "" = silent.
///
/// The output shape is chosen by the event the entry point ran for (`input.hook_event_name`), not
/// by `kind`. A Stop with context, or a context event with a block, renders silence.
pub(crate) fn render(_kind: HookKind, input: &HookInput, out: &ClaudeOut) -> String {
    let event = HookEvent::parse(&input.hook_event_name);
    if let Some(reason) = out.block_reason.as_deref()
        && event == HookEvent::Stop
    {
        return block_json(reason);
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
