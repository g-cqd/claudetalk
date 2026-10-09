//! Unit tests for the Cursor dialect on the DOCS-DERIVED payloads in `tests/fixtures/cursor/`
//! (not captured from a Cursor binary; see the README there).

use serde_json::{Value, json};

use super::{LOOP_LIMIT, parse, render};
use crate::context::is_inbox_tool;
use crate::dialect::{ClaudeOut, Dialect, to_input, to_output};
use crate::schema::{HookInput, block_json, context_json};
use crate::{HookError, HookKind, HookOutcome};

const SESSION_START: &str = include_str!("../../../tests/fixtures/cursor/sessionStart.json");
const POST_TOOL_USE: &str = include_str!("../../../tests/fixtures/cursor/postToolUse.json");
const AFTER_MCP: &str = include_str!("../../../tests/fixtures/cursor/afterMCPExecution.json");
const STOP: &str = include_str!("../../../tests/fixtures/cursor/stop.json");
const STOP_LOOP1: &str = include_str!("../../../tests/fixtures/cursor/stop_loop1.json");
const BEFORE_SUBMIT: &str = include_str!("../../../tests/fixtures/cursor/beforeSubmitPrompt.json");

const CONVERSATION: &str = "7c1e0a52-3b9d-4f6e-8a21-5d0c9b7e4f10";
const CWD: &str = "/Users/me/proj";

fn value(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is JSON")
}

fn parse_fixture(raw: &str) -> HookInput {
    parse(None, value(raw)).expect("fixture parses")
}

fn with_event(name: &str) -> HookInput {
    HookInput {
        hook_event_name: name.to_owned(),
        ..HookInput::default()
    }
}

#[test]
fn every_fixture_parses_with_the_cursor_origin() {
    for raw in [
        SESSION_START,
        POST_TOOL_USE,
        AFTER_MCP,
        STOP,
        STOP_LOOP1,
        BEFORE_SUBMIT,
    ] {
        let input = parse_fixture(raw);
        assert_eq!(input.origin.as_deref(), Some("cursor"));
        assert_eq!(input.session_id, CONVERSATION);
        assert_eq!(input.cwd.as_deref(), Some(CWD));
        assert_eq!(input.transcript_path, None);
    }
}

#[test]
fn session_start_maps_to_session_start() {
    let input = parse_fixture(SESSION_START);
    assert_eq!(input.hook_event_name, "SessionStart");
    assert_eq!(input.tool_name, None);
    assert!(!input.stop_hook_active);
}

#[test]
fn post_tool_use_keeps_the_builtin_tool_name_and_output() {
    let input = parse_fixture(POST_TOOL_USE);
    assert_eq!(input.hook_event_name, "PostToolUse");
    assert_eq!(input.tool_name.as_deref(), Some("Shell"));
    assert_eq!(
        input.tool_response,
        Some(Value::String(
            "{\"exitCode\":0,\"stdout\":\"README.md\\n\"}".into()
        ))
    );
}

#[test]
fn after_mcp_execution_is_an_inbox_post_tool_use() {
    let input = parse_fixture(AFTER_MCP);
    assert_eq!(input.hook_event_name, "PostToolUse");
    assert_eq!(input.tool_name.as_deref(), Some("mcp__agora__inbox"));
    assert!(is_inbox_tool(
        input.tool_name.as_deref().unwrap_or_default()
    ));
    assert_eq!(input.tool_response, Some(json!({"messages": []})));
}

#[test]
fn after_mcp_execution_keeps_a_non_json_result_as_a_string() {
    let mut v = value(AFTER_MCP);
    v["result_json"] = json!("not json");
    let input = parse(None, v).expect("parses");
    assert_eq!(input.tool_response, Some(json!("not json")));
}

#[test]
fn after_mcp_execution_without_result_json_has_no_response() {
    let mut v = value(AFTER_MCP);
    v.as_object_mut().expect("object").remove("result_json");
    let input = parse(None, v).expect("parses");
    assert_eq!(input.tool_response, None);
}

#[test]
fn incomplete_mcp_payload_is_never_an_inbox_read() {
    let mut v = value(AFTER_MCP);
    v.as_object_mut().expect("object").remove("mcp_server_name");
    let input = parse(None, v).expect("parses");
    assert_eq!(input.hook_event_name, "cursor:afterMCPExecution");
    assert_eq!(input.tool_name, None);
}

