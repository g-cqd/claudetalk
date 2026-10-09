//! Antigravity (`agy`) dialect (DOCS-ONLY, antigravity.google/docs/hooks; SPEC 2.4 and 3.4).
//!
//! `agy` is not installed locally, so nothing here is VERIFIED-LOCAL. The fixtures in
//! `tests/fixtures/agy/` are docs-derived (see the README there). The payload carries no reliable
//! event name, so the event comes from `--event` (the installed hook command passes it), with the
//! payload's `hookEventName` as the fallback.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::schema::HookInput;
use crate::{HookError, HookKind};

use super::ClaudeOut;

#[cfg(test)]
mod tests;

/// The agy hook stdin object (camelCase, DOCS-ONLY). Every field is optional; unknown fields are
/// ignored.
///
/// Only the fields the parser reads are kept, and each is held as a raw `Value` and extracted
/// leniently: a `null` or wrong-typed value yields no data for that field and never fails the
/// whole payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct AgyHookInput {
    /// Read as the session id; a non-string is treated as absent.
    pub conversation_id: Option<Value>,
    /// Read as `workspacePaths[0]` -> cwd; anything but a non-empty array of strings is absent.
    pub workspace_paths: Option<Value>,
    pub transcript_path: Option<Value>,
    /// Not reliably sent by agy; used only when no `--event` arrives.
    pub hook_event_name: Option<Value>,
    /// PreToolUse / PostToolUse: the tool call; only `name` is read (as `tool_name`).
    pub tool_call: Option<Value>,
}

impl AgyHookInput {
    /// The session id, or "" when absent or not a string.
    fn session_id(&self) -> String {
        text(self.conversation_id.as_ref()).unwrap_or_default()
    }

    /// The first workspace path, when it is a non-empty string.
    fn cwd(&self) -> Option<String> {
        self.workspace_paths
            .as_ref()?
            .as_array()?
            .first()?
            .as_str()
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
    }

    /// The tool name of `toolCall`, when present and non-empty.
    fn tool_name(&self) -> Option<String> {
        self.tool_call
            .as_ref()?
            .get("name")?
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    }
}

/// A string field as owned text; `None` for a missing, `null` or non-string value.
fn text(v: Option<&Value>) -> Option<String> {
    v?.as_str().map(str::to_owned)
}

/// Camel-case agy payload: event REQUIRED from `event` (else payload `hookEventName`, else ignored); `conversationId` -> session_id; `workspacePaths[0]` -> cwd; `transcriptPath`; `toolCall.name` -> tool_name. PreInvocation and PostInvocation map to "PostToolBatch", Stop to "Stop" (stop_hook_active false), everything else is ignored. Sets `origin = "agy"`. Null or wrong-typed fields are treated as absent.
pub(crate) fn parse(event: Option<&str>, v: Value) -> Result<HookInput, HookError> {
    let raw: AgyHookInput =
        serde_json::from_value(v).map_err(|e| HookError::Input(format!("agy payload: {e}")))?;
    let payload_event = text(raw.hook_event_name.as_ref());
    let native = event
        .filter(|e| !e.is_empty())
        .or(payload_event.as_deref())
        .unwrap_or_default();
    Ok(HookInput {
        session_id: raw.session_id(),
        transcript_path: text(raw.transcript_path.as_ref()),
        cwd: raw.cwd(),
        hook_event_name: map_event(native),
        tool_name: raw.tool_name(),
        origin: Some("agy".to_owned()),
        ..HookInput::default()
    })
}

/// The Claude event an agy event stands for. PreInvocation and PostInvocation become
/// "PostToolBatch" (context), Stop becomes "Stop". Every other agy event keeps its native name
/// behind `agy:`, which no entry point recognises, so it runs nothing and the cursor stays put.
/// An absent event stays empty.
fn map_event(native: &str) -> String {
    match native {
        "" => String::new(),
        "PreInvocation" | "PostInvocation" => "PostToolBatch".to_owned(),
        "Stop" => "Stop".to_owned(),
        other => format!("agy:{other}"),
    }
}

/// context -> `{"injectSteps":[{"ephemeralMessage":T}]}`; block -> `{"decision":"continue","reason":R}`. "" = silent.
///
/// The shape follows `input.hook_event_name`: "PostToolBatch" carries context, "Stop" carries a
/// block. Any other event is silent, as is a context or block that is absent or empty.
pub(crate) fn render(_kind: HookKind, input: &HookInput, out: &ClaudeOut) -> String {
    match input.hook_event_name.as_str() {
        "PostToolBatch" => match out.context.as_deref().filter(|t| !t.is_empty()) {
            Some(text) => json!({"injectSteps": [{"ephemeralMessage": text}]}).to_string(),
            None => String::new(),
        },
        "Stop" => match out.block_reason.as_deref().filter(|r| !r.is_empty()) {
            Some(reason) => json!({"decision": "continue", "reason": reason}).to_string(),
            None => String::new(),
        },
        _ => String::new(),
    }
}
