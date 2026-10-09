//! Talking to the daemon from a hook: identity, a non-spawning connection and the inbox.

use crate::HookError;
use agora_daemon::Paths;
use agora_deliver::{Harness, Mechanism};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The folder whose identity the hook acts as, canonicalized. Order: `explicit` (poll's
/// `--folder`) > env `AGORA_FOLDER` (when non-empty and not containing `${`, an unexpanded
/// placeholder) > env `CLAUDE_PROJECT_DIR` (non-empty) > `input_cwd` (stdin `cwd`, non-empty) >
/// the process's current directory.
pub fn resolve_folder(
    explicit: Option<&Path>,
    input_cwd: Option<&str>,
) -> Result<PathBuf, HookError> {
    let chosen = match choose_folder(
        explicit,
        std::env::var_os("AGORA_FOLDER"),
        std::env::var_os("CLAUDE_PROJECT_DIR"),
        input_cwd,
    ) {
        Some(folder) => folder,
        None => std::env::current_dir()?,
    };
    // A chosen folder that does not exist is an error, not a reason to try the next candidate.
    Ok(chosen.canonicalize()?)
}

/// The folder choice of `resolve_folder` over the given values, not canonicalized. `None` when
/// every candidate is absent or empty. Pure, so the order is tested without the environment.
fn choose_folder(
    explicit: Option<&Path>,
    agora_folder: Option<OsString>,
    project_dir: Option<OsString>,
    input_cwd: Option<&str>,
) -> Option<PathBuf> {
    explicit
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| env_folder(agora_folder))
        .or_else(|| env_folder(project_dir))
        .or_else(|| input_cwd.filter(|c| !c.is_empty()).map(PathBuf::from))
}

/// An environment value usable as a folder: non-empty and without an unexpanded `${` placeholder.
fn env_folder(value: Option<OsString>) -> Option<PathBuf> {
    value
        .filter(|v| !v.is_empty() && !v.to_string_lossy().contains("${"))
        .map(PathBuf::from)
}

/// The `Ctx` for `folder` via `agora_client::identity::load_existing_identity`: never creates
/// the machine seed or any other state.
pub fn ctx_for(home: &Path, folder: &Path) -> Result<agora_client::Ctx, HookError> {
    let paths = Paths::new(home);
    let (identity, label) = agora_client::identity::load_existing_identity(&paths, folder)?;
    Ok(agora_client::Ctx {
        paths,
        identity,
        label,
    })
}

/// The inbox as the hooks use it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inbox {
    /// The api JSON `messages` array (each message has `channel`, `seq`, `text`, ...).
    pub messages: Vec<serde_json::Value>,
    pub cursor: i64,
    pub timed_out: bool,
}

/// `Client::connect_no_spawn` (daemon absent -> `Err`) then `api::chat::inbox_with` with
/// `InboxParams { wait: (wait_ms > 0).then(|| secs rounded up, at most 55), after, limit }`.
/// `wait_ms == 0` means no wait: only messages with seq > `after` are returned at once.
pub async fn fetch(
    ctx: &agora_client::Ctx,
    after: i64,
    wait_ms: u64,
    limit: i64,
) -> Result<Inbox, HookError> {
    let mut client =
        agora_client::client::Client::connect_no_spawn(&ctx.paths, &ctx.identity, &ctx.label)
            .await?;
    let params = agora_client::api::chat::InboxParams {
        wait: wait_secs(wait_ms),
        after,
        limit,
    };
    let out = agora_client::api::chat::inbox_with(&mut client, ctx, params).await?;
    Ok(inbox_of(out.json, after))
}

/// The long-poll seconds for `wait_ms`: none for 0, otherwise rounded up and at most 55.
fn wait_secs(wait_ms: u64) -> Option<u64> {
    (wait_ms > 0).then(|| wait_ms.div_ceil(1000).min(55))
}

