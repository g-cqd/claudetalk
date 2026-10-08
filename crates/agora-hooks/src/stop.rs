//! `agora hook stop`: keep Claude working while the inbox holds undelivered messages.
//!
//! Loop protection has two parts. Claude Code runs Stop again after a block, with
//! `stop_hook_active` true, and that run stays silent, so one block never chains into another by
//! itself. Across runs, `stop_blocks` counts the blocks since the last user prompt: at
//! `max_blocks` Stop stops blocking. The cursor is left alone in that case, so the pending
//! messages stay unseen and are delivered by the next rung or the next prompt. `UserPromptSubmit`
//! (the `context` hook) resets the count to 0.

use crate::daemon::{ctx_for, fetch, heartbeat, resolve_folder, seq_of};
use crate::render::{cap_chars, render_messages};
use crate::schema::{HookEvent, HookInput, block_json};
use crate::state::{SessionLock, SessionState};
use crate::{HookError, HookOptions, HookOutcome};
use agora_deliver::Mechanism;
use serde_json::Value;

/// Messages fetched per Stop run; the rest wait for a later run.
const FETCH_LIMIT: i64 = 50;

/// What a Stop run does after looking at the state and the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Print nothing. `reset`: the block count goes back to 0 and is saved.
    Silent { reset: bool },
    /// Print `json` (a block), then save the new cursor and block count.
    Block {
        json: String,
        new_cursor: i64,
        new_blocks: u32,
    },
}

/// Heartbeat `stop_continue`. `input.stop_hook_active` true -> silent. Lock the session state;
/// fetch messages with seq > `state.cursor` (no wait, limit 50). None -> `stop_blocks = 0`,
/// save, silent. `stop_blocks >= opts.max_blocks` -> silent (cursor untouched). Otherwise stdout
/// `schema::block_json(text)` with the text rendered like `context` (capped at
/// `opts.max_context_chars`), `stop_blocks += 1`, `cursor` = last shown seq, save.
pub async fn run(opts: &HookOptions, input: &HookInput) -> Result<HookOutcome, HookError> {
    if HookEvent::parse(&input.hook_event_name) != HookEvent::Stop {
        return Ok(HookOutcome::silent());
    }
    let folder = resolve_folder(opts.folder.as_deref(), input.cwd.as_deref())?;
    let ctx = ctx_for(&opts.home, &folder)?;
    heartbeat(&ctx, Mechanism::StopContinue);
    if input.stop_hook_active {
        return Ok(HookOutcome::silent());
    }

    let mut lock = SessionLock::acquire(&opts.home, &input.session_id).await?;
    let inbox = fetch(&ctx, lock.state().cursor, 0, FETCH_LIMIT).await?;
    let decision = decide(
        lock.state(),
        &inbox.messages,
        opts.max_blocks,
        opts.max_context_chars,
    );
    match decision {
        Decision::Silent { reset } => {
            if reset {
                lock.state().stop_blocks = 0;
                lock.save()?;
            }
            Ok(HookOutcome::silent())
        }
        Decision::Block {
            json,
            new_cursor,
            new_blocks,
        } => {
            let state = lock.state();
            state.cursor = new_cursor;
            state.stop_blocks = new_blocks;
            lock.save()?;
            Ok(HookOutcome::stdout(json))
        }
    }
}

