//! `agora hook context`: UserPromptSubmit, PostToolUse and PostToolBatch.

use crate::daemon::{ctx_for, fetch, heartbeat, resolve_folder, seq_of};
use crate::render::render_messages;
use crate::schema::{HookEvent, HookInput, context_json};
use crate::state::SessionLock;
use crate::{HookError, HookOptions, HookOutcome};
use agora_deliver::Mechanism;

/// Heartbeat `hook_inject` once the folder and identity resolve. `SessionStart`, `Stop` and
/// unknown events -> silent, with no work at all. `SubagentStop` -> silent after a best-effort
/// heartbeat only: its `additionalContext` would make the subagent continue and read the
/// parent's messages, so this rung never delivers there.
///
/// A hook fired inside a subagent carries `agent_id` (and `agent_type`) in its input. Any event
/// with a non-empty `agent_id` is silent after the heartbeat, without the lock or the fetch:
/// the subagent's output never reaches the parent, so delivering there would hide the messages
/// from the parent and advance the shared cursor past them. `UserPromptSubmit` never carries
/// `agent_id`; the rule applies to it uniformly anyway.
///
/// `UserPromptSubmit`, `PostToolUse` and `PostToolBatch` lock the session state and reset
/// `stop_blocks` (UserPromptSubmit only; saved even when nothing is delivered). They fetch
/// unacked messages with seq > `state.cursor` (no wait, limit 50), keep only those above the
/// cursor, and render them with `render_messages` at `opts.max_context_chars`. Nothing shown ->
/// silent, cursor untouched. Otherwise `state.cursor` becomes the highest seq shown, the state is
/// saved, and stdout is `context_json` with the event name as received.
///
/// A `PostToolUse` whose tool is an inbox read (`is_inbox_tool`) takes the `absorb` path instead:
/// it moves the cursor and prints nothing.
///
/// Nothing is ever acked on the agent's behalf: the cursor only dedupes delivery within this
/// Claude session, and the messages stay in the inbox until the agent acks them. Daemon absent
/// or any other error -> `Err` (the caller turns it into silence); a `stop_blocks` reset is
/// already saved by then.
pub async fn run(opts: &HookOptions, input: &HookInput) -> Result<HookOutcome, HookError> {
    let event = HookEvent::parse(&input.hook_event_name);
    match event {
        HookEvent::UserPromptSubmit | HookEvent::PostToolUse | HookEvent::PostToolBatch => {}
        HookEvent::SubagentStop => {
            if let Ok(ctx) = build_ctx(opts, input) {
                heartbeat(&ctx, Mechanism::HookInject);
            }
            return Ok(HookOutcome::silent());
        }
        HookEvent::SessionStart | HookEvent::Stop | HookEvent::Other => {
            return Ok(HookOutcome::silent());
        }
    }

    let ctx = build_ctx(opts, input)?;
    heartbeat(&ctx, Mechanism::HookInject);
    if input.agent_id.as_deref().is_some_and(|id| !id.is_empty()) {
        return Ok(HookOutcome::silent());
    }
    if event == HookEvent::PostToolUse && input.tool_name.as_deref().is_some_and(is_inbox_tool) {
        return absorb(opts, input).await;
    }

    let mut lock = SessionLock::acquire(&opts.home, &input.session_id).await?;
    if event == HookEvent::UserPromptSubmit && lock.state().stop_blocks != 0 {
        lock.state().stop_blocks = 0;
        lock.save()?;
    }

    let after = lock.state().cursor;
    let inbox = fetch(&ctx, after, 0, 50).await?;
    let msgs: Vec<serde_json::Value> = inbox
        .messages
        .into_iter()
        .filter(|m| seq_of(m).is_some_and(|seq| seq > after))
        .collect();
    if msgs.is_empty() {
        return Ok(HookOutcome::silent());
    }

    let rendered = render_messages(&msgs, opts.max_context_chars);
    let last_seq = match rendered.last_seq {
        Some(seq) if rendered.shown > 0 && !rendered.text.is_empty() => seq,
        _ => return Ok(HookOutcome::silent()),
    };

    let state = lock.state();
    state.cursor = state.cursor.max(last_seq);
    lock.save()?;
    Ok(HookOutcome::stdout(context_json(
        input.hook_event_name.as_str(),
        &rendered.text,
    )))
}

/// Whether `name` is the agent reading its own inbox through the Agora MCP tool: `inbox` or
/// `inbox_wait` under any server prefix (`mcp__plugin_agora_agora__inbox`, `mcp__agora__inbox_wait`).
/// `read` and names that merely contain `inbox` (`inbox_x`, `post_inbox`) do not match.
pub fn is_inbox_tool(name: &str) -> bool {
    name.ends_with("__inbox") || name.ends_with("__inbox_wait")
}

