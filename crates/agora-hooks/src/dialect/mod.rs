//! Hook dialects: `agora hook <kind> --dialect <d> [--event <E>]`.
//!
//! The M4 entry points (`context`, `stop`, `session-start`) stay Claude-shaped and unchanged. A
//! dialect converts the native stdin into the Claude-shaped `HookInput` BEFORE them (`to_input`)
//! and converts their Claude-shaped stdout (`hookSpecificOutput.additionalContext` /
//! `{"decision":"block","reason"}`) into the native output AFTER them (`to_output`). Heartbeats,
//! cursor, block cap, framing and fail-open are all inherited.
//!
//! Event mapping rule (all dialects): the native event maps to one of the Claude events the entry
//! points know; an event the entry point must ignore maps to a name they treat as `Other` (for
//! example the native name), so NOTHING runs and the cursor never moves. Never map to an event
//! whose output the harness cannot display (that would advance the cursor and drop the text).
//!
//! Each per-dialect module exposes exactly `pub(crate) fn parse` and `pub(crate) fn render`.

use agora_deliver::Harness;
use serde_json::Value;

use crate::schema::HookInput;
use crate::{HookError, HookKind, HookOutcome};

/// The UTF-8 byte order mark some producers prepend to stdin.
const BOM: &[u8] = b"\xEF\xBB\xBF";

pub(crate) mod agy;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod vscode;

/// Cursor's stop-hook `loop_limit`, shared with the Cursor installer in `agora-cli`.
pub use cursor::LOOP_LIMIT;

/// Whose native hook protocol a run speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Claude,
    Codex,
    Copilot,
    /// Same as Copilot: the producer is auto-detected per payload (VS Code or Copilot CLI).
    CopilotVsCode,
    Agy,
    Cursor,
}

impl Dialect {
    /// Every dialect, in enum order.
    pub const ALL: [Dialect; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::CopilotVsCode,
        Self::Agy,
        Self::Cursor,
    ];

    /// Inverse of `as_str` (exact, case-sensitive); anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }

    /// `"claude"` | `"codex"` | `"copilot"` | `"copilot-vscode"` | `"agy"` | `"cursor"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::CopilotVsCode => "copilot-vscode",
            Self::Agy => "agy",
            Self::Cursor => "cursor",
        }
    }

    /// Claude -> ClaudeCode, Codex -> Codex, Copilot -> CopilotCli, CopilotVsCode -> CopilotVsCode,
    /// Agy -> Antigravity, Cursor -> Cursor.
    pub fn harness(self) -> Harness {
        match self {
            Self::Claude => Harness::ClaudeCode,
            Self::Codex => Harness::Codex,
            Self::Copilot => Harness::CopilotCli,
            Self::CopilotVsCode => Harness::CopilotVsCode,
            Self::Agy => Harness::Antigravity,
            Self::Cursor => Harness::Cursor,
        }
    }
}

/// Native stdin bytes to the Claude-shaped `HookInput`.
///
/// Every dialect first strips a leading UTF-8 BOM. Claude: `input::parse_input` on the rest; then,
/// when `event` is Some and the parsed `hook_event_name` is empty, the event fills it. Other
/// dialects: parse JSON (invalid or non-object -> `Err(HookError::Input(..))`), dispatch to the
/// dialect module's `parse(event, value)`, then apply two central guards. A name the entry points
/// do not act on becomes `{dialect}:{name}` (`known_or_ignored`). An event with an empty session
/// id becomes `{dialect}:{event}` (a name with ':' is already ignored and is kept). Either way
/// the entry points see an `Other` event and nothing runs.
pub fn to_input(d: Dialect, event: Option<&str>, raw: &[u8]) -> Result<HookInput, HookError> {
    let bytes = raw.strip_prefix(BOM).unwrap_or(raw);
    let mut input = match d {
        Dialect::Claude => return claude_to_input(bytes, event),
        Dialect::Codex => codex::parse(event, json_object(bytes)?)?,
        Dialect::Copilot | Dialect::CopilotVsCode => copilot::parse(event, json_object(bytes)?)?,
        Dialect::Agy => agy::parse(event, json_object(bytes)?)?,
        Dialect::Cursor => cursor::parse(event, json_object(bytes)?)?,
    };
    input.hook_event_name = known_or_ignored(d, &input.hook_event_name);
    if input.session_id.trim().is_empty() && !input.hook_event_name.contains(':') {
        input.hook_event_name = format!("{}:{}", d.as_str(), input.hook_event_name);
    }
    Ok(input)
}