#[test]
fn mcp_tool_without_a_server_is_never_an_inbox_read_even_with_an_inbox_suffix() {
    let mut v = value(AFTER_MCP);
    v.as_object_mut().expect("object").remove("mcp_server_name");
    v["tool_name"] = json!("other__inbox");
    let input = parse(None, v).expect("parses");
    assert_eq!(input.hook_event_name, "cursor:afterMCPExecution");
}

#[test]
fn non_inbox_mcp_tool_is_an_ignored_native_event() {
    let mut v = value(AFTER_MCP);
    v["tool_name"] = json!("chat");
    let input = parse(None, v).expect("parses");
    assert_eq!(input.hook_event_name, "cursor:afterMCPExecution");
    assert_eq!(
        crate::schema::HookEvent::parse(&input.hook_event_name),
        crate::schema::HookEvent::Other
    );
}

#[test]
fn stop_below_the_loop_limit_is_not_already_active() {
    for raw in [STOP, STOP_LOOP1] {
        let input = parse_fixture(raw);
        assert_eq!(input.hook_event_name, "Stop");
        assert!(!input.stop_hook_active);
    }
}

#[test]
fn stop_is_already_active_from_the_loop_limit_on() {
    let active_at = |count: i64| {
        let mut v = value(STOP);
        v["loop_count"] = json!(count);
        parse(None, v).expect("parses").stop_hook_active
    };
    assert!(!active_at(LOOP_LIMIT - 1));
    assert!(active_at(LOOP_LIMIT));
    assert!(active_at(LOOP_LIMIT + 1));
}

#[test]
fn inbox_name_from_another_mcp_server_is_an_ordinary_tool() {
    for server in ["other", "Agora", "agora_x", ""] {
        let mut v = value(AFTER_MCP);
        v["mcp_server_name"] = json!(server);
        let input = parse(None, v).expect("parses");
        assert_eq!(
            input.hook_event_name, "cursor:afterMCPExecution",
            "{server}"
        );
        assert_eq!(input.tool_name, None, "{server}");
        assert_eq!(input.tool_response, None, "{server}");
    }
}

#[test]
fn before_submit_prompt_is_an_ignored_native_event() {
    let input = parse_fixture(BEFORE_SUBMIT);
    assert_eq!(input.hook_event_name, "cursor:beforeSubmitPrompt");
    assert_eq!(
        crate::schema::HookEvent::parse(&input.hook_event_name),
        crate::schema::HookEvent::Other
    );
}

#[test]
fn subagent_stop_and_other_events_are_prefixed() {
    let sub = parse(
        None,
        json!({"conversation_id": CONVERSATION, "hook_event_name": "subagentStop"}),
    )
    .expect("parses");
    assert_eq!(sub.hook_event_name, "cursor:subagentStop");
    let other = parse(None, json!({"hook_event_name": "afterFileEdit"})).expect("parses");
    assert_eq!(other.hook_event_name, "cursor:afterFileEdit");
}

#[test]
fn payload_event_wins_over_the_event_argument_and_the_argument_fills_a_gap() {
    let filled = parse(
        Some("sessionStart"),
        json!({"conversation_id": CONVERSATION}),
    )
    .expect("parses");
    assert_eq!(filled.hook_event_name, "SessionStart");
    let payload = parse(Some("sessionStart"), json!({"hook_event_name": "stop"})).expect("parses");
    assert_eq!(payload.hook_event_name, "Stop");
    let empty = parse(None, json!({"hook_event_name": ""})).expect("parses");
    assert_eq!(empty.hook_event_name, "cursor:");
}

#[test]
fn conversation_id_wins_and_session_id_is_the_fallback() {
    let both = parse(
        None,
        json!({"conversation_id": "conv", "session_id": "sess", "hook_event_name": "sessionStart"}),
    )
    .expect("parses");
    assert_eq!(both.session_id, "conv");
    let fallback = parse(
        None,
        json!({"conversation_id": "", "session_id": "sess", "hook_event_name": "sessionStart"}),
    )
    .expect("parses");
    assert_eq!(fallback.session_id, "sess");
}

