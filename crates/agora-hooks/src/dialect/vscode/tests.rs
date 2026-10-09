//! Unit tests for the VS Code dialect, on the DOCS-DERIVED payloads in `tests/fixtures/vscode/`
//! (see the README there: not captured from a running VS Code).

use serde_json::{Value, json};

use super::{parse, render};
use crate::dialect::ClaudeOut;
use crate::schema::{HookEvent, HookInput};
use crate::{HookError, HookKind};

const SESSION_START: &str = include_str!("../../../tests/fixtures/vscode/SessionStart.json");
const USER_PROMPT_SUBMIT: &str =
    include_str!("../../../tests/fixtures/vscode/UserPromptSubmit.json");
const POST_TOOL_USE: &str = include_str!("../../../tests/fixtures/vscode/PostToolUse.json");
const STOP: &str = include_str!("../../../tests/fixtures/vscode/Stop.json");
const STOP_ACTIVE: &str = include_str!("../../../tests/fixtures/vscode/Stop_active.json");
const SUBAGENT_STOP: &str = include_str!("../../../tests/fixtures/vscode/SubagentStop.json");

/// Session of every fixture.
const SESSION: &str = "5c1e8a2f-3b7d-4e9a-8f10-2d6c4b7a9e31";
const CWD: &str = "/Users/me/proj";

const ALL_FIXTURES: [&str; 6] = [
    SESSION_START,
    USER_PROMPT_SUBMIT,
    POST_TOOL_USE,
    STOP,
    STOP_ACTIVE,
    SUBAGENT_STOP,
];

/// Parse a fixture through the dialect parser, as `to_input` does after the JSON decode.
fn parse_fixture(text: &str) -> HookInput {
    let v: Value = serde_json::from_str(text).expect("fixture is valid JSON");
    parse(None, v).expect("fixture parses")
}

fn parse_value(v: Value) -> HookInput {
    parse(None, v).expect("payload parses")
}

fn out_context(text: &str) -> ClaudeOut {
    ClaudeOut {
        context: Some(text.to_owned()),
        context_event: None,
        block_reason: None,
    }
}

fn out_block(reason: &str) -> ClaudeOut {
    ClaudeOut {
        context: None,
        context_event: None,
        block_reason: Some(reason.to_owned()),
    }
}

/// Render and parse the result as JSON; the render must be a single JSON object.
fn render_json(kind: HookKind, input: &HookInput, out: &ClaudeOut) -> Value {
    let s = render(kind, input, out);
    serde_json::from_str(&s).expect("render is JSON")
}

#[test]
fn every_fixture_parses_with_session_cwd_and_origin() {
    for text in ALL_FIXTURES {
        let input = parse_fixture(text);
        assert_eq!(input.session_id, SESSION);
        assert_eq!(input.cwd.as_deref(), Some(CWD));
        assert_eq!(input.origin.as_deref(), Some("vscode"));
        assert!(input.transcript_path.is_some());
    }
}

#[test]
fn session_start_fixture_maps_new_to_startup() {
    let input = parse_fixture(SESSION_START);
    assert_eq!(input.hook_event_name, "SessionStart");
    assert_eq!(input.source.as_deref(), Some("startup"));
    assert!(!input.stop_hook_active);
}

#[test]
fn user_prompt_submit_fixture_has_no_source() {
    let input = parse_fixture(USER_PROMPT_SUBMIT);
    assert_eq!(input.hook_event_name, "UserPromptSubmit");
    assert_eq!(input.source, None);
}

#[test]
fn post_tool_use_fixture_keeps_tool_name_and_object_response() {
    let input = parse_fixture(POST_TOOL_USE);
    assert_eq!(input.hook_event_name, "PostToolUse");
    assert_eq!(input.tool_name.as_deref(), Some("mcp__agora__chat"));
    let response = input.tool_response.expect("tool_response present");
    assert!(response.is_object(), "object response kept as an object");
    assert_eq!(response["content"][0]["type"], "text");
}

#[test]
fn stop_fixtures_carry_stop_hook_active() {
    let stop = parse_fixture(STOP);
    assert_eq!(HookEvent::parse(&stop.hook_event_name), HookEvent::Stop);
    assert!(!stop.stop_hook_active);

    let active = parse_fixture(STOP_ACTIVE);
    assert_eq!(HookEvent::parse(&active.hook_event_name), HookEvent::Stop);
    assert!(active.stop_hook_active);
}

#[test]
fn subagent_stop_fixture_keeps_agent_identity() {
    let input = parse_fixture(SUBAGENT_STOP);
    assert_eq!(input.hook_event_name, "SubagentStop");
    assert_eq!(input.agent_type.as_deref(), Some("Explore"));
    assert_eq!(
        input.agent_id.as_deref(),
        Some("a3c9e7d1-0b2f-4d6e-9a8b-1c2d3e4f5a6b")
    );
}

#[test]
fn source_other_than_new_is_kept() {
    let input = parse_value(json!({
        "hook_event_name": "SessionStart",
        "session_id": SESSION,
        "source": "resume",
    }));
    assert_eq!(input.source.as_deref(), Some("resume"));
}

#[test]
fn empty_event_name_takes_the_event_argument() {
    let v = json!({"session_id": SESSION, "hook_event_name": ""});
    let input = parse(Some("Stop"), v).expect("parses");
    assert_eq!(input.hook_event_name, "Stop");

    let v = json!({"session_id": SESSION, "hook_event_name": "PostToolUse"});
    let input = parse(Some("Stop"), v).expect("parses");
    assert_eq!(input.hook_event_name, "PostToolUse", "a present name wins");
}

