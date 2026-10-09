//! Unit tests for the Copilot dialect. The `hook_*` payloads are REAL Copilot CLI 0.0.369 captures
//! (no session id); the `newer_*` payloads are DOCS-DERIVED (see the README in
//! `tests/fixtures/copilot/`). The VS Code payload is DOCS-DERIVED too (see its README).

use serde_json::{Value, json};

use super::{parse, render};
use crate::dialect::{ClaudeOut, Dialect, to_output, vscode};
use crate::schema::{HookEvent, HookInput, context_json};
use crate::{HookKind, HookOutcome};

const REAL_SESSION_START: &str =
    include_str!("../../../tests/fixtures/copilot/hook_sessionStart.json");
const REAL_POST_TOOL_USE: &str =
    include_str!("../../../tests/fixtures/copilot/hook_postToolUse.json");
const REAL_USER_PROMPT: &str =
    include_str!("../../../tests/fixtures/copilot/hook_userPromptSubmitted.json");
const REAL_SESSION_END: &str = include_str!("../../../tests/fixtures/copilot/hook_sessionEnd.json");
const REAL_PRE_TOOL_USE: &str =
    include_str!("../../../tests/fixtures/copilot/hook_preToolUse.json");
const REAL_ERROR_OCCURRED: &str =
    include_str!("../../../tests/fixtures/copilot/hook_errorOccurred.json");

const NEWER_SESSION_START: &str =
    include_str!("../../../tests/fixtures/copilot/newer_sessionStart.json");
const NEWER_POST_TOOL_USE: &str =
    include_str!("../../../tests/fixtures/copilot/newer_postToolUse.json");
const NEWER_AGENT_STOP: &str = include_str!("../../../tests/fixtures/copilot/newer_agentStop.json");
const NEWER_AGENT_STOP_ACTIVE: &str =
    include_str!("../../../tests/fixtures/copilot/newer_agentStop_active.json");

const VSCODE_STOP: &str = include_str!("../../../tests/fixtures/vscode/Stop.json");

/// `cwd` of every 0.0.369 capture.
const REAL_CWD: &str = "/private/tmp/agora-m6/probe-copilot2/proj";
/// `sessionId` of the newer payloads.
const SESSION: &str = "7d3e1f0a-2b4c-4d5e-9f60-1a2b3c4d5e6f";

/// The real 0.0.369 captures, each with the native event the hook command passes as `--event`.
const REAL_CASES: [(&str, &str); 6] = [
    ("sessionStart", REAL_SESSION_START),
    ("postToolUse", REAL_POST_TOOL_USE),
    ("userPromptSubmitted", REAL_USER_PROMPT),
    ("sessionEnd", REAL_SESSION_END),
    ("preToolUse", REAL_PRE_TOOL_USE),
    ("errorOccurred", REAL_ERROR_OCCURRED),
];

fn value(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is JSON")
}

fn parsed(event: &str, raw: &str) -> HookInput {
    parse(Some(event), value(raw)).expect("fixture parses")
}

fn context(text: &str) -> ClaudeOut {
    ClaudeOut {
        context: Some(text.into()),
        ..ClaudeOut::default()
    }
}

fn block(reason: &str) -> ClaudeOut {
    ClaudeOut {
        block_reason: Some(reason.into()),
        ..ClaudeOut::default()
    }
}

#[test]
fn real_0_0_369_payloads_map_to_ignored_legacy_events() {
    for (event, raw) in REAL_CASES {
        let input = parsed(event, raw);
        assert_eq!(input.hook_event_name, format!("copilot:{event}"), "{event}");
        assert_eq!(
            HookEvent::parse(&input.hook_event_name),
            HookEvent::Other,
            "{event}"
        );
        assert_eq!(input.cwd.as_deref(), Some(REAL_CWD), "{event}");
        assert!(input.session_id.is_empty(), "{event}");
        assert_eq!(input.origin.as_deref(), Some("copilot-cli"), "{event}");
    }
}

#[test]
fn real_0_0_369_payloads_render_nothing_even_with_output() {
    for (event, raw) in REAL_CASES {
        let input = parsed(event, raw);
        assert_eq!(
            render(HookKind::SessionStart, &input, &context("x")),
            "",
            "{event}"
        );
        assert_eq!(render(HookKind::Stop, &input, &block("x")), "", "{event}");
    }
}

#[test]
fn newer_session_start_maps_to_session_start() {
    let input = parsed("sessionStart", NEWER_SESSION_START);
    assert_eq!(input.hook_event_name, "SessionStart");
    assert_eq!(input.session_id, SESSION);
    assert_eq!(input.source.as_deref(), Some("startup"));
    assert_eq!(input.cwd.as_deref(), Some("/Users/me/proj"));
    assert_eq!(input.origin.as_deref(), Some("copilot-cli"));
}

#[test]
fn newer_agora_inbox_maps_to_the_inbox_tool_name() {
    let input = parsed("postToolUse", NEWER_POST_TOOL_USE);
    assert_eq!(input.hook_event_name, "PostToolUse");
    assert_eq!(input.tool_name.as_deref(), Some("mcp__agora__inbox"));
    assert!(
        input
            .tool_name
            .as_deref()
            .is_some_and(crate::context::is_inbox_tool)
    );
    assert_eq!(
        input.tool_response,
        Some(value(NEWER_POST_TOOL_USE)["toolResult"].clone())
    );
}

