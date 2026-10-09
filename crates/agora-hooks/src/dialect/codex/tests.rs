//! Unit tests for the Codex dialect, on the real Codex CLI 0.157.1 payloads in
//! `tests/fixtures/codex/` (captured 2026-10-08).

use serde_json::{Value, json};

use super::{parse, render};
use crate::dialect::{ClaudeOut, Dialect, to_input, to_output};
use crate::schema::{HookInput, block_json, context_json};
use crate::{HookError, HookKind, HookOutcome};

const SESSION_START: &str = include_str!("../../../tests/fixtures/codex/hook_SessionStart.json");
const USER_PROMPT_SUBMIT: &str =
    include_str!("../../../tests/fixtures/codex/hook_UserPromptSubmit.json");
const POST_TOOL_USE: &str = include_str!("../../../tests/fixtures/codex/hook_PostToolUse.json");
const POST_TOOL_USE_BASH: &str =
    include_str!("../../../tests/fixtures/codex/hook_PostToolUse_Bash.json");
const STOP: &str = include_str!("../../../tests/fixtures/codex/hook_Stop.json");
const STOP_SECOND_ACTIVE: &str =
    include_str!("../../../tests/fixtures/codex/hook_Stop_second_active.json");
const SESSION_END: &str = include_str!("../../../tests/fixtures/codex/hook_SessionEnd.json");

/// Session of the SessionStart, UserPromptSubmit, PostToolUse (MCP), Stop and SessionEnd captures.
const SESSION_A: &str = "01a11cd9-600c-7530-96b7-db4f0fc53a73";
/// Session of the PostToolUse (Bash) capture.
const SESSION_B: &str = "01a11ce8-12e1-7180-8e01-853b98ebf200";
/// Session of the second-active Stop capture.
const SESSION_C: &str = "01a11ce4-ca2b-7231-ab98-290de32f5168";
const CWD_PRIVATE: &str = "/private/tmp/agora-m6/probe-codex/proj";
const CWD_PLAIN: &str = "/tmp/agora-m6/probe-codex/proj";

const ALL_FIXTURES: [&str; 7] = [
    SESSION_START,
    USER_PROMPT_SUBMIT,
    POST_TOOL_USE,
    POST_TOOL_USE_BASH,
    STOP,
    STOP_SECOND_ACTIVE,
    SESSION_END,
];

fn fixture(text: &str) -> HookInput {
    to_input(Dialect::Codex, None, text.as_bytes()).expect("captured Codex payload parses")
}

fn value(s: &str) -> Value {
    serde_json::from_str(s).expect("rendered output is JSON")
}

fn context_out(text: &str) -> ClaudeOut {
    ClaudeOut {
        context: Some(text.to_owned()),
        ..ClaudeOut::default()
    }
}

fn block_out(reason: &str) -> ClaudeOut {
    ClaudeOut {
        block_reason: Some(reason.to_owned()),
        ..ClaudeOut::default()
    }
}

#[test]
fn every_fixture_parses_with_its_identity_fields() {
    let cases = [
        (
            SESSION_START,
            SESSION_A,
            CWD_PRIVATE,
            "SessionStart",
            None,
            false,
        ),
        (
            USER_PROMPT_SUBMIT,
            SESSION_A,
            CWD_PRIVATE,
            "UserPromptSubmit",
            None,
            false,
        ),
        (
            POST_TOOL_USE,
            SESSION_A,
            CWD_PRIVATE,
            "PostToolUse",
            Some("mcp__logger__ping"),
            false,
        ),
        (
            POST_TOOL_USE_BASH,
            SESSION_B,
            CWD_PLAIN,
            "PostToolUse",
            Some("Bash"),
            false,
        ),
        (STOP, SESSION_A, CWD_PRIVATE, "Stop", None, false),
        (
            STOP_SECOND_ACTIVE,
            SESSION_C,
            CWD_PRIVATE,
            "Stop",
            None,
            true,
        ),
        (
            SESSION_END,
            SESSION_A,
            CWD_PRIVATE,
            "codex:SessionEnd",
            None,
            false,
        ),
    ];
    for (text, session, cwd, event, tool, active) in cases {
        let input = fixture(text);
        assert_eq!(input.session_id, session, "{event}");
        assert_eq!(input.cwd.as_deref(), Some(cwd), "{event}");
        assert_eq!(input.hook_event_name, event);
        assert_eq!(input.tool_name.as_deref(), tool, "{event}");
        assert_eq!(input.stop_hook_active, active, "{event}");
        assert_eq!(input.origin.as_deref(), Some("codex"), "{event}");
    }
}

#[test]
fn session_start_fixture_keeps_its_source() {
    let input = fixture(SESSION_START);
    assert_eq!(input.source.as_deref(), Some("startup"));
}

