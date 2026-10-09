//! `agora hook poll --session ID --folder DIR`: called by the plugin mod every few seconds.

use crate::daemon::{ctx_for, fetch, heartbeat_for, resolve_folder, seq_of};
use crate::render::{Rendered, render_messages};
use crate::schema::HookInput;
use crate::state::SessionLock;
use crate::{HookError, HookOptions, HookOutcome};
use agora_deliver::Mechanism;

/// Called by `plugin/claude/hooks/register.js` every 5 s while the session is idle. The mod
/// submits the returned `text` with `$.prompt.submit`, so one poll becomes one turn. Delivering
/// advances the per-session cursor that the other hook rungs share, so they will not repeat the
/// messages. Nothing is acked on the agent's behalf.
///
/// Stdin is ignored (`_input` is unused). The session comes from `opts.session` and the folder
/// from `opts.folder` (the CLI requires both). An empty or missing session is
/// `Err(HookError::Input(..))`. Without `opts.folder`, `resolve_folder` falls back to the
/// environment and the working directory. The run heartbeats `mod_submit`, takes the session
/// lock, and fetches messages with seq > the cursor (no wait, limit 50). When nothing is shown
/// the stdout is `{"count":0}`. Otherwise it is `{"count":N,"text":"<rendered>"}` on one line,
/// the cursor moves to the last shown seq, and the state is saved before the output is returned.
pub async fn run(opts: &HookOptions, _input: &HookInput) -> Result<HookOutcome, HookError> {
    let session = match opts.session.as_deref() {
        Some(session) if !session.is_empty() => session,
        _ => return Err(HookError::Input("poll needs --session".into())),
    };
    let folder = resolve_folder(opts.folder.as_deref(), None)?;
    let ctx = ctx_for(&opts.home, &folder)?;
    heartbeat_for(&ctx, opts.dialect.harness(), Mechanism::ModSubmit);

    let mut lock = SessionLock::acquire(&opts.home, session).await?;
    let cursor = lock.state().cursor;
    let inbox = fetch(&ctx, cursor, 0, 50).await?;
    let fresh: Vec<serde_json::Value> = inbox
        .messages
        .into_iter()
        .filter(|m| seq_of(m).is_some_and(|seq| seq > cursor))
        .collect();

    let rendered = render_messages(&fresh, opts.max_context_chars);
    if rendered.shown > 0 {
        // Shown messages always carry a seq. Without one the cursor cannot move, so fail
        // silently rather than deliver the same text again on the next poll.
        let last = rendered
            .last_seq
            .ok_or_else(|| HookError::Input("rendered messages without a seq".into()))?;
        lock.state().cursor = cursor.max(last);
        lock.save()?;
    }
    Ok(HookOutcome::stdout(poll_json(&rendered)))
}

/// The one-line stdout of `poll`: `{"count":0}` when nothing was shown, otherwise
/// `{"count":N,"text":..}` with N = `r.shown`.
fn poll_json(r: &Rendered) -> String {
    if r.shown == 0 {
        return serde_json::json!({"count": 0}).to_string();
    }
    serde_json::json!({"count": r.shown, "text": r.text}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_daemon::Paths;
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// A short temp dir under /tmp (Unix socket paths must stay short).
    fn temp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ap")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    fn opts(home: &Path, session: Option<&str>, folder: Option<PathBuf>) -> HookOptions {
        let mut o = HookOptions::new(home.to_path_buf());
        o.session = session.map(str::to_string);
        o.folder = folder;
        o
    }

    #[tokio::test]
    async fn missing_session_is_an_input_error() {
        let home = temp_home();
        let folder = temp_home();
        let o = opts(home.path(), None, Some(folder.path().to_path_buf()));
        let res = run(&o, &HookInput::default()).await;
        assert!(matches!(res, Err(HookError::Input(_))));
    }

    #[tokio::test]
    async fn empty_session_is_an_input_error() {
        let home = temp_home();
        let folder = temp_home();
        let o = opts(home.path(), Some(""), Some(folder.path().to_path_buf()));
        let res = run(&o, &HookInput::default()).await;
        assert!(matches!(res, Err(HookError::Input(_))));
    }

    #[tokio::test]
    async fn no_machine_seed_is_an_error_and_creates_nothing() {
        let home = temp_home();
        let folder = temp_home();
        let o = opts(home.path(), Some("s1"), Some(folder.path().to_path_buf()));
        let res = run(&o, &HookInput::default()).await;
        assert!(res.is_err());
        let entries = std::fs::read_dir(home.path()).expect("read home").count();
        assert_eq!(entries, 0);
    }

    #[tokio::test]
    async fn mod_submit_heartbeat_is_written_before_the_fetch_fails() {
        let home = temp_home();
        let folder = temp_home();
        agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
            .expect("machine seed");
        let o = opts(home.path(), Some("s1"), Some(folder.path().to_path_buf()));
        // No daemon is running, so the fetch fails; the bound only guards against a hang.
        let res = tokio::time::timeout(Duration::from_secs(5), run(&o, &HookInput::default()))
            .await
            .expect("poll returned within 5 s");
        assert!(res.is_err());

        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let pk = agora_store::hex(&ctx.identity.public());
        let file =
            agora_client::heartbeat::dir(&ctx.paths, &pk).join(Mechanism::ModSubmit.as_str());
        let text = std::fs::read_to_string(&file).expect("mod_submit heartbeat file");
        let ts: i64 = text.trim().parse().expect("decimal ms timestamp");
        assert!(ts > 0);
    }

    #[test]
    fn poll_json_count_zero_has_only_the_count_key() {
        let r = Rendered {
            text: String::new(),
            shown: 0,
            last_seq: None,
        };
        let line = poll_json(&r);
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("json");
        let obj = v.as_object().expect("object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, ["count"]);
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn poll_json_count_n_has_exactly_count_and_text() {
        let r = Rendered {
            text: "Agora: 2 new messages.\n<message>a</message>".into(),
            shown: 2,
            last_seq: Some(9),
        };
        let line = poll_json(&r);
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("json");
        let obj = v.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["count", "text"]);
        assert_eq!(v["count"], 2);
        assert_eq!(v["text"], r.text);
    }
}