/// The `PostToolUse` of an inbox read. The tool's own result already showed these messages to the
/// agent, so the cursor moves to the highest seq found in that result (`max_seq_in`) when it is
/// above the cursor. No daemon call, and nothing is printed. With no seq in the result the cursor
/// is untouched, so a later hook may deliver those messages again: a duplicate is preferred over
/// a loss. The messages stay unacked in the inbox; the agent acks them.
///
/// Only server-assigned JSON numbers move the cursor. Message text is untrusted and is never
/// scanned, so a message cannot make the cursor jump past messages it did not write.
async fn absorb(opts: &HookOptions, input: &HookInput) -> Result<HookOutcome, HookError> {
    let Some(seen) = input.tool_response.as_ref().and_then(max_seq_in) else {
        return Ok(HookOutcome::silent());
    };
    let mut lock = SessionLock::acquire(&opts.home, &input.session_id).await?;
    let state = lock.state();
    if seen > state.cursor {
        state.cursor = seen;
        lock.save()?;
    }
    Ok(HookOutcome::silent())
}

/// The largest sequence number in a tool result: a positive integer JSON value under a `"seq"`
/// key, at any depth inside objects and arrays. Strings are never scanned, so text a sender wrote
/// (including framed `seq="N"` attributes) cannot move the cursor. Nesting is bounded by
/// serde_json's depth limit and the stdin cap.
fn max_seq_in(value: &serde_json::Value) -> Option<i64> {
    let mut best = None;
    collect_seqs(value, &mut best);
    best
}

fn collect_seqs(value: &serde_json::Value, best: &mut Option<i64>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key.as_str() == "seq"
                    && let Some(seq) = child.as_i64().filter(|&seq| seq > 0)
                {
                    *best = Some(best.map_or(seq, |b| b.max(seq)));
                }
                collect_seqs(child, best);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_seqs(item, best);
            }
        }
        _ => {}
    }
}