#[test]
fn tool_names_normalize_only_for_the_agora_server() {
    let cases = [
        ("logger-ping", "logger-ping"),
        ("agora-inbox_wait", "mcp__agora__inbox_wait"),
        ("agora-", "agora-"),
        ("agora", "agora"),
    ];
    for (sent, want) in cases {
        let raw = json!({"sessionId": SESSION, "toolName": sent}).to_string();
        let input = parse(Some("postToolUse"), value(&raw)).expect("parses");
        assert_eq!(input.tool_name.as_deref(), Some(want), "{sent}");
    }
}

#[test]
fn newer_agent_stop_maps_to_stop_and_reads_stop_hook_active() {
    let idle = parsed("agentStop", NEWER_AGENT_STOP);
    assert_eq!(idle.hook_event_name, "Stop");
    assert!(!idle.stop_hook_active);
    assert!(idle.transcript_path.is_some());

    let active = parsed("agentStop", NEWER_AGENT_STOP_ACTIVE);
    assert_eq!(active.hook_event_name, "Stop");
    assert!(active.stop_hook_active);
}

#[test]
fn camel_case_stop_hook_active_alias_is_read() {
    let camel = json!({"sessionId": SESSION, "stopHookActive": true}).to_string();
    let snake = json!({"sessionId": SESSION, "stop_hook_active": true}).to_string();
    for raw in [camel, snake] {
        let input = parse(Some("agentStop"), value(&raw)).expect("parses");
        assert!(input.stop_hook_active, "{raw}");
    }
}

#[test]
fn subagent_stop_and_other_native_events_with_a_session() {
    let sub = parse(Some("subagentStop"), json!({"sessionId": SESSION})).expect("parses");
    assert_eq!(sub.hook_event_name, "SubagentStop");

    for native in ["userPromptSubmitted", "preToolUse", "errorOccurred"] {
        let input = parse(Some(native), json!({"sessionId": SESSION})).expect("parses");
        assert_eq!(
            input.hook_event_name,
            format!("copilot:{native}"),
            "{native}"
        );
    }
}

#[test]
fn null_and_wrong_typed_fields_do_not_silence_the_payload() {
    let raw = json!({
        "sessionId": "s1",
        "timestamp": null,
        "toolArgs": 7,
        "stop_hook_active": null
    })
    .to_string();
    let input = parse(Some("agentStop"), value(&raw)).expect("parses");
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.session_id, "s1");
    assert!(!input.stop_hook_active);
}

#[test]
fn missing_event_argument_is_ignored() {
    let input = parse(None, json!({"sessionId": SESSION})).expect("parses");
    assert_eq!(HookEvent::parse(&input.hook_event_name), HookEvent::Other);
}

#[test]
fn vscode_payload_is_routed_to_the_vscode_parser() {
    let input = parse(Some("Stop"), value(VSCODE_STOP)).expect("parses");
    assert_eq!(input.origin.as_deref(), Some("vscode"));
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.session_id, "5c1e8a2f-3b7d-4e9a-8f10-2d6c4b7a9e31");
    assert_eq!(
        input,
        vscode::parse(Some("Stop"), value(VSCODE_STOP)).expect("parses")
    );
}

#[test]
fn vscode_render_uses_the_vscode_stop_shape() {
    let input = parse(Some("Stop"), value(VSCODE_STOP)).expect("parses");
    let out = block("unread mail");
    let got = render(HookKind::Stop, &input, &out);
    assert_eq!(got, vscode::render(HookKind::Stop, &input, &out));

    let shape: Value = serde_json::from_str(&got).expect("render is JSON");
    assert_eq!(shape["decision"], "block");
    assert_eq!(shape["reason"], "unread mail");
    assert_eq!(shape["hookSpecificOutput"]["hookEventName"], "Stop");
    assert_eq!(shape["hookSpecificOutput"]["decision"], "block");
}

#[test]
fn cli_render_shapes_for_newer_payloads() {
    let start = parsed("sessionStart", NEWER_SESSION_START);
    assert_eq!(
        render(HookKind::SessionStart, &start, &context("hi")),
        r#"{"additionalContext":"hi"}"#
    );

    let post = parsed("postToolUse", NEWER_POST_TOOL_USE);
    assert_eq!(
        render(HookKind::Context, &post, &context("hi")),
        r#"{"additionalContext":"hi"}"#
    );

    let stop = parsed("agentStop", NEWER_AGENT_STOP);
    assert_eq!(
        render(HookKind::Stop, &stop, &block("unread")),
        r#"{"decision":"block","reason":"unread"}"#
    );
}

#[test]
fn cli_render_is_silent_where_the_cli_cannot_show_output() {
    let start = parsed("sessionStart", NEWER_SESSION_START);
    assert_eq!(render(HookKind::Stop, &start, &block("x")), "");

    let stop = parsed("agentStop", NEWER_AGENT_STOP);
    assert_eq!(render(HookKind::Stop, &stop, &context("x")), "");

    let prompt = parse(Some("userPromptSubmitted"), json!({"sessionId": SESSION})).expect("parses");
    assert_eq!(render(HookKind::Context, &prompt, &context("x")), "");
}

#[test]
fn copilot_cli_to_output_turns_the_entry_output_into_native_json() {
    let input = parsed("postToolUse", NEWER_POST_TOOL_USE);
    let out = to_output(
        Dialect::Copilot,
        HookKind::Context,
        &input,
        HookOutcome::stdout(context_json("PostToolUse", "hi")),
    );
    assert_eq!(
        out,
        HookOutcome::stdout(r#"{"additionalContext":"hi"}"#.into())
    );
}
