//! `agora hook rewake`: the asyncRewake rung. Runs in the background after Stop (or, with
//! `--quick`, after SessionStart) and exits 2 with the rendered messages on stderr to wake an
//! idle Claude Code session.
//!
//! Ownership: a session has at most one long-polling rewake that matters. Each long-mode run
//! writes its token `"<pid>-<start ms>"` into `SessionState::rewake_owner` under the session
//! lock, so the newest poller wins. Every superseded poller finds another owner at the start of
//! its next round and exits 0 silently, so it is gone within one round (at most 55 s).
//!
//! The lock is never held while waiting. A long-poll lasts up to 55 s per round and up to an
//! hour in total, and the context and stop hooks of the same session need that lock on every
//! prompt; holding it would stall them past their budgets. The lock is taken only for the
//! owner check and the cursor update.
//!
//! Quick mode (`--quick`, the SessionStart variant) takes no ownership and does not wait. It
//! makes one non-waiting fetch bounded by `min(budget_ms, QUICK_BUDGET_MS)`. Claude Code runs
//! asyncRewake SessionStart hooks synchronously in `-p`/desktop/stream-json mode (bug
//! anthropics/claude-code#89960), so the quick check keeps that startup block to about 1.5 s.
//! The real long-poll is armed on every Stop. Quick mode writes no `async_rewake_hook`
//! heartbeat: a one-shot check does not prove an armed long-poll, so only long mode does.
//!
//! Entrypoint guard: Claude Code sets `CLAUDE_CODE_ENTRYPOINT` for itself and its children:
//! `cli` for the interactive TUI, `sdk-cli` for `claude -p`, `sdk-ts`/`sdk-py` for the Agent SDK,
//! and other values for desktop, IDE and remote surfaces. Outside the TUI the rung misbehaves. A
//! SessionStart rewake runs synchronously and its exit 2 is dropped after the cursor has advanced,
//! so the message is lost to the other hooks (bug anthropics/claude-code#89960). A Stop rewake
//! keeps `claude -p` waiting until its long-poll ends, which is a hang until killed. So `run`
//! returns silent before it touches the heartbeat, the ownership or the daemon whenever the
//! variable is set to anything but exactly `cli`. Unset or empty (manual runs, tests) runs as
//! before. Caveat: a TUI started from inside another Claude Code session inherits the parent's
//! value, so its rewake stays silent.

use std::time::Duration;

use agora_client::ops::now_ms;
use agora_deliver::Mechanism;
use serde_json::Value;
use tokio::time::Instant;

use crate::daemon::{ctx_for, fetch, heartbeat, resolve_folder, seq_of};
use crate::render::render_messages;
use crate::schema::HookInput;
use crate::state::{SessionLock, load};
use crate::{HookError, HookOptions, HookOutcome, QUICK_BUDGET_MS};

/// The most messages one fetch returns.
const FETCH_LIMIT: i64 = 50;
/// The longest single long-poll round, in ms. The daemon caps InboxWait at 55 s as well.
const MAX_ROUND_MS: u64 = 55_000;
/// A round that returns faster than this did not wait, with or without messages: the daemon
/// does not honour waits, so the rung stops rather than spin.
const MIN_ROUND: Duration = Duration::from_millis(100);
/// Less time than this left in the total wait means no new round is started.
const MIN_REMAINING: Duration = Duration::from_secs(1);