#[test]
fn mcp_post_tool_use_keeps_tool_response_as_the_object() {
    let input = fixture(POST_TOOL_USE);
    assert_eq!(
        input.tool_response,
        Some(json!({"content": [{"type": "text", "text": "pong-1"}]}))
    );
}

#[test]
fn bash_post_tool_use_keeps_tool_response_as_the_string() {
    let input = fixture(POST_TOOL_USE_BASH);
    assert_eq!(input.tool_response, Some(Value::String("one\n".into())));
}

#[test]
fn event_argument_fills_only_an_empty_event_name() {
    let empty = br#"{"session_id":"s1","cwd":"/w"}"#;
    let filled = to_input(Dialect::Codex, Some("SessionStart"), empty).expect("valid object");
    assert_eq!(filled.hook_event_name, "SessionStart");
    assert_eq!(filled.origin.as_deref(), Some("codex"));

    let kept =
        to_input(Dialect::Codex, Some("PostToolUse"), STOP.as_bytes()).expect("valid object");
    assert_eq!(kept.hook_event_name, "Stop");
}

#[test]
fn wrong_field_type_is_an_input_error() {
    let raw = br#"{"session_id":5,"hook_event_name":"Stop"}"#;
    assert!(matches!(
        to_input(Dialect::Codex, None, raw),
        Err(HookError::Input(_))
    ));
    let direct = parse(None, json!({"stop_hook_active": "yes"}));
    assert!(matches!(direct, Err(HookError::Input(_))));
}

#[test]
fn null_and_unread_fields_do_not_fail_the_payload() {
    let raw = br#"{"turn_id": null, "stop_hook_active": null, "model": 5, "session_id": "s", "hook_event_name": "Stop", "cwd": "/x"}"#;
    let input = to_input(Dialect::Codex, None, raw).expect("null and unread fields parse");
    assert!(!input.stop_hook_active);
    assert_eq!(input.session_id, "s");
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.cwd.as_deref(), Some("/x"));
}

#[test]
fn context_renders_for_the_three_context_events() {
    let cases = [
        (SESSION_START, "SessionStart"),
        (USER_PROMPT_SUBMIT, "UserPromptSubmit"),
        (POST_TOOL_USE, "PostToolUse"),
    ];
    for (text, event) in cases {
        let input = fixture(text);
        let got = render(HookKind::Context, &input, &context_out("hi"));
        assert_eq!(
            value(&got),
            json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": "hi"}}),
            "{event}"
        );
    }
}

#[test]
fn block_renders_for_stop() {
    let input = fixture(STOP);
    let got = render(HookKind::Stop, &input, &block_out("unread mail"));
    assert_eq!(
        value(&got),
        json!({"decision": "block", "reason": "unread mail"})
    );
}

#[test]
fn context_on_stop_or_session_end_is_silent() {
    for text in [STOP, SESSION_END] {
        let input = fixture(text);
        assert_eq!(
            render(HookKind::Context, &input, &context_out("hi")),
            "",
            "{}",
            input.hook_event_name
        );
    }
}

#[test]
fn block_on_post_tool_use_is_silent() {
    let input = fixture(POST_TOOL_USE);
    assert_eq!(render(HookKind::Stop, &input, &block_out("why")), "");
}

#[test]
fn empty_claude_out_is_silent_for_every_fixture() {
    for text in ALL_FIXTURES {
        let input = fixture(text);
        assert_eq!(
            render(HookKind::Context, &input, &ClaudeOut::default()),
            "",
            "{}",
            input.hook_event_name
        );
    }
}

#[test]
fn end_to_end_post_tool_use_context_is_the_codex_context_json() {
    let input = to_input(Dialect::Codex, None, POST_TOOL_USE.as_bytes()).expect("fixture parses");
    let out = to_output(
        Dialect::Codex,
        HookKind::Context,
        &input,
        HookOutcome::stdout(context_json("PostToolUse", "hi")),
    );
    assert_eq!(out, HookOutcome::stdout(context_json("PostToolUse", "hi")));
    assert_eq!(
        value(&out.stdout),
        json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "hi"}})
    );
}

#[test]
fn end_to_end_stop_block_is_the_codex_block_json() {
    let input = to_input(Dialect::Codex, None, STOP.as_bytes()).expect("fixture parses");
    let out = to_output(
        Dialect::Codex,
        HookKind::Stop,
        &input,
        HookOutcome::stdout(block_json("unread mail")),
    );
    assert_eq!(out, HookOutcome::stdout(block_json("unread mail")));
    assert_eq!(
        value(&out.stdout),
        json!({"decision": "block", "reason": "unread mail"})
    );
}