#[test]
fn string_tool_response_is_kept_as_a_string() {
    let input = parse_value(json!({
        "hook_event_name": "PostToolUse",
        "session_id": SESSION,
        "tool_response": "plain text result",
    }));
    assert_eq!(input.tool_response, Some(json!("plain text result")));
}

#[test]
fn non_object_payloads_are_input_errors() {
    for v in [
        json!("just a string"),
        json!([1, 2, 3]),
        json!(42),
        json!(null),
    ] {
        let err: HookError = parse(None, v.clone()).expect_err("non-object must not parse");
        assert!(matches!(err, HookError::Input(_)), "{v} gave {err:?}");
    }
}

#[test]
fn null_and_untyped_fields_never_fail_the_payload() {
    let input = parse_value(json!({
        "hook_event_name": "Stop",
        "session_id": SESSION,
        "timestamp": null,
        "stop_hook_active": null,
        "model": 7,
    }));
    assert_eq!(input.session_id, SESSION);
    assert!(!input.stop_hook_active);
}

#[test]
fn wrong_typed_read_fields_read_as_absent() {
    let input = parse(
        Some("Stop"),
        json!({
            "hook_event_name": 3,
            "session_id": 42,
            "cwd": ["x"],
            "transcript_path": null,
            "source": {},
            "tool_name": false,
            "agent_type": 1,
            "stop_hook_active": "yes",
        }),
    )
    .expect("parses");
    assert_eq!(
        input.hook_event_name, "Stop",
        "a non-string name takes the event"
    );
    assert_eq!(
        input.session_id, "",
        "an empty session is ignored by the dialect guard"
    );
    assert_eq!(input.cwd, None);
    assert_eq!(input.transcript_path, None);
    assert_eq!(input.source, None);
    assert_eq!(input.tool_name, None);
    assert_eq!(input.agent_type, None);
    assert!(!input.stop_hook_active);
}

#[test]
fn agora_mcp_tool_name_takes_the_claude_form() {
    let input = parse_value(json!({
        "hook_event_name": "PostToolUse",
        "session_id": SESSION,
        "tool_name": "mcp_agora_inbox",
    }));
    let name = input.tool_name.as_deref().expect("tool name");
    assert_eq!(name, "mcp__agora__inbox");
    assert!(crate::context::is_inbox_tool(name));
}

#[test]
fn agora_prefix_without_a_tool_is_kept() {
    let input = parse_value(json!({
        "hook_event_name": "PostToolUse",
        "session_id": SESSION,
        "tool_name": "mcp_agora_",
    }));
    assert_eq!(input.tool_name.as_deref(), Some("mcp_agora_"));
}

#[test]
fn other_tool_names_are_kept() {
    let input = parse_value(json!({
        "hook_event_name": "PostToolUse",
        "session_id": SESSION,
        "tool_name": "run_in_terminal",
    }));
    assert_eq!(input.tool_name.as_deref(), Some("run_in_terminal"));
}

#[test]
fn session_start_context_renders_with_its_event() {
    let input = parse_fixture(SESSION_START);
    let out = render_json(HookKind::SessionStart, &input, &out_context("hello"));
    assert_eq!(
        out,
        json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "hello"}})
    );
}

#[test]
fn user_prompt_and_post_tool_context_render_with_their_event() {
    let input = parse_fixture(USER_PROMPT_SUBMIT);
    let out = render_json(HookKind::Context, &input, &out_context("ctx"));
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );

    let input = parse_fixture(POST_TOOL_USE);
    let out = render_json(HookKind::Context, &input, &out_context("ctx"));
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], "ctx");
}

#[test]
fn stop_block_renders_both_shapes() {
    let input = parse_fixture(STOP);
    let out = render_json(HookKind::Stop, &input, &out_block("keep going"));
    assert_eq!(
        out,
        json!({
            "decision": "block",
            "reason": "keep going",
            "hookSpecificOutput": {
                "hookEventName": "Stop",
                "decision": "block",
                "reason": "keep going",
            },
        })
    );
}

#[test]
fn mismatched_renders_are_silent() {
    // A context on a Stop, a block on a context event, and a context on SubagentStop.
    let stop = parse_fixture(STOP);
    assert_eq!(render(HookKind::Stop, &stop, &out_context("x")), "");

    let start = parse_fixture(SESSION_START);
    assert_eq!(render(HookKind::SessionStart, &start, &out_block("x")), "");

    let sub = parse_fixture(SUBAGENT_STOP);
    assert_eq!(render(HookKind::Stop, &sub, &out_context("x")), "");
    assert_eq!(render(HookKind::Stop, &sub, &out_block("x")), "");

    // An unknown event name renders nothing, even with both outputs.
    let other = parse_value(json!({"hook_event_name": "Notification", "session_id": SESSION}));
    let both = ClaudeOut {
        context: Some("x".into()),
        context_event: None,
        block_reason: Some("y".into()),
    };
    assert_eq!(render(HookKind::Context, &other, &both), "");
}

#[test]
fn empty_claude_output_is_silent() {
    let input = parse_fixture(STOP);
    assert_eq!(render(HookKind::Stop, &input, &ClaudeOut::default()), "");
}

#[test]
fn block_takes_precedence_over_context_on_stop() {
    let input = parse_fixture(STOP);
    let both = ClaudeOut {
        context: Some("ctx".into()),
        context_event: None,
        block_reason: Some("keep going".into()),
    };
    let out = render_json(HookKind::Stop, &input, &both);
    assert_eq!(out["decision"], "block");
    assert!(
        out.get("hookSpecificOutput")
            .and_then(|h| h.get("additionalContext"))
            .is_none()
    );
}