/// First, unless `CLAUDE_CODE_ENTRYPOINT` is unset, empty or exactly `cli`, returns silent at once
/// (see the module doc).
///
/// Long mode heartbeats `async_rewake_hook` at start and after every round; quick mode writes
/// no heartbeat.
///
/// `opts.quick` (SessionStart variant, no ownership, no long wait, no heartbeat): one
/// non-waiting fetch bounded by `min(opts.budget_ms, QUICK_BUDGET_MS)`; messages (seq >
/// `state.cursor`) -> deliver as below (exit 2); else silent. This bounds the #89960 startup
/// block to ~1.5 s.
///
/// Otherwise take ownership: `state.rewake_owner = "<pid>-<start ms>"`, saved under the lock
/// (newest wins). Loop until `opts.max_wait_ms` is spent: if the owner in the state is not us
/// -> silent; long-poll `daemon::fetch(after = state.cursor, wait = min(remaining, 55 s))`
/// WITHOUT holding the lock; heartbeat after every round (an idle long-poll stays fresh); a
/// round faster than `MIN_ROUND` that returns nothing new -> silent (no spin); on messages:
/// lock, keep only seq > `state.cursor`, none -> continue; owner not us -> silent; else
/// `HookOutcome::wake(render::render_messages(..).text)`, `cursor` = last shown seq, save.
/// Daemon absent or error -> silent exit 0 (`Ok(HookOutcome::silent())` or `Err`).
pub async fn run(opts: &HookOptions, input: &HookInput) -> Result<HookOutcome, HookError> {
    let entrypoint = std::env::var_os("CLAUDE_CODE_ENTRYPOINT");
    // A value that is not UTF-8 becomes replacement characters, which are not "cli": silent.
    let entrypoint = entrypoint.as_deref().map(|v| v.to_string_lossy());
    if !interactive_entrypoint(entrypoint.as_deref()) {
        return Ok(HookOutcome::silent());
    }
    let folder = resolve_folder(opts.folder.as_deref(), input.cwd.as_deref())?;
    let ctx = ctx_for(&opts.home, &folder)?;
    let session = input.session_id.as_str();

    if opts.quick {
        return quick(opts, &ctx, session).await;
    }
    heartbeat(&ctx, Mechanism::AsyncRewakeHook);

    let token = format!("{}-{}", std::process::id(), now_ms());
    {
        let mut lock = SessionLock::acquire(&opts.home, session).await?;
        lock.state().rewake_owner = Some(token.clone());
        lock.save()?;
    }

    let budget = Duration::from_millis(opts.max_wait_ms);
    let started = Instant::now();
    loop {
        if load(&opts.home, session).rewake_owner.as_deref() != Some(token.as_str()) {
            return Ok(HookOutcome::silent());
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining < MIN_REMAINING {
            return Ok(HookOutcome::silent());
        }
        let wait_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .min(MAX_ROUND_MS);
        let cursor = load(&opts.home, session).cursor;

        let round = Instant::now();
        let inbox = match fetch(&ctx, cursor, wait_ms, FETCH_LIMIT).await {
            Ok(inbox) => inbox,
            // Daemon absent or failing: nothing to wait for.
            Err(_) => return Ok(HookOutcome::silent()),
        };
        heartbeat(&ctx, Mechanism::AsyncRewakeHook);
        let fast = round.elapsed() < MIN_ROUND;

        if inbox.messages.is_empty() {
            if fast {
                return Ok(HookOutcome::silent());
            }
            // The long-poll timed out with nothing new: wait again.
            continue;
        }
        if let Some(outcome) = deliver(opts, session, Some(&token), inbox.messages).await? {
            return Ok(outcome);
        }
        if fast {
            // Messages came back, none new, and the round did not wait: the daemon keeps
            // returning what sits above the cursor. Stop rather than spin.
            return Ok(HookOutcome::silent());
        }
    }
}

/// The quick (SessionStart) variant: one non-waiting fetch after the session cursor, bounded by
/// `min(budget_ms, QUICK_BUDGET_MS)`, then delivery. No ownership is taken.
async fn quick(
    opts: &HookOptions,
    ctx: &agora_client::Ctx,
    session: &str,
) -> Result<HookOutcome, HookError> {
    let budget = Duration::from_millis(opts.budget_ms.min(QUICK_BUDGET_MS));
    let cursor = load(&opts.home, session).cursor;
    let inbox = match tokio::time::timeout(budget, fetch(ctx, cursor, 0, FETCH_LIMIT)).await {
        Ok(Ok(inbox)) => inbox,
        Ok(Err(_)) | Err(_) => return Ok(HookOutcome::silent()),
    };
    if inbox.messages.is_empty() {
        return Ok(HookOutcome::silent());
    }
    Ok(deliver(opts, session, None, inbox.messages)
        .await?
        .unwrap_or_else(HookOutcome::silent))
}

/// Deliver the messages not yet delivered to this session, under the session lock.
///
/// `owner` is the token of a long-mode run: when the state names another owner the result is
/// `Some(silent)`, which ends the run. `None` means no ownership check (quick mode). Returns
/// `None` when nothing new is shown (the caller keeps waiting). Otherwise the cursor advances
/// to the last shown seq, is saved, and the result is the wake outcome.
async fn deliver(
    opts: &HookOptions,
    session: &str,
    owner: Option<&str>,
    msgs: Vec<Value>,
) -> Result<Option<HookOutcome>, HookError> {
    let mut lock = SessionLock::acquire(&opts.home, session).await?;
    if owner.is_some_and(|token| lock.state().rewake_owner.as_deref() != Some(token)) {
        return Ok(Some(HookOutcome::silent()));
    }
    let new = fresh(msgs, lock.state().cursor);
    if new.is_empty() {
        return Ok(None);
    }
    let rendered = render_messages(&new, opts.max_context_chars);
    if rendered.shown == 0 {
        return Ok(None);
    }
    if let Some(last) = rendered.last_seq {
        let state = lock.state();
        state.cursor = state.cursor.max(last);
    }
    lock.save()?;
    Ok(Some(HookOutcome::wake(rendered.text)))
}

/// The messages with a `seq` above `cursor`, in ascending seq order. Sorting makes the cursor
/// (the seq of the last shown message) safe: no unshown message can sit below it. Messages
/// without an integer `seq` are dropped, since the cursor cannot track them.
fn fresh(mut msgs: Vec<Value>, cursor: i64) -> Vec<Value> {
    msgs.retain(|m| seq_of(m).is_some_and(|seq| seq > cursor));
    msgs.sort_by_key(seq_of);
    msgs
}

/// Whether the rewake rung may run, given the value of `CLAUDE_CODE_ENTRYPOINT`. Only the
/// interactive TUI (`cli`) runs it. Unset or empty (manual runs, tests) runs it too. Any other
/// value, such as `sdk-cli`, `sdk-ts`, `claude-desktop` or `CLI`, means the rung stays silent.
fn interactive_entrypoint(v: Option<&str>) -> bool {
    matches!(v, None | Some("") | Some("cli"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ah")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    /// A temp home holding a machine seed.
    fn seeded_home() -> tempfile::TempDir {
        let home = temp_dir();
        agora_client::identity::load_or_create_machine_seed(&agora_daemon::Paths::new(home.path()))
            .expect("machine seed");
        home
    }

    fn opts_for(home: &tempfile::TempDir, folder: &tempfile::TempDir, quick: bool) -> HookOptions {
        let mut opts = HookOptions::new(home.path().to_path_buf());
        opts.folder = Some(folder.path().to_path_buf());
        opts.quick = quick;
        opts
    }

    /// The `async_rewake_hook` heartbeat file of the seeded identity for `folder`.
    fn rewake_beat(home: &tempfile::TempDir, folder: &tempfile::TempDir) -> std::path::PathBuf {
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let pk = agora_store::hex(&ctx.identity.public());
        agora_client::heartbeat::dir(&ctx.paths, &pk).join(Mechanism::AsyncRewakeHook.as_str())
    }

    fn input(session: &str) -> HookInput {
        HookInput {
            session_id: session.to_string(),
            hook_event_name: "Stop".to_string(),
            ..HookInput::default()
        }
    }

    fn msg(seq: i64) -> Value {
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
            "text": format!("body {seq}"),
            "attachments": [],
        })
    }

    #[tokio::test]
    async fn no_seed_is_an_error() {
        let home = temp_dir();
        let folder = temp_dir();
        let res = run(&opts_for(&home, &folder, false), &input("no-seed")).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn long_mode_without_a_daemon_takes_ownership_and_exits_silent() {
        let home = seeded_home();
        let folder = temp_dir();
        let opts = opts_for(&home, &folder, false);
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(Duration::from_secs(3), run(&opts, &input("long-1")))
            .await
            .expect("run returns within 3 s")
            .expect("run is Ok");
        assert_eq!(out, HookOutcome::silent());
        assert!(started.elapsed() < Duration::from_secs(3));

        let owner = load(home.path(), "long-1")
            .rewake_owner
            .expect("owner is set");
        assert!(owner.starts_with(&format!("{}-", std::process::id())));
        assert!(
            rewake_beat(&home, &folder).is_file(),
            "long mode writes the heartbeat"
        );
    }

    #[tokio::test]
    async fn quick_mode_without_a_daemon_is_silent_and_takes_no_ownership() {
        let home = seeded_home();
        let folder = temp_dir();
        let opts = opts_for(&home, &folder, true);
        let out = tokio::time::timeout(Duration::from_secs(2), run(&opts, &input("quick-1")))
            .await
            .expect("run returns within 2 s")
            .expect("run is Ok");
        assert_eq!(out, HookOutcome::silent());
        assert_eq!(load(home.path(), "quick-1").rewake_owner, None);
    }

    #[tokio::test]
    async fn quick_mode_without_a_daemon_writes_no_heartbeat() {
        let home = seeded_home();
        let folder = temp_dir();
        let opts = opts_for(&home, &folder, true);
        let out = tokio::time::timeout(Duration::from_secs(2), run(&opts, &input("quick-beat")))
            .await
            .expect("run returns within 2 s")
            .expect("run is Ok");
        assert_eq!(out, HookOutcome::silent());
        assert!(
            !rewake_beat(&home, &folder).exists(),
            "a one-shot check does not prove an armed long-poll"
        );
    }

    #[test]
    fn fresh_keeps_newer_sequenced_messages_in_seq_order() {
        let msgs = vec![msg(5), json!({"text": "no seq"}), msg(2), msg(3), msg(7)];
        let got: Vec<i64> = fresh(msgs, 2).iter().filter_map(seq_of).collect();
        assert_eq!(got, vec![3, 5, 7]);
    }

    #[test]
    fn fresh_of_nothing_new_is_empty() {
        assert!(fresh(vec![msg(1)], 1).is_empty());
        assert!(fresh(Vec::new(), 0).is_empty());
    }

    #[tokio::test]
    async fn deliver_wakes_once_and_advances_the_cursor() {
        let home = temp_dir();
        let opts = HookOptions::new(home.path().to_path_buf());
        let msgs = vec![msg(1), msg(2), msg(3)];

        let first = deliver(&opts, "deliver-1", None, msgs.clone())
            .await
            .expect("deliver")
            .expect("first delivery wakes");
        assert_eq!(first.exit_code, 2);
        assert!(first.stderr.starts_with("Agora: 3 new messages for you."));
        assert_eq!(load(home.path(), "deliver-1").cursor, 3);

        let second = deliver(&opts, "deliver-1", None, msgs)
            .await
            .expect("deliver again");
        assert_eq!(
            second, None,
            "already delivered messages are not shown twice"
        );
    }

    #[tokio::test]
    async fn deliver_checks_the_owner_before_delivering() {
        let home = temp_dir();
        let opts = HookOptions::new(home.path().to_path_buf());
        {
            let mut lock = SessionLock::acquire(home.path(), "owned-1")
                .await
                .expect("acquire");
            lock.state().rewake_owner = Some("newer-token".to_string());
            lock.save().expect("save");
        }

        let superseded = deliver(&opts, "owned-1", Some("old-token"), vec![msg(1)])
            .await
            .expect("deliver");
        assert_eq!(superseded, Some(HookOutcome::silent()));
        assert_eq!(load(home.path(), "owned-1").cursor, 0);

        let owner = deliver(&opts, "owned-1", Some("newer-token"), vec![msg(1)])
            .await
            .expect("deliver");
        assert!(owner.is_some_and(|o| o.exit_code == 2));
    }

    #[test]
    fn unset_empty_or_cli_entrypoint_runs() {
        assert!(interactive_entrypoint(None));
        assert!(interactive_entrypoint(Some("")));
        assert!(interactive_entrypoint(Some("cli")));
    }

    #[test]
    fn other_entrypoints_stay_silent() {
        for v in [
            "sdk-cli",
            "sdk-ts",
            "sdk-py",
            "claude-desktop",
            "claude-vscode",
            "CLI",
        ] {
            assert!(
                !interactive_entrypoint(Some(v)),
                "{v} must not run the rung"
            );
        }
    }
}