/// The Claude path of `to_input`: `input::parse_input`, then `event` fills an empty event name.
fn claude_to_input(bytes: &[u8], event: Option<&str>) -> Result<HookInput, HookError> {
    let mut input = crate::input::parse_input(bytes)?;
    if input.hook_event_name.is_empty()
        && let Some(e) = event
    {
        input.hook_event_name = e.to_string();
    }
    Ok(input)
}

/// Parse the native stdin as a JSON object; invalid JSON or any other value is `HookError::Input`.
fn json_object(bytes: &[u8]) -> Result<Value, HookError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| HookError::Input(format!("stdin is not valid JSON: {e}")))?;
    if !value.is_object() {
        return Err(HookError::Input("stdin is not a JSON object".into()));
    }
    Ok(value)
}

/// The event name a non-Claude payload may carry into the entry points (RB-3).
///
/// A name that already contains ':' is an ignored name and is returned unchanged. The events the
/// entry points act on, per dialect: Codex, Copilot, Copilot in VS Code and Cursor take
/// `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop` and `SubagentStop`; Agy takes
/// `PostToolBatch` and `Stop`. Any other name becomes `{dialect}:{name}`, which is `Other`.
/// Claude has no native whitelist (this is not called for it), so every name is prefixed.
pub(crate) fn known_or_ignored(d: Dialect, name: &str) -> String {
    if name.contains(':') {
        return name.to_owned();
    }
    let known: &[&str] = match d {
        Dialect::Codex | Dialect::Copilot | Dialect::CopilotVsCode | Dialect::Cursor => &[
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "Stop",
            "SubagentStop",
        ],
        Dialect::Agy => &["PostToolBatch", "Stop"],
        Dialect::Claude => &[],
    };
    if known.contains(&name) {
        name.to_owned()
    } else {
        format!("{}:{}", d.as_str(), name)
    }
}

/// The M4 entry point's outcome to the dialect's native outcome.
///
/// Identity for Claude. Otherwise: `exit_code != 0` or empty stdout -> silent; parse a `ClaudeOut`
/// from stdout (`parse_claude_stdout`); render it with the dialect module (`input` is the
/// `HookInput` the entry point ran with, whose `origin` the Copilot render consults); an empty
/// render -> silent; else stdout = the render, stderr empty, exit 0.
pub fn to_output(d: Dialect, kind: HookKind, input: &HookInput, out: HookOutcome) -> HookOutcome {
    let render: fn(HookKind, &HookInput, &ClaudeOut) -> String = match d {
        Dialect::Claude => return out,
        Dialect::Codex => codex::render,
        Dialect::Copilot | Dialect::CopilotVsCode => copilot::render,
        Dialect::Agy => agy::render,
        Dialect::Cursor => cursor::render,
    };
    if out.exit_code != 0 || out.stdout.trim().is_empty() {
        return HookOutcome::silent();
    }
    let claude_out = parse_claude_stdout(&out.stdout);
    let rendered = render(kind, input, &claude_out);
    if rendered.is_empty() {
        HookOutcome::silent()
    } else {
        HookOutcome::stdout(rendered)
    }
}

/// What an M4 entry point said, dialect-neutral.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeOut {
    /// `hookSpecificOutput.additionalContext`.
    pub context: Option<String>,
    /// `hookSpecificOutput.hookEventName` accompanying `context`.
    pub context_event: Option<String>,
    /// `reason` of a `{"decision":"block","reason":..}` output.
    pub block_reason: Option<String>,
}

/// Parse `{"hookSpecificOutput":{"hookEventName","additionalContext"}}` and/or
/// `{"decision":"block","reason"}`; anything else (invalid JSON, other shapes) -> `ClaudeOut::default()`.
pub fn parse_claude_stdout(stdout: &str) -> ClaudeOut {
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(stdout) else {
        return ClaudeOut::default();
    };
    let mut out = ClaudeOut::default();
    if let Some(specific) = obj.get("hookSpecificOutput").and_then(Value::as_object) {
        out.context = specific
            .get("additionalContext")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.context_event = specific
            .get("hookEventName")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    if obj.get("decision").and_then(Value::as_str) == Some("block") {
        out.block_reason = obj.get("reason").and_then(Value::as_str).map(str::to_owned);
    }
    out
}

/// `mcp__<server>__<tool>` (the Claude/Codex naming `context::is_inbox_tool` understands).
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

#[cfg(test)]
mod tests;