#[test]
fn cwd_is_the_first_workspace_root_and_missing_roots_give_none() {
    let input = parse(
        None,
        json!({"workspace_roots": ["/a", "/b"], "hook_event_name": "stop"}),
    )
    .expect("parses");
    assert_eq!(input.cwd.as_deref(), Some("/a"));
    let none = parse(None, json!({"hook_event_name": "stop"})).expect("parses");
    assert_eq!(none.cwd, None);
}

#[test]
fn non_object_payloads_are_input_errors() {
    let cases = [json!("text"), json!(42), json!(null), json!([1, 2])];
    for case in cases {
        assert!(
            matches!(parse(None, case.clone()), Err(HookError::Input(_))),
            "{case}"
        );
    }
}

#[test]
fn null_and_wrong_typed_fields_are_absent_not_fatal() {
    let input = parse(
        None,
        json!({
            "conversation_id": CONVERSATION,
            "hook_event_name": "stop",
            "loop_count": null,
            "workspace_roots": null,
            "model": 5,
            "transcript_path": 7,
        }),
    )
    .expect("parses");
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.session_id, CONVERSATION);
    assert_eq!(input.cwd, None);
    assert_eq!(input.transcript_path, None);
    let wrong = parse(
        None,
        json!({"conversation_id": CONVERSATION, "hook_event_name": 5, "workspace_roots": "/x"}),
    )
    .expect("parses");
    assert_eq!(wrong.hook_event_name, "cursor:");
    assert_eq!(wrong.cwd, None);
}

#[test]
fn bom_prefixed_fixture_reaches_the_parser_through_to_input() {
    let mut raw = b"\xEF\xBB\xBF".to_vec();
    raw.extend_from_slice(SESSION_START.as_bytes());
    let input = to_input(Dialect::Cursor, None, &raw).expect("BOM stripped, parses");
    assert_eq!(input.hook_event_name, "SessionStart");
}

#[test]
fn context_renders_additional_context_on_session_start_and_post_tool_use() {
    let out = ClaudeOut {
        context: Some("hello <b>".into()),
        ..ClaudeOut::default()
    };
    for event in ["SessionStart", "PostToolUse"] {
        let got = render(HookKind::Context, &with_event(event), &out);
        assert_eq!(
            got,
            json!({"additional_context": "hello <b>"}).to_string(),
            "{event}"
        );
    }
}

#[test]
fn block_renders_followup_message_on_stop() {
    let out = ClaudeOut {
        block_reason: Some("unread mail".into()),
        ..ClaudeOut::default()
    };
    let got = render(HookKind::Stop, &with_event("Stop"), &out);
    assert_eq!(got, json!({"followup_message": "unread mail"}).to_string());
}

#[test]
fn mismatched_shapes_and_ignored_events_render_silence() {
    let context = ClaudeOut {
        context: Some("x".into()),
        ..ClaudeOut::default()
    };
    let block = ClaudeOut {
        block_reason: Some("y".into()),
        ..ClaudeOut::default()
    };
    let silent = [
        (with_event("Stop"), &context),
        (with_event("SessionStart"), &block),
        (with_event("PostToolUse"), &block),
        (with_event("cursor:beforeSubmitPrompt"), &context),
        (with_event("cursor:beforeSubmitPrompt"), &block),
        (with_event("SubagentStop"), &block),
    ];
    for (input, out) in &silent {
        assert_eq!(
            render(HookKind::Context, input, out),
            "",
            "{}",
            input.hook_event_name
        );
    }
    assert_eq!(
        render(HookKind::Stop, &with_event("Stop"), &ClaudeOut::default()),
        ""
    );
}

#[test]
fn to_output_turns_the_entry_point_block_into_a_followup_message() {
    let input = parse_fixture(STOP);
    let out = to_output(
        Dialect::Cursor,
        HookKind::Stop,
        &input,
        HookOutcome::stdout(block_json("why")),
    );
    assert_eq!(
        out,
        HookOutcome::stdout(json!({"followup_message": "why"}).to_string())
    );
    let ctx = to_output(
        Dialect::Cursor,
        HookKind::Context,
        &parse_fixture(POST_TOOL_USE),
        HookOutcome::stdout(context_json("PostToolUse", "ctx")),
    );
    assert_eq!(ctx.stdout, json!({"additional_context": "ctx"}).to_string());
}
