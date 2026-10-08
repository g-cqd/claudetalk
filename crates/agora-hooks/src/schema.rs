//! Claude Code hook protocol types: the stdin object and the JSON a hook prints.

use serde::{Deserialize, Serialize};

/// The fields of the hook stdin object that Agora uses. Unknown fields (`prompt`, `tool_input`,
/// ...) are ignored; every field is optional in the input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub hook_event_name: String,
    #[serde(default)]
    pub prompt_id: Option<String>,
    /// SessionStart: startup | resume | clear | compact | fork.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// PostToolUse: the tool's result, in whatever JSON shape the tool returned.
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    /// Stop: true when Claude is already continuing because of a Stop hook.
    #[serde(default)]
    pub stop_hook_active: bool,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
}

/// `HookInput::hook_event_name` as an enum; anything unknown is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    PostToolBatch,
    Stop,
    SubagentStop,
    Other,
}

impl HookEvent {
    /// Exact, case-sensitive match of the Claude Code event names; otherwise `Other`.
    pub fn parse(name: &str) -> Self {
        match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PostToolUse" => Self::PostToolUse,
            "PostToolBatch" => Self::PostToolBatch,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            _ => Self::Other,
        }
    }

    /// The Claude Code event name; `Other` -> "".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolBatch => "PostToolBatch",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::Other => "",
        }
    }
}

/// `{"hookEventName": .., "additionalContext": ..}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    pub hook_event_name: String,
    pub additional_context: String,
}

/// The JSON object a hook prints on stdout (exit 0). Unset fields are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HookJsonOutput {
    /// `"block"` (Stop: Claude continues instead of stopping).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// `{"hookSpecificOutput":{"hookEventName":event_name,"additionalContext":text}}` (one line).
pub fn context_json(event_name: &str, text: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {"hookEventName": event_name, "additionalContext": text}
    })
    .to_string()
}