/// The `Inbox` of an api `inbox` JSON: a missing `messages` is empty, a missing `cursor` is
/// `after`, a missing `timed_out` is false.
fn inbox_of(mut json: Value, after: i64) -> Inbox {
    let messages = match json.get_mut("messages").map(std::mem::take) {
        Some(Value::Array(messages)) => messages,
        _ => Vec::new(),
    };
    Inbox {
        messages,
        cursor: json.get("cursor").and_then(Value::as_i64).unwrap_or(after),
        timed_out: json
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// `agora_client::heartbeat::beat_for` for the ctx identity (hex of its public key) and harness
/// `h` at the current time. Errors are ignored.
pub fn heartbeat_for(ctx: &agora_client::Ctx, h: Harness, m: Mechanism) {
    let pk = agora_store::hex(&ctx.identity.public());
    let _ = agora_client::heartbeat::beat_for(&ctx.paths, &pk, h, m, agora_client::ops::now_ms());
}

/// `heartbeat_for` with Claude Code, the default harness. Kept for callers that name none.
pub fn heartbeat(ctx: &agora_client::Ctx, m: Mechanism) {
    heartbeat_for(ctx, Harness::ClaudeCode, m);
}

/// The `seq` field of a message JSON, `None` when absent or not an integer.
pub fn seq_of(m: &serde_json::Value) -> Option<i64> {
    m.get("seq").and_then(Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ah")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    /// A temp home holding a machine seed, and a temp folder to act as.
    fn seeded_home() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = temp_dir();
        agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
            .expect("machine seed");
        (home, temp_dir())
    }

    #[test]
    fn explicit_folder_wins_over_everything() {
        let got = choose_folder(Some(Path::new("/x")), os("/a"), os("/b"), Some("/c"));
        assert_eq!(got, Some(PathBuf::from("/x")));
    }

    #[test]
    fn agora_folder_wins_over_project_dir_when_usable() {
        let got = choose_folder(None, os("/a"), os("/b"), Some("/c"));
        assert_eq!(got, Some(PathBuf::from("/a")));
    }

    #[test]
    fn agora_folder_with_placeholder_is_skipped() {
        let got = choose_folder(None, os("${CLAUDE_PROJECT_DIR}"), os("/b"), Some("/c"));
        assert_eq!(got, Some(PathBuf::from("/b")));
    }

    #[test]
    fn project_dir_is_next() {
        let got = choose_folder(None, None, os("/b"), Some("/c"));
        assert_eq!(got, Some(PathBuf::from("/b")));
    }

    #[test]
    fn input_cwd_is_next() {
        let got = choose_folder(None, os(""), os(""), Some("/c"));
        assert_eq!(got, Some(PathBuf::from("/c")));
    }

    #[test]
    fn empty_candidates_are_skipped_and_none_is_left() {
        assert_eq!(
            choose_folder(Some(Path::new("")), os(""), os(""), Some("")),
            None
        );
        assert_eq!(choose_folder(None, None, None, None), None);
    }

    #[test]
    fn explicit_folder_is_canonicalized() {
        let dir = temp_dir();
        let got = resolve_folder(Some(dir.path()), None).expect("explicit folder resolves");
        assert_eq!(got, dir.path().canonicalize().expect("canonical path"));
    }

    #[test]
    fn missing_explicit_folder_is_an_error() {
        let dir = temp_dir();
        let missing = dir.path().join("missing");
        assert!(matches!(
            resolve_folder(Some(&missing), None),
            Err(HookError::Io(_))
        ));
    }

    #[test]
    fn ctx_for_without_machine_seed_fails_and_creates_nothing() {
        let home = temp_dir();
        let folder = temp_dir();
        assert!(ctx_for(home.path(), folder.path()).is_err());
        let entries = std::fs::read_dir(home.path()).expect("read home").count();
        assert_eq!(entries, 0);
    }

    #[test]
    fn ctx_for_with_machine_seed_builds_the_context() {
        let (home, folder) = seeded_home();
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        assert_eq!(ctx.paths.home, home.path());
    }

    #[test]
    fn wait_secs_rounds_up_and_caps_at_55() {
        assert_eq!(wait_secs(0), None);
        assert_eq!(wait_secs(1), Some(1));
        assert_eq!(wait_secs(1_000), Some(1));
        assert_eq!(wait_secs(1_001), Some(2));
        assert_eq!(wait_secs(55_000), Some(55));
        assert_eq!(wait_secs(u64::MAX), Some(55));
    }

    #[test]
    fn inbox_of_reads_fields_and_defaults_missing_ones() {
        let full = inbox_of(
            json!({"messages": [{"seq": 3}], "cursor": 3, "timed_out": true}),
            0,
        );
        assert_eq!(full.messages.len(), 1);
        assert_eq!(full.cursor, 3);
        assert!(full.timed_out);

        let empty = inbox_of(json!({}), 9);
        assert_eq!(
            empty,
            Inbox {
                messages: vec![],
                cursor: 9,
                timed_out: false
            }
        );

        let wrong_type = inbox_of(json!({"messages": "x"}), 0);
        assert!(wrong_type.messages.is_empty());
    }

    #[tokio::test]
    async fn fetch_without_daemon_fails_fast() {
        let (home, folder) = seeded_home();
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let res = tokio::time::timeout(Duration::from_secs(2), fetch(&ctx, 0, 0, 10))
            .await
            .expect("fetch returned within 2 s");
        assert!(res.is_err());
    }

    #[test]
    fn heartbeat_for_writes_the_hook_inject_file_under_its_harness() {
        let (home, folder) = seeded_home();
        let ctx = ctx_for(home.path(), folder.path()).expect("ctx");
        let before = agora_client::ops::now_ms();
        heartbeat_for(&ctx, Harness::Codex, Mechanism::HookInject);
        let pk = agora_store::hex(&ctx.identity.public());
        let file = home
            .path()
            .join("delivery")
            .join("heartbeats")
            .join(&pk)
            .join("codex")
            .join("hook_inject");
        let text = std::fs::read_to_string(&file).expect("heartbeat file");
        let ts: i64 = text.trim().parse().expect("decimal ms timestamp");
        assert!(ts >= before);
        assert!(
            !home
                .path()
                .join("delivery/heartbeats")
                .join(&pk)
                .join("claude")
                .exists(),
            "a codex beat must not create the claude directory"
        );
    }

    #[test]
    fn seq_of_reads_only_an_integer_seq() {
        assert_eq!(seq_of(&json!({"seq": 7})), Some(7));
        assert_eq!(seq_of(&json!({"seq": "7"})), None);
        assert_eq!(seq_of(&json!({"seq": 1.5})), None);
        assert_eq!(seq_of(&json!({"text": "hi"})), None);
    }
}
