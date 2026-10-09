//! `run`'s dialect gate and folder rule, and the seeded-home runs of a dialect's session start.
//! A non-Claude dialect is silent for `rewake` and `poll` before stdin is read. A non-Claude run
//! acts for its payload's `cwd` when `AGORA_FOLDER` names no folder. A Copilot 0.0.369 payload
//! (no session id) is silent and touches no state, while a Codex payload in the same seeded home
//! produces the native SessionStart output.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use agora_daemon::Paths;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::state::sessions_dir;
use crate::{HookKind, HookOptions, HookOutcome, dialect_folder, run};

/// The real Copilot CLI 0.0.369 sessionStart payload (no session id), VERIFIED-LOCAL.
const COPILOT_SESSION_START: &str =
    include_str!("../tests/fixtures/copilot/hook_sessionStart.json");
/// The real Codex 0.157.1 SessionStart payload, VERIFIED-LOCAL.
const CODEX_SESSION_START: &str = include_str!("../tests/fixtures/codex/hook_SessionStart.json");

/// Reports whether it was read, and yields EOF (so a read that is not gated is silent, not a
/// panic).
struct FlagReader<'a>(&'a AtomicBool);

impl Read for FlagReader<'_> {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.store(true, Ordering::SeqCst);
        Ok(0)
    }
}

/// Options for `dialect` on a fresh home. The `TempDir` must outlive the run.
fn options(dialect: Dialect) -> (tempfile::TempDir, HookOptions) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = HookOptions::new(dir.path().to_path_buf());
    opts.dialect = dialect;
    (dir, opts)
}

fn non_claude() -> impl Iterator<Item = Dialect> {
    Dialect::ALL.into_iter().filter(|d| *d != Dialect::Claude)
}

/// A temp home holding a machine seed, and a temp folder to act as (both under `/tmp`, as in the
/// `session_start` tests).
fn seeded_home() -> (tempfile::TempDir, tempfile::TempDir) {
    let temp = || {
        tempfile::Builder::new()
            .prefix("ah")
            .tempdir_in("/tmp")
            .expect("temp dir")
    };
    let home = temp();
    agora_client::identity::load_or_create_machine_seed(&Paths::new(home.path()))
        .expect("machine seed");
    (home, temp())
}

/// `json` with its `cwd` replaced by `folder`, serialized again.
fn with_cwd(json: &str, folder: &Path) -> String {
    let mut value: Value = serde_json::from_str(json).expect("fixture is JSON");
    value["cwd"] = Value::String(folder.to_string_lossy().into_owned());
    value.to_string()
}

#[test]
fn non_claude_rewake_is_silent_without_reading_stdin() {
    for dialect in non_claude() {
        let (_dir, opts) = options(dialect);
        let read = AtomicBool::new(false);
        let out = run(HookKind::Rewake, &opts, &mut FlagReader(&read));
        assert_eq!(out, HookOutcome::silent(), "{dialect:?}");
        assert!(!read.load(Ordering::SeqCst), "{dialect:?} read stdin");
    }
}

#[test]
fn non_claude_poll_is_silent_without_reading_stdin() {
    for dialect in non_claude() {
        let (_dir, opts) = options(dialect);
        let read = AtomicBool::new(false);
        let out = run(HookKind::Poll, &opts, &mut FlagReader(&read));
        assert_eq!(out, HookOutcome::silent(), "{dialect:?}");
        assert!(!read.load(Ordering::SeqCst), "{dialect:?} read stdin");
    }
}

#[test]
fn claude_rewake_still_reads_stdin() {
    // Control for the gate above: the Claude path does read stdin for rewake.
    let (_dir, opts) = options(Dialect::Claude);
    let read = AtomicBool::new(false);
    let out = run(HookKind::Rewake, &opts, &mut FlagReader(&read));
    assert_eq!(out, HookOutcome::silent());
    assert!(
        read.load(Ordering::SeqCst),
        "Claude rewake did not read stdin"
    );
}

#[test]
fn copilot_0_0_369_session_start_is_silent_and_touches_nothing() {
    // The fixture has no session id: a legacy invocation whose output the CLI ignores. Its cwd is
    // the seeded folder, so a run that did act would produce output; the silence is the dialect's.
    let (home, folder) = seeded_home();
    let mut opts = HookOptions::new(home.path().to_path_buf());
    opts.dialect = Dialect::Copilot;
    opts.event = Some("sessionStart".to_owned());
    let payload = with_cwd(COPILOT_SESSION_START, folder.path());
    let out = run(HookKind::SessionStart, &opts, &mut payload.as_bytes());
    assert_eq!(out, HookOutcome::silent());
    assert!(!sessions_dir(home.path()).exists(), "session state written");
    assert!(
        !home.path().join("delivery").join("heartbeats").exists(),
        "heartbeat written"
    );
}

#[test]
fn codex_session_start_in_the_same_seeded_home_is_delivered() {
    // Control for the Copilot case: the same home and folder, with a real Codex payload, produces
    // the SessionStart output and writes the session state.
    let (home, folder) = seeded_home();
    let mut opts = HookOptions::new(home.path().to_path_buf());
    opts.dialect = Dialect::Codex;
    opts.folder = Some(folder.path().to_path_buf());
    let payload = with_cwd(CODEX_SESSION_START, folder.path());
    let out = run(HookKind::SessionStart, &opts, &mut payload.as_bytes());
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("\"hookEventName\":\"SessionStart\""),
        "{out:?}"
    );
    assert!(sessions_dir(home.path()).exists(), "no session state");
}

#[test]
fn dialect_folder_takes_the_payload_cwd_when_agora_folder_names_none() {
    for dialect in non_claude() {
        for env in [
            None,
            Some(OsString::new()),
            Some(OsString::from("${workspaceFolder}")),
        ] {
            let got = dialect_folder(dialect, env.clone(), Some("/work/p"));
            assert_eq!(got, Some(PathBuf::from("/work/p")), "{dialect:?} {env:?}");
        }
    }
}

#[test]
fn dialect_folder_keeps_a_usable_agora_folder() {
    for dialect in non_claude() {
        let env = Some(OsString::from("/env/p"));
        assert_eq!(
            dialect_folder(dialect, env, Some("/work/p")),
            None,
            "{dialect:?}"
        );
    }
}

#[test]
fn dialect_folder_needs_a_non_empty_cwd() {
    assert_eq!(dialect_folder(Dialect::Codex, None, None), None);
    assert_eq!(dialect_folder(Dialect::Codex, None, Some("")), None);
}

#[test]
fn dialect_folder_is_never_set_for_claude() {
    assert_eq!(dialect_folder(Dialect::Claude, None, Some("/work/p")), None);
    let placeholder = Some(OsString::from("${x}"));
    assert_eq!(
        dialect_folder(Dialect::Claude, placeholder, Some("/work/p")),
        None
    );
}