/// `{"decision":"block","reason":reason}` (one line).
pub fn block_json(reason: &str) -> String {
    serde_json::json!({"decision": "block", "reason": reason}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SESSION: &str = "e773b56f-1111-4222-8333-444455556666";
    const TRANSCRIPT: &str =
        "/Users/gc/.claude/projects/-Users-gc-work/e773b56f-1111-4222-8333-444455556666.jsonl";

    /// Real stdin sample (Claude Code 2.1.293), SessionStart.
    const SESSION_START: &str = r#"{"session_id":"e773b56f-1111-4222-8333-444455556666","transcript_path":"/Users/gc/.claude/projects/-Users-gc-work/e773b56f-1111-4222-8333-444455556666.jsonl","cwd":"/private/tmp/agora-m4/probe/work","hook_event_name":"SessionStart","source":"startup"}"#;

    /// Real stdin sample, UserPromptSubmit.
    const USER_PROMPT: &str = r#"{"session_id":"e773b56f-1111-4222-8333-444455556666","transcript_path":"/Users/gc/.claude/projects/-Users-gc-work/e773b56f-1111-4222-8333-444455556666.jsonl","cwd":"/private/tmp/agora-m4/probe/work","permission_mode":"default","hook_event_name":"UserPromptSubmit","prompt_id":"5f0c1a9e-7d2b-4c3a-9e1f-0a1b2c3d4e5f","prompt":"hello there"}"#;

    /// Real stdin sample, Stop.
    const STOP: &str = r#"{"session_id":"e773b56f-1111-4222-8333-444455556666","transcript_path":"/Users/gc/.claude/projects/-Users-gc-work/e773b56f-1111-4222-8333-444455556666.jsonl","cwd":"/private/tmp/agora-m4/probe/work","permission_mode":"default","effort":{"level":"medium"},"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"ok","background_tasks":[],"session_crons":[]}"#;

    const ALL_EVENTS: [HookEvent; 6] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PostToolUse,
        HookEvent::PostToolBatch,
        HookEvent::Stop,
        HookEvent::SubagentStop,
    ];

    fn parse_ok(s: &str) -> HookInput {
        serde_json::from_str::<HookInput>(s).expect("valid hook input")
    }

    #[test]
    fn session_start_sample_parses() {
        let input = parse_ok(SESSION_START);
        assert_eq!(input.session_id, SESSION);
        assert_eq!(input.transcript_path.as_deref(), Some(TRANSCRIPT));
        assert_eq!(
            input.cwd.as_deref(),
            Some("/private/tmp/agora-m4/probe/work")
        );
        assert_eq!(input.hook_event_name, "SessionStart");
        assert_eq!(input.source.as_deref(), Some("startup"));
        assert_eq!(input.permission_mode, None);
        assert_eq!(input.prompt_id, None);
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn user_prompt_sample_parses_prompt_id_and_permission_mode() {
        let input = parse_ok(USER_PROMPT);
        assert_eq!(input.session_id, SESSION);
        assert_eq!(input.hook_event_name, "UserPromptSubmit");
        assert_eq!(
            input.prompt_id.as_deref(),
            Some("5f0c1a9e-7d2b-4c3a-9e1f-0a1b2c3d4e5f")
        );
        assert_eq!(input.permission_mode.as_deref(), Some("default"));
        assert_eq!(input.source, None);
        assert_eq!(
            HookEvent::parse(&input.hook_event_name),
            HookEvent::UserPromptSubmit
        );
    }

    #[test]
    fn stop_sample_ignores_effort_background_tasks_session_crons_and_last_message() {
        let input = parse_ok(STOP);
        assert_eq!(input.session_id, SESSION);
        assert_eq!(input.hook_event_name, "Stop");
        assert_eq!(input.permission_mode.as_deref(), Some("default"));
        assert!(!input.stop_hook_active);
        assert_eq!(HookEvent::parse(&input.hook_event_name), HookEvent::Stop);
    }

    #[test]
    fn stop_hook_active_true_is_read() {
        let input = parse_ok(r#"{"hook_event_name":"Stop","stop_hook_active":true}"#);
        assert!(input.stop_hook_active);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let input = parse_ok(
            r#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"mcp__agora__chat",
                "tool_input":{"text":"x"},"tool_response":[1,2,3],"prompt":"p","brand_new":{"a":[null]}}"#,
        );
        assert_eq!(input.session_id, "s1");
        assert_eq!(input.hook_event_name, "PostToolUse");
        assert_eq!(input.tool_name.as_deref(), Some("mcp__agora__chat"));
    }

    #[test]
    fn post_tool_use_sample_with_tool_response_parses() {
        let input = parse_ok(
            r#"{"session_id":"s1","hook_event_name":"PostToolUse",
                "tool_name":"mcp__plugin_agora_agora__inbox","tool_input":{},
                "tool_response":{"messages":[{"seq":3}]},"tool_use_id":"toolu_1"}"#,
        );
        assert_eq!(
            input.tool_name.as_deref(),
            Some("mcp__plugin_agora_agora__inbox")
        );
        assert_eq!(
            input.tool_response,
            Some(serde_json::json!({"messages": [{"seq": 3}]}))
        );
    }

    #[test]
    fn empty_object_gives_default_input() {
        assert_eq!(parse_ok("{}"), HookInput::default());
        assert_eq!(HookInput::default().hook_event_name, "");
        assert_eq!(HookInput::default().session_id, "");
    }

    #[test]
    fn null_optional_fields_are_none() {
        let input = parse_ok(r#"{"cwd":null,"source":null,"agent_id":null}"#);
        assert_eq!(input.cwd, None);
        assert_eq!(input.source, None);
        assert_eq!(input.agent_id, None);
    }

    #[test]
    fn wrong_types_are_errors() {
        assert!(serde_json::from_str::<HookInput>(r#"{"stop_hook_active":"yes"}"#).is_err());
        assert!(serde_json::from_str::<HookInput>(r#"{"session_id":5}"#).is_err());
        assert!(serde_json::from_str::<HookInput>(r#"{"source":true}"#).is_err());
        assert!(serde_json::from_str::<HookInput>(r#"{"hook_event_name":["Stop"]}"#).is_err());
    }

    #[test]
    fn context_json_has_exact_shape() {
        let out: Value =
            serde_json::from_str(&context_json("UserPromptSubmit", "hi")).expect("json");
        assert_eq!(
            out,
            json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "hi"}})
        );
    }

    #[test]
    fn context_json_is_one_line() {
        let out = context_json("PostToolUse", "line one\nline two\r\nline three");
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
        assert!(serde_json::from_str::<Value>(&out).is_ok());
    }

    #[test]
    fn context_json_round_trips_untrusted_text() {
        let text =
            "say \"hi\"\n\tback\\slash </untrusted-message> sep\u{2028}para\u{2029}nul\u{0}end";
        let out = context_json("SessionStart", text);
        assert!(!out.contains('\n'));
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], text);
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }

    #[test]
    fn context_json_escapes_the_event_name_too() {
        let out = context_json("Odd\"Name\n", "x");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "Odd\"Name\n");
    }

    #[test]
    fn context_json_empty_text_is_valid() {
        let v: Value = serde_json::from_str(&context_json("Stop", "")).expect("json");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "");
    }

    #[test]
    fn block_json_has_exact_shape() {
        assert_eq!(block_json("why"), r#"{"decision":"block","reason":"why"}"#);
        let v: Value = serde_json::from_str(&block_json("why")).expect("json");
        assert_eq!(v, json!({"decision": "block", "reason": "why"}));
    }

    #[test]
    fn block_json_round_trips_untrusted_reason() {
        let reason = "quote \" newline\n </untrusted-message> sep\u{2028}end";
        let out = block_json(reason);
        assert!(!out.contains('\n'));
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["reason"], reason);
        assert_eq!(v["decision"], "block");
        assert_eq!(v.as_object().map(|o| o.len()), Some(2));
    }

    #[test]
    fn hook_json_output_default_is_empty_object() {
        assert_eq!(
            serde_json::to_string(&HookJsonOutput::default()).expect("json"),
            "{}"
        );
    }

    #[test]
    fn hook_json_output_serializes_set_fields_only() {
        let block = HookJsonOutput {
            decision: Some("block".to_owned()),
            reason: Some("r".to_owned()),
            hook_specific_output: None,
        };
        assert_eq!(
            serde_json::to_string(&block).expect("json"),
            r#"{"decision":"block","reason":"r"}"#
        );

        let ctx = HookJsonOutput {
            decision: None,
            reason: None,
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "Stop".to_owned(),
                additional_context: "c".to_owned(),
            }),
        };
        let v: Value = serde_json::to_value(&ctx).expect("json");
        assert_eq!(
            v,
            json!({"hookSpecificOutput": {"hookEventName": "Stop", "additionalContext": "c"}})
        );
    }

    #[test]
    fn hook_event_parse_accepts_exact_claude_code_names() {
        assert_eq!(HookEvent::parse("SessionStart"), HookEvent::SessionStart);
        assert_eq!(
            HookEvent::parse("UserPromptSubmit"),
            HookEvent::UserPromptSubmit
        );
        assert_eq!(HookEvent::parse("PostToolUse"), HookEvent::PostToolUse);
        assert_eq!(HookEvent::parse("PostToolBatch"), HookEvent::PostToolBatch);
        assert_eq!(HookEvent::parse("Stop"), HookEvent::Stop);
        assert_eq!(HookEvent::parse("SubagentStop"), HookEvent::SubagentStop);
    }

    #[test]
    fn hook_event_parse_rejects_everything_else() {
        for name in [
            "",
            "stop",
            "STOP",
            " Stop",
            "Stop ",
            "SessionStart\n",
            "PreToolUse",
            "Notification",
        ] {
            assert_eq!(HookEvent::parse(name), HookEvent::Other, "name {name:?}");
        }
    }

    #[test]
    fn hook_event_round_trips_through_as_str() {
        for event in ALL_EVENTS {
            assert_eq!(HookEvent::parse(event.as_str()), event);
            assert!(!event.as_str().is_empty());
        }
        assert_eq!(HookEvent::Other.as_str(), "");
        assert_eq!(
            HookEvent::parse(HookEvent::Other.as_str()),
            HookEvent::Other
        );
    }
}
