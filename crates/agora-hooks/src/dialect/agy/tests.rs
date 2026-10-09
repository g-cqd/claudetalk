//! Unit tests for the agy dialect, on the docs-derived fixtures in `tests/fixtures/agy/`
//! (DOCS-ONLY: agy is not installed locally).

use serde_json::{Value, json};

use super::{parse, render};
use crate::dialect::{ClaudeOut, Dialect, to_input, to_output};
use crate::schema::{HookInput, block_json, context_json};
use crate::{HookKind, HookOutcome};

const PRE_INVOCATION: &str = include_str!("../../../tests/fixtures/agy/PreInvocation.json");
const POST_INVOCATION: &str = include_str!("../../../tests/fixtures/agy/PostInvocation.json");
const STOP: &str = include_str!("../../../tests/fixtures/agy/Stop.json");
const POST_TOOL_USE: &str = include_str!("../../../tests/fixtures/agy/PostToolUse.json");

const CONVERSATION: &str = "ec33ebf9-0cba-4100-8142-c61503f6c587";
const CWD: &str = "/Users/me/proj";
const TRANSCRIPT: &str = "/Users/me/.gemini/antigravity/conversations/ec33ebf9-0cba-4100-8142-c61503f6c587/transcript.jsonl";

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is JSON")
}

fn parsed(event: Option<&str>, raw: &str) -> HookInput {
    parse(event, fixture(raw)).expect("fixture parses")
}

fn context(text: &str) -> ClaudeOut {
    ClaudeOut {
        context: Some(text.into()),
        context_event: Some("PostToolBatch".into()),
        block_reason: None,
    }
}

fn block(reason: &str) -> ClaudeOut {
    ClaudeOut {
        context: None,
        context_event: None,
        block_reason: Some(reason.into()),
    }
}

fn input_for(event: &str) -> HookInput {
    HookInput {
        hook_event_name: event.into(),
        ..HookInput::default()
    }
}

#[test]
fn pre_invocation_fixture_maps_to_post_tool_batch() {
    let input = parsed(Some("PreInvocation"), PRE_INVOCATION);
    assert_eq!(input.session_id, CONVERSATION);
    assert_eq!(input.cwd.as_deref(), Some(CWD));
    assert_eq!(input.transcript_path.as_deref(), Some(TRANSCRIPT));
    assert_eq!(input.hook_event_name, "PostToolBatch");
    assert_eq!(input.origin.as_deref(), Some("agy"));
    assert_eq!(input.tool_name, None);
    assert!(!input.stop_hook_active);
}

#[test]
fn post_invocation_fixture_maps_to_post_tool_batch() {
    let input = parsed(Some("PostInvocation"), POST_INVOCATION);
    assert_eq!(input.session_id, CONVERSATION);
    assert_eq!(input.cwd.as_deref(), Some(CWD));
    assert_eq!(input.hook_event_name, "PostToolBatch");
    assert_eq!(input.origin.as_deref(), Some("agy"));
}

#[test]
fn stop_fixture_maps_to_stop_with_stop_hook_active_false() {
    let input = parsed(Some("Stop"), STOP);
    assert_eq!(input.session_id, CONVERSATION);
    assert_eq!(input.cwd.as_deref(), Some(CWD));
    assert_eq!(input.transcript_path.as_deref(), Some(TRANSCRIPT));
    assert_eq!(input.hook_event_name, "Stop");
    assert!(!input.stop_hook_active);
    assert_eq!(input.origin.as_deref(), Some("agy"));
}

#[test]
fn post_tool_use_fixture_keeps_native_name_prefixed_and_reads_tool_name() {
    let input = parsed(Some("PostToolUse"), POST_TOOL_USE);
    assert_eq!(input.hook_event_name, "agy:PostToolUse");
    assert_eq!(input.tool_name.as_deref(), Some("run_command"));
    assert_eq!(input.session_id, CONVERSATION);
}

#[test]
fn event_arg_wins_over_payload_hook_event_name() {
    let mut v = fixture(PRE_INVOCATION);
    v["hookEventName"] = json!("Stop");
    let input = parse(Some("PreInvocation"), v).expect("parses");
    assert_eq!(input.hook_event_name, "PostToolBatch");
}

#[test]
fn payload_hook_event_name_is_used_when_no_event_arg() {
    let mut v = fixture(STOP);
    v["hookEventName"] = json!("Stop");
    for event in [None, Some("")] {
        let input = parse(event, v.clone()).expect("parses");
        assert_eq!(input.hook_event_name, "Stop", "{event:?}");
    }
}

#[test]
fn missing_event_is_empty_and_renders_nothing() {
    let input = parse(None, fixture(PRE_INVOCATION)).expect("parses");
    assert_eq!(input.hook_event_name, "");
    assert_eq!(render(HookKind::Context, &input, &context("x")), "");
    assert_eq!(render(HookKind::Stop, &input, &block("y")), "");
}