/// The Stop decision over the fetched messages, without I/O. Messages with `seq` <= the cursor
/// (or without a `seq`) are ignored; the rest are sorted by seq and rendered with `max_chars`.
fn decide(state: &SessionState, msgs: &[Value], max_blocks: u32, max_chars: usize) -> Decision {
    let mut fresh: Vec<Value> = msgs
        .iter()
        .filter(|m| seq_of(m).is_some_and(|seq| seq > state.cursor))
        .cloned()
        .collect();
    if fresh.is_empty() {
        return Decision::Silent { reset: true };
    }
    if state.stop_blocks >= max_blocks {
        return Decision::Silent { reset: false };
    }
    fresh.sort_by_key(seq_of);
    let rendered = render_messages(&fresh, max_chars);
    if rendered.shown == 0 {
        return Decision::Silent { reset: false };
    }
    Decision::Block {
        json: block_json(&cap_chars(&rendered.text, max_chars)),
        new_cursor: rendered
            .last_seq
            .map_or(state.cursor, |seq| seq.max(state.cursor)),
        new_blocks: state.stop_blocks.saturating_add(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_daemon::Paths;
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ah")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    fn msg(seq: i64, text: &str) -> Value {
        json!({
            "seq": seq,
            "id": format!("m-{seq}"),
            "channel": "c",
            "sender": "ab",
            "pseudonym": "p",
            "ts": 1,
            "encrypted": false,
            "epoch": 0,
            "verified": true,
            "text": text,
            "attachments": [],
        })
    }

    fn state(cursor: i64, stop_blocks: u32) -> SessionState {
        SessionState {
            cursor,
            stop_blocks,
            ..SessionState::default()
        }
    }

    /// The `(json, new_cursor, new_blocks)` of a block decision; panics on silence.
    fn block_of(d: Decision) -> (String, i64, u32) {
        let Decision::Block {
            json,
            new_cursor,
            new_blocks,
        } = d
        else {
            panic!("expected a block, got {d:?}");
        };
        (json, new_cursor, new_blocks)
    }

    fn reason_of(json: &str) -> String {
        let v: Value = serde_json::from_str(json).expect("block json");
        v["reason"].as_str().expect("reason string").to_owned()
    }

    #[test]
    fn empty_inbox_resets_the_block_count() {
        let d = decide(&state(3, 5), &[], 8, 9_000);
        assert_eq!(d, Decision::Silent { reset: true });
    }

    #[test]
    fn messages_at_or_below_the_cursor_are_ignored() {
        let msgs = [msg(5, "old-five"), msg(7, "old-seven")];
        let d = decide(&state(7, 2), &msgs, 8, 9_000);
        assert_eq!(d, Decision::Silent { reset: true });
    }

    #[test]
    fn blocks_at_the_cap_are_silent_without_reset() {
        let msgs = [msg(1, "pending")];
        assert_eq!(
            decide(&state(0, 8), &msgs, 8, 9_000),
            Decision::Silent { reset: false }
        );
        assert_eq!(
            decide(&state(0, 9), &msgs, 8, 9_000),
            Decision::Silent { reset: false }
        );
    }

    #[test]
    fn one_block_below_the_cap_still_blocks() {
        let msgs = [msg(1, "pending")];
        let (_, cursor, blocks) = block_of(decide(&state(0, 7), &msgs, 8, 9_000));
        assert_eq!((cursor, blocks), (1, 8));
    }

    #[test]
    fn block_moves_cursor_to_last_shown_and_counts_the_block() {
        let msgs = [
            msg(3, "old-three"),
            msg(6, "beta-six"),
            msg(5, "alpha-five"),
        ];
        let (json, cursor, blocks) = block_of(decide(&state(4, 2), &msgs, 8, 9_000));
        let reason = reason_of(&json);
        assert!(reason.contains("alpha-five"), "{reason}");
        assert!(reason.contains("beta-six"), "{reason}");
        assert!(!reason.contains("old-three"), "{reason}");
        assert_eq!(cursor, 6);
        assert_eq!(blocks, 3);
    }

    #[test]
    fn reason_never_exceeds_max_chars() {
        let long = "x".repeat(5_000);
        let msgs = [msg(1, &long)];
        let max_chars = 1_000;
        let (json, cursor, _) = block_of(decide(&state(0, 0), &msgs, 8, max_chars));
        let reason = reason_of(&json);
        assert!(!reason.is_empty());
        assert!(reason.chars().count() <= max_chars);
        assert_eq!(cursor, 1);
    }

    #[test]
    fn nothing_that_fits_is_silent_without_reset() {
        let msgs = [msg(1, "hi")];
        assert_eq!(
            decide(&state(0, 0), &msgs, 8, 10),
            Decision::Silent { reset: false }
        );
    }

    #[tokio::test]
    async fn non_stop_event_is_silent() {
        let opts = HookOptions::new(PathBuf::from("/nonexistent-agora-home"));
        let input = HookInput {
            hook_event_name: "UserPromptSubmit".to_owned(),
            ..HookInput::default()
        };
        let out = run(&opts, &input).await.expect("silent");
        assert_eq!(out, HookOutcome::silent());
    }

    #[tokio::test]
    async fn stop_hook_active_is_silent_and_still_arms_the_heartbeat() {
        let home = temp_dir();
        let folder = temp_dir();
        let paths = Paths::new(home.path());
        agora_client::identity::load_or_create_machine_seed(&paths).expect("machine seed");
        let opts = HookOptions {
            folder: Some(folder.path().to_path_buf()),
            ..HookOptions::new(home.path().to_path_buf())
        };
        let input = HookInput {
            hook_event_name: "Stop".to_owned(),
            session_id: "stop-active".to_owned(),
            stop_hook_active: true,
            ..HookInput::default()
        };
        let out = run(&opts, &input).await.expect("silent");
        assert_eq!(out, HookOutcome::silent());

        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let pk = agora_store::hex(&ctx.identity.public());
        let beat = agora_client::heartbeat::dir(&paths, &pk).join(Mechanism::StopContinue.as_str());
        assert!(beat.is_file(), "missing {}", beat.display());
    }

    #[tokio::test]
    async fn no_machine_seed_is_an_error() {
        let home = temp_dir();
        let folder = temp_dir();
        let opts = HookOptions {
            folder: Some(folder.path().to_path_buf()),
            ..HookOptions::new(home.path().to_path_buf())
        };
        let input = HookInput {
            hook_event_name: "Stop".to_owned(),
            session_id: "no-seed".to_owned(),
            ..HookInput::default()
        };
        assert!(run(&opts, &input).await.is_err());
    }
}
