//! Unit tests for the dialect conversions in `dialect/mod.rs`.

use super::*;
use crate::schema::{HookEvent, block_json, context_json};
use serde_json::json;

fn non_claude() -> impl Iterator<Item = Dialect> {
    Dialect::ALL.into_iter().filter(|d| *d != Dialect::Claude)
}

#[test]
fn as_str_and_parse_round_trip_for_all_six() {
    assert_eq!(Dialect::ALL.len(), 6);
    for d in Dialect::ALL {
        assert_eq!(Dialect::parse(d.as_str()), Some(d), "{d:?}");
    }
}

#[test]
fn as_str_gives_the_spec_names_in_enum_order() {
    let names: Vec<&str> = Dialect::ALL.iter().map(|d| d.as_str()).collect();
    assert_eq!(
        names,
        [
            "claude",
            "codex",
            "copilot",
            "copilot-vscode",
            "agy",
            "cursor"
        ]
    );
}

#[test]
fn parse_rejects_unknown_and_non_exact_names() {
    for s in [
        "Codex",
        "copilot_vscode",
        "",
        "Claude",
        " claude",
        "copilot-vs-code",
    ] {
        assert_eq!(Dialect::parse(s), None, "{s:?}");
    }
}

#[test]
fn default_dialect_is_claude() {
    assert_eq!(Dialect::default(), Dialect::Claude);
}

#[test]
fn harness_mapping_matches_the_spec() {
    assert_eq!(Dialect::Claude.harness(), Harness::ClaudeCode);
    assert_eq!(Dialect::Codex.harness(), Harness::Codex);
    assert_eq!(Dialect::Copilot.harness(), Harness::CopilotCli);
    assert_eq!(Dialect::CopilotVsCode.harness(), Harness::CopilotVsCode);
    assert_eq!(Dialect::Agy.harness(), Harness::Antigravity);
    assert_eq!(Dialect::Cursor.harness(), Harness::Cursor);
}

#[test]
fn parse_claude_stdout_reads_context_json() {
    let out = parse_claude_stdout(&context_json("PostToolUse", "hello <b>"));
    assert_eq!(
        out,
        ClaudeOut {
            context: Some("hello <b>".into()),
            context_event: Some("PostToolUse".into()),
            block_reason: None,
        }
    );
}

#[test]
fn parse_claude_stdout_reads_block_json() {
    let out = parse_claude_stdout(&block_json("unread mail"));
    assert_eq!(
        out,
        ClaudeOut {
            context: None,
            context_event: None,
            block_reason: Some("unread mail".into()),
        }
    );
}

#[test]
fn parse_claude_stdout_reads_both_shapes_in_one_object() {
    let mut v: Value =
        serde_json::from_str(&context_json("Stop", "ctx")).expect("context_json is JSON");
    v["decision"] = json!("block");
    v["reason"] = json!("why");
    let out = parse_claude_stdout(&v.to_string());
    assert_eq!(out.context.as_deref(), Some("ctx"));
    assert_eq!(out.context_event.as_deref(), Some("Stop"));
    assert_eq!(out.block_reason.as_deref(), Some("why"));
}

#[test]
fn parse_claude_stdout_garbage_is_default() {
    for s in ["not json", "{", "[1,2]", "\"text\"", "42", "null"] {
        assert_eq!(parse_claude_stdout(s), ClaudeOut::default(), "{s:?}");
    }
}

#[test]
fn parse_claude_stdout_empty_is_default() {
    for s in ["", "   \n"] {
        assert_eq!(parse_claude_stdout(s), ClaudeOut::default(), "{s:?}");
    }
}

#[test]
fn parse_claude_stdout_ignores_other_decisions_and_wrong_types() {
    let cases = [
        json!({"decision": "approve", "reason": "x"}),
        json!({"decision": "block"}),
        json!({"decision": "block", "reason": 5}),
        json!({"hookSpecificOutput": {"additionalContext": 5}}),
    ];
    for case in cases {
        let out = parse_claude_stdout(&case.to_string());
        assert_eq!(out.block_reason, None, "{case}");
        assert_eq!(out.context, None, "{case}");
    }
}

#[test]
fn mcp_tool_name_uses_the_double_underscore_form() {
    assert_eq!(
        mcp_tool_name("agora", "inbox_read"),
        "mcp__agora__inbox_read"
    );
}