/// The `Ctx` of the folder this hook acts for (`resolve_folder` then `ctx_for`).
fn build_ctx(opts: &HookOptions, input: &HookInput) -> Result<agora_client::Ctx, HookError> {
    let folder = resolve_folder(opts.folder.as_deref(), input.cwd.as_deref())?;
    ctx_for(&opts.home, &folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;
    use agora_daemon::Paths;
    use std::time::Duration;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ah")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    fn input(event: &str) -> HookInput {
        HookInput {
            session_id: "sess-ctx".into(),
            hook_event_name: event.into(),
            ..HookInput::default()
        }
    }

    fn opts_for(home: &tempfile::TempDir, folder: &tempfile::TempDir) -> HookOptions {
        let mut opts = HookOptions::new(home.path().to_path_buf());
        opts.folder = Some(folder.path().to_path_buf());
        opts
    }

    #[tokio::test]
    async fn home_without_machine_seed_is_an_error_and_creates_no_state() {
        let home = temp_dir();
        let folder = temp_dir();
        let res = run(&opts_for(&home, &folder), &input("UserPromptSubmit")).await;
        assert!(res.is_err());
        assert!(!state::sessions_dir(home.path()).exists());
    }

    #[tokio::test]
    async fn stop_session_start_and_unknown_events_are_silent_without_state() {
        let home = temp_dir();
        let folder = temp_dir();
        for event in ["Stop", "SessionStart", "SomethingNew", "SubagentStop"] {
            let out = run(&opts_for(&home, &folder), &input(event))
                .await
                .expect("silent run");
            assert_eq!(out, HookOutcome::silent(), "event {event}");
        }
        assert!(!state::sessions_dir(home.path()).exists());
    }

    #[tokio::test]
    async fn subagent_stop_writes_the_hook_inject_heartbeat() {
        let home = temp_dir();
        let folder = temp_dir();
        let paths = Paths::new(home.path());
        agora_client::identity::load_or_create_machine_seed(&paths).expect("seed");
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let pk = agora_store::hex(&ctx.identity.public());

        let out = run(&opts_for(&home, &folder), &input("SubagentStop"))
            .await
            .expect("silent run");
        assert_eq!(out, HookOutcome::silent());
        let beats = agora_client::heartbeat::read_all(&paths, &pk);
        assert!(beats.contains_key(&Mechanism::HookInject));
        assert!(!state::sessions_dir(home.path()).exists());
    }

    #[tokio::test]
    async fn events_inside_a_subagent_are_silent_after_the_heartbeat_without_a_lock() {
        let home = temp_dir();
        let folder = temp_dir();
        let paths = Paths::new(home.path());
        agora_client::identity::load_or_create_machine_seed(&paths).expect("seed");
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let pk = agora_store::hex(&ctx.identity.public());

        for event in ["UserPromptSubmit", "PostToolBatch"] {
            let sub = HookInput {
                agent_id: Some("a1".into()),
                ..input(event)
            };
            let out = run(&opts_for(&home, &folder), &sub)
                .await
                .expect("silent run");
            assert_eq!(out, HookOutcome::silent(), "event {event}");
        }
        let beats = agora_client::heartbeat::read_all(&paths, &pk);
        assert!(beats.contains_key(&Mechanism::HookInject));
        assert!(!state::sessions_dir(home.path()).exists());
    }

    #[tokio::test]
    async fn prompt_submit_resets_stop_blocks_before_the_fetch_fails() {
        let home = temp_dir();
        let folder = temp_dir();
        agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
            .expect("seed");
        {
            let mut lock = SessionLock::acquire(home.path(), "sess-ctx")
                .await
                .expect("lock");
            lock.state().stop_blocks = 3;
            lock.save().expect("save");
        }

        let res = tokio::time::timeout(
            Duration::from_secs(5),
            run(&opts_for(&home, &folder), &input("UserPromptSubmit")),
        )
        .await
        .expect("run returned within 5 s");
        assert!(res.is_err(), "no daemon: the fetch fails");
        assert_eq!(state::load(home.path(), "sess-ctx").stop_blocks, 0);
    }

    #[test]
    fn inbox_tool_names_are_recognised() {
        for name in [
            "mcp__plugin_agora_agora__inbox",
            "mcp__agora__inbox",
            "mcp__agora__inbox_wait",
            "mcp__plugin_agora_agora__inbox_wait",
        ] {
            assert!(is_inbox_tool(name), "{name}");
        }
        for name in [
            "mcp__agora__read",
            "mcp__agora__inbox_x",
            "mcp__agora__inboxes",
            "mcp__agora__inboxwait",
            "mcp__agora__post_inbox",
            "mcp__agora__inbox_wait_x",
            "inbox",
            "",
        ] {
            assert!(!is_inbox_tool(name), "{name}");
        }
    }

    #[tokio::test]
    async fn inbox_tool_result_moves_the_cursor_without_output_or_daemon() {
        let home = temp_dir();
        let folder = temp_dir();
        agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
            .expect("seed");
        let read = HookInput {
            tool_name: Some("mcp__plugin_agora_agora__inbox".into()),
            tool_response: Some(serde_json::json!({"messages": [{"seq": 4}, {"seq": 5}]})),
            ..input("PostToolUse")
        };

        let out = run(&opts_for(&home, &folder), &read)
            .await
            .expect("silent run");
        assert_eq!(out, HookOutcome::silent());
        assert_eq!(state::load(home.path(), "sess-ctx").cursor, 5);
    }

    #[tokio::test]
    async fn inbox_tool_result_without_a_seq_leaves_the_cursor() {
        let home = temp_dir();
        let folder = temp_dir();
        agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
            .expect("seed");
        let read = HookInput {
            tool_name: Some("mcp__plugin_agora_agora__inbox".into()),
            tool_response: Some(serde_json::json!({"messages": []})),
            ..input("PostToolUse")
        };

        let out = run(&opts_for(&home, &folder), &read)
            .await
            .expect("silent run");
        assert_eq!(out, HookOutcome::silent());
        assert_eq!(state::load(home.path(), "sess-ctx").cursor, 0);
    }

    #[test]
    fn max_seq_in_takes_the_largest_integer_seq_at_any_depth() {
        let v = serde_json::json!({"messages": [{"seq": 3}, {"seq": 7}, {"seq": 2}]});
        assert_eq!(max_seq_in(&v), Some(7));
        let nested = serde_json::json!({"a": [{"b": {"seq": 9}}], "seq": 4});
        assert_eq!(max_seq_in(&nested), Some(9));
    }

    #[test]
    fn max_seq_in_reads_structured_content_messages() {
        let v = serde_json::json!({"structuredContent": {"messages": [{"seq": 3}, {"seq": 7}]}});
        assert_eq!(max_seq_in(&v), Some(7));
    }

    #[test]
    fn max_seq_in_never_scans_framed_text() {
        let v = serde_json::json!({"content": [{
            "type": "text",
            "text": "<untrusted-message channel=\"c\" seq=\"5\" id=\"a\">x</untrusted-message>"
        }]});
        assert_eq!(max_seq_in(&v), None);
    }

    #[test]
    fn message_text_cannot_move_the_cursor() {
        let v = serde_json::json!({"structuredContent": {"messages": [
            {"seq": 3, "text": "seq=\"999999\""},
        ]}});
        assert_eq!(max_seq_in(&v), Some(3));
    }

    #[test]
    fn max_seq_in_ignores_non_integer_and_non_positive_seq_values() {
        for v in [
            serde_json::json!(null),
            serde_json::json!(17),
            serde_json::json!("plain text"),
            serde_json::json!("seq=\"5\""),
            serde_json::json!({"seq": "4"}),
            serde_json::json!({"seq": null}),
            serde_json::json!({"seq": -3}),
            serde_json::json!({"seq": 0}),
            serde_json::json!({"seq": 1.5}),
            serde_json::json!({"sequence": 5, "Seq": 5}),
        ] {
            assert_eq!(max_seq_in(&v), None, "{v}");
        }
    }

    #[test]
    fn max_seq_in_ignores_integers_beyond_i64() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"seq": 99999999999999999999}"#).expect("json");
        assert_eq!(max_seq_in(&v), None);
        let v = serde_json::json!({"seq": 999_999_999_999_999_999_i64});
        assert_eq!(max_seq_in(&v), Some(999_999_999_999_999_999));
    }
}