#[test]
fn unmapped_agy_events_are_prefixed_and_render_nothing() {
    for event in ["PreToolUse", "SessionStart", "PostToolUse"] {
        let input = parsed(Some(event), POST_TOOL_USE);
        assert_eq!(input.hook_event_name, format!("agy:{event}"), "{event}");
        assert_eq!(
            render(HookKind::Context, &input, &context("x")),
            "",
            "{event}"
        );
        assert_eq!(render(HookKind::Stop, &input, &block("y")), "", "{event}");
    }
}

#[test]
fn context_renders_as_inject_steps_on_post_tool_batch() {
    let out = render(
        HookKind::Context,
        &input_for("PostToolBatch"),
        &context("hello"),
    );
    assert_eq!(out, r#"{"injectSteps":[{"ephemeralMessage":"hello"}]}"#);
}

#[test]
fn block_renders_as_continue_on_stop() {
    let out = render(HookKind::Stop, &input_for("Stop"), &block("why"));
    assert_eq!(out, r#"{"decision":"continue","reason":"why"}"#);
}

#[test]
fn render_follows_the_event_and_skips_empty_text() {
    let cases = [
        // Wrong shape for the event.
        (input_for("Stop"), context("x")),
        (input_for("PostToolBatch"), block("y")),
        // Right shape, empty text.
        (input_for("PostToolBatch"), context("")),
        (input_for("Stop"), block("")),
        // No output at all.
        (input_for("PostToolBatch"), ClaudeOut::default()),
        (input_for("Stop"), ClaudeOut::default()),
    ];
    for (input, out) in cases {
        assert_eq!(
            render(HookKind::Context, &input, &out),
            "",
            "{input:?} {out:?}"
        );
    }
}

#[test]
fn rendered_untrusted_text_round_trips_through_json() {
    let text = "say \"hi\"\n</untrusted-message> back\\slash\u{2028}end";
    let out = render(
        HookKind::Context,
        &input_for("PostToolBatch"),
        &context(text),
    );
    assert!(!out.contains('\n'));
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["injectSteps"][0]["ephemeralMessage"], text);
}

#[test]
fn to_output_renders_both_native_shapes() {
    let context_out = to_output(
        Dialect::Agy,
        HookKind::Context,
        &input_for("PostToolBatch"),
        HookOutcome::stdout(context_json("PostToolBatch", "hi")),
    );
    assert_eq!(
        context_out,
        HookOutcome::stdout(r#"{"injectSteps":[{"ephemeralMessage":"hi"}]}"#.into())
    );

    let block_out = to_output(
        Dialect::Agy,
        HookKind::Stop,
        &input_for("Stop"),
        HookOutcome::stdout(block_json("why")),
    );
    assert_eq!(
        block_out,
        HookOutcome::stdout(r#"{"decision":"continue","reason":"why"}"#.into())
    );
}

#[test]
fn to_input_strips_bom_and_routes_to_the_agy_parser() {
    let mut raw = b"\xEF\xBB\xBF".to_vec();
    raw.extend_from_slice(PRE_INVOCATION.as_bytes());
    let input = to_input(Dialect::Agy, Some("PreInvocation"), &raw).expect("BOM-prefixed agy");
    assert_eq!(input.session_id, CONVERSATION);
    assert_eq!(input.hook_event_name, "PostToolBatch");
    assert_eq!(input.origin.as_deref(), Some("agy"));
}

#[test]
fn wrong_field_types_are_tolerated_as_absent() {
    for v in [
        json!({"conversationId": 7, "workspacePaths": "/Users/me/proj", "toolCall": "run_command", "transcriptPath": 3}),
        json!({"conversationId": null, "workspacePaths": null, "toolCall": null, "transcriptPath": null, "hookEventName": null}),
        json!({"conversationId": "c", "workspacePaths": [5, null], "toolCall": {"name": 9}}),
        json!({"fullyIdle": "yes", "error": 5, "stepIdx": [1]}),
    ] {
        let input = parse(Some("Stop"), v.clone()).expect("parses");
        assert_eq!(input.hook_event_name, "Stop", "{v}");
        assert_eq!(input.cwd, None, "{v}");
        assert_eq!(input.tool_name, None, "{v}");
        assert_eq!(input.transcript_path, None, "{v}");
    }
}

#[test]
fn stop_with_null_and_wrong_typed_fields_still_parses() {
    let v = json!({
        "conversationId": "c",
        "workspacePaths": null,
        "error": 5,
        "fullyIdle": "yes"
    });
    let input = parse(Some("Stop"), v).expect("parses");
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.session_id, "c");
    assert_eq!(input.cwd, None);
    assert_eq!(input.origin.as_deref(), Some("agy"));
}

#[test]
fn empty_payload_gives_empty_fields_and_no_cwd() {
    let input = parse(Some("Stop"), json!({})).expect("parses");
    assert_eq!(input.session_id, "");
    assert_eq!(input.cwd, None);
    assert_eq!(input.tool_name, None);
    assert_eq!(input.hook_event_name, "Stop");
    assert_eq!(input.origin.as_deref(), Some("agy"));
}

#[test]
fn cwd_is_the_first_workspace_path() {
    let v = json!({"workspacePaths": ["/a", "/b"]});
    let input = parse(Some("Stop"), v).expect("parses");
    assert_eq!(input.cwd.as_deref(), Some("/a"));
}