#[test]
fn claude_to_input_parses_bom_prefixed_json() {
    let mut raw = BOM.to_vec();
    raw.extend_from_slice(br#"{"session_id":"s1","hook_event_name":"Stop"}"#);
    let input = to_input(Dialect::Claude, None, &raw).expect("BOM-prefixed JSON parses");
    assert_eq!(input.session_id, "s1");
    assert_eq!(input.hook_event_name, "Stop");
}

#[test]
fn claude_to_input_fills_empty_event_name_from_event_arg() {
    let raw = br#"{"session_id":"s1"}"#;
    let input = to_input(Dialect::Claude, Some("SessionStart"), raw).expect("valid object");
    assert_eq!(input.hook_event_name, "SessionStart");
}

#[test]
fn claude_to_input_keeps_payload_event_name_over_event_arg() {
    let raw = br#"{"hook_event_name":"Stop"}"#;
    let input = to_input(Dialect::Claude, Some("SessionStart"), raw).expect("valid object");
    assert_eq!(input.hook_event_name, "Stop");
}

#[test]
fn claude_to_input_rejects_non_object_and_empty_stdin() {
    for raw in [&b"[]"[..], &b"42"[..], &b""[..]] {
        assert!(
            matches!(
                to_input(Dialect::Claude, None, raw),
                Err(HookError::Input(_))
            ),
            "{raw:?}"
        );
    }
}

#[test]
fn other_dialects_reject_invalid_json_and_non_objects_as_input_errors() {
    let mut bom_array = BOM.to_vec();
    bom_array.extend_from_slice(b"[]");
    let cases: [&[u8]; 6] = [b"{", b"[]", b"\"x\"", b"42", b"", &bom_array];
    for d in non_claude() {
        for raw in cases {
            assert!(
                matches!(to_input(d, None, raw), Err(HookError::Input(_))),
                "{d:?} {raw:?}"
            );
        }
    }
}

#[test]
fn claude_to_output_is_identity_including_exit_code_two() {
    let wake = HookOutcome::wake("wake text".into());
    let out = to_output(
        Dialect::Claude,
        HookKind::Rewake,
        &HookInput::default(),
        wake.clone(),
    );
    assert_eq!(out, wake);
    assert_eq!(out.exit_code, 2);
}

#[test]
fn claude_to_output_passes_stdout_through() {
    let stdout = HookOutcome::stdout(context_json("SessionStart", "x"));
    let out = to_output(
        Dialect::Claude,
        HookKind::SessionStart,
        &HookInput::default(),
        stdout.clone(),
    );
    assert_eq!(out, stdout);
}

#[test]
fn non_claude_nonzero_exit_is_silent_even_with_stdout() {
    let out = HookOutcome {
        stdout: context_json("PostToolUse", "x"),
        stderr: "wake".into(),
        exit_code: 2,
    };
    for d in non_claude() {
        let got = to_output(d, HookKind::Context, &HookInput::default(), out.clone());
        assert_eq!(got, HookOutcome::silent(), "{d:?}");
    }
}

#[test]
fn non_claude_empty_or_blank_stdout_is_silent() {
    for stdout in ["", " \n\t"] {
        let out = HookOutcome::stdout(stdout.into());
        for d in non_claude() {
            let got = to_output(d, HookKind::Stop, &HookInput::default(), out.clone());
            assert_eq!(got, HookOutcome::silent(), "{d:?} {stdout:?}");
        }
    }
}

const CLAUDE_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "Stop",
    "SubagentStop",
];

#[test]
fn known_events_pass_through_for_each_dialect() {
    for d in [
        Dialect::Codex,
        Dialect::Copilot,
        Dialect::CopilotVsCode,
        Dialect::Cursor,
    ] {
        for name in CLAUDE_EVENTS {
            assert_eq!(known_or_ignored(d, name), name, "{d:?} {name}");
        }
    }
    for name in ["PostToolBatch", "Stop"] {
        assert_eq!(known_or_ignored(Dialect::Agy, name), name, "agy {name}");
    }
}

#[test]
fn unknown_events_get_the_dialect_prefix() {
    for d in [
        Dialect::Codex,
        Dialect::Copilot,
        Dialect::CopilotVsCode,
        Dialect::Cursor,
    ] {
        assert_eq!(
            known_or_ignored(d, "PostToolBatch"),
            format!("{}:PostToolBatch", d.as_str())
        );
    }
    assert_eq!(
        known_or_ignored(Dialect::Agy, "SessionStart"),
        "agy:SessionStart"
    );
    assert_eq!(
        known_or_ignored(Dialect::Codex, "userPromptSubmitted"),
        "codex:userPromptSubmitted"
    );
}

#[test]
fn codex_post_tool_batch_and_pre_tool_use_are_ignored() {
    for event in ["PostToolBatch", "PreToolUse"] {
        let raw = format!(r#"{{"session_id":"s1","hook_event_name":"{event}"}}"#);
        let input = to_input(Dialect::Codex, None, raw.as_bytes()).expect("valid object");
        assert_eq!(input.hook_event_name, format!("codex:{event}"));
        assert_eq!(HookEvent::parse(&input.hook_event_name), HookEvent::Other);
    }
}

#[test]
fn agy_pre_and_post_invocation_still_map_to_post_tool_batch() {
    for native in ["PreInvocation", "PostInvocation"] {
        let input = to_input(Dialect::Agy, Some(native), br#"{"conversationId":"c1"}"#)
            .expect("valid object");
        assert_eq!(input.hook_event_name, "PostToolBatch", "{native}");
        assert_eq!(input.session_id, "c1");
    }
}

#[test]
fn prefixed_names_are_left_alone() {
    assert_eq!(known_or_ignored(Dialect::Codex, "agy:Stop"), "agy:Stop");
    let with_id = to_input(Dialect::Codex, Some("agy:Stop"), br#"{"session_id":"s1"}"#)
        .expect("valid object");
    assert_eq!(with_id.hook_event_name, "agy:Stop");
    let without_id = to_input(Dialect::Codex, Some("agy:Stop"), b"{}").expect("valid object");
    assert_eq!(without_id.hook_event_name, "agy:Stop");
}

#[test]
fn empty_session_id_ignores_the_event_for_each_non_claude_dialect() {
    // Copilot CLI 0.0.369 payloads carry no sessionId; their name is already `copilot:<native>`.
    let cases: [(Dialect, Option<&str>, &str, &str); 6] = [
        (
            Dialect::Codex,
            None,
            r#"{"hook_event_name":"Stop"}"#,
            "codex:Stop",
        ),
        (
            Dialect::Copilot,
            Some("sessionStart"),
            r#"{"cwd":"/w"}"#,
            "copilot:sessionStart",
        ),
        (
            Dialect::Copilot,
            None,
            r#"{"hook_event_name":"SessionStart"}"#,
            "copilot:SessionStart",
        ),
        (
            Dialect::CopilotVsCode,
            None,
            r#"{"hook_event_name":"SessionStart"}"#,
            "copilot-vscode:SessionStart",
        ),
        (Dialect::Agy, Some("Stop"), "{}", "agy:Stop"),
        (
            Dialect::Cursor,
            None,
            r#"{"hook_event_name":"stop"}"#,
            "cursor:Stop",
        ),
    ];
    for (d, event, raw, expected) in cases {
        let input = to_input(d, event, raw.as_bytes()).expect("valid object");
        assert_eq!(input.hook_event_name, expected, "{d:?} {raw}");
        assert_eq!(HookEvent::parse(&input.hook_event_name), HookEvent::Other);
    }
}

#[test]
fn known_session_id_keeps_the_mapped_event() {
    let cases: [(Dialect, Option<&str>, &str, &str); 4] = [
        (
            Dialect::Codex,
            None,
            r#"{"session_id":"s1","hook_event_name":"Stop"}"#,
            "Stop",
        ),
        (
            Dialect::CopilotVsCode,
            None,
            r#"{"session_id":"s1","hook_event_name":"SessionStart"}"#,
            "SessionStart",
        ),
        (
            Dialect::Cursor,
            None,
            r#"{"conversation_id":"c1","hook_event_name":"stop"}"#,
            "Stop",
        ),
        (
            Dialect::Agy,
            Some("Stop"),
            r#"{"conversationId":"c1"}"#,
            "Stop",
        ),
    ];
    for (d, event, raw, expected) in cases {
        let input = to_input(d, event, raw.as_bytes()).expect("valid object");
        assert_eq!(input.hook_event_name, expected, "{d:?} {raw}");
    }
}

#[test]
fn claude_keeps_events_and_names_without_a_session_id() {
    let input = to_input(
        Dialect::Claude,
        None,
        br#"{"hook_event_name":"PostToolBatch"}"#,
    )
    .expect("valid object");
    assert_eq!(input.hook_event_name, "PostToolBatch");
    assert_eq!(input.session_id, "");
    let input = to_input(Dialect::Claude, Some("SessionStart"), b"{}").expect("valid object");
    assert_eq!(input.hook_event_name, "SessionStart");
}
