//! End-to-end tests of `agora hook <kind> --dialect <d>` on native harness payloads.
//!
//! Payloads come from `crates/agora-hooks/tests/fixtures/`: Codex CLI 0.157.1 captures (real) and
//! VS Code payloads built from the documented schema. Each has `cwd` set to alice's folder and
//! `session_id` to a test-local id. The hook runs from the folders root without
//! `CLAUDE_PROJECT_DIR`, so the payload's `cwd` is what names the identity. Bob posts the messages
//! alice receives. Protocol output is asserted on stdout, exit codes with `.code()`. Hooks never
//! spawn the daemon, so each test starts it with `init` first, and `Env::drop` stops it.

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

const CODEX_START: &str =
    include_str!("../../agora-hooks/tests/fixtures/codex/hook_SessionStart.json");
const CODEX_PROMPT: &str =
    include_str!("../../agora-hooks/tests/fixtures/codex/hook_UserPromptSubmit.json");
const CODEX_POST_TOOL: &str =
    include_str!("../../agora-hooks/tests/fixtures/codex/hook_PostToolUse.json");
const CODEX_STOP: &str = include_str!("../../agora-hooks/tests/fixtures/codex/hook_Stop.json");
const CODEX_STOP_ACTIVE: &str =
    include_str!("../../agora-hooks/tests/fixtures/codex/hook_Stop_second_active.json");
const CODEX_END: &str = include_str!("../../agora-hooks/tests/fixtures/codex/hook_SessionEnd.json");

const VSCODE_START: &str =
    include_str!("../../agora-hooks/tests/fixtures/vscode/SessionStart.json");
const VSCODE_PROMPT: &str =
    include_str!("../../agora-hooks/tests/fixtures/vscode/UserPromptSubmit.json");
const VSCODE_STOP: &str = include_str!("../../agora-hooks/tests/fixtures/vscode/Stop.json");

/// Both names read the VS Code payload; `copilot` detects it per invocation.
const VSCODE_DIALECTS: [&str; 2] = ["copilot-vscode", "copilot"];
/// The hook kind each native event runs, with a real payload for it.
const CODEX_HOOKS: [(&str, &str); 3] = [
    ("session-start", CODEX_START),
    ("context", CODEX_PROMPT),
    ("stop", CODEX_STOP),
];
const VSCODE_HOOKS: [(&str, &str); 3] = [
    ("session-start", VSCODE_START),
    ("context", VSCODE_PROMPT),
    ("stop", VSCODE_STOP),
];

struct Env {
    home: tempfile::TempDir,
    folders: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        // Short prefix under /tmp: the daemon socket path must fit in 104 bytes.
        let home = tempfile::Builder::new()
            .prefix("ag")
            .tempdir_in("/tmp")
            .expect("home dir");
        let folders = tempfile::tempdir().expect("folders dir");
        for who in ["alice", "bob"] {
            fs::create_dir(folders.path().join(who)).expect("user folder");
        }
        Self { home, folders }
    }

    fn folder(&self, who: &str) -> PathBuf {
        self.folders.path().join(who)
    }

    /// `agora <args>` run as `who`: that user's folder is the identity.
    fn command(&self, who: &str) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.env("AGORA_HOME", self.home.path())
            .env("AGORA_LOG", "warn")
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .current_dir(self.folder(who))
            .timeout(CMD_TIMEOUT);
        cmd
    }

    /// Runs `args --json` as `who`, requires success, and returns the parsed document.
    fn json(&self, who: &str, args: &[&str]) -> Value {
        let assert = self.command(who).args(args).arg("--json").assert();
        let out = assert.success();
        serde_json::from_slice(&out.get_output().stdout).expect("one JSON document")
    }

    /// `agora hook <args>` with `stdin`. The working directory is the folders root and
    /// `CLAUDE_PROJECT_DIR` is unset, so only the payload's `cwd` can name alice.
    fn hook(&self, args: &[&str], stdin: impl Into<Vec<u8>>) -> Assert {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.env("AGORA_HOME", self.home.path())
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .env_remove("AGORA_HOOK_BUDGET_MS")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .env_remove("CLAUDE_PROJECT_DIR")
            .current_dir(self.folders.path())
            .timeout(CMD_TIMEOUT)
            .arg("hook")
            .args(args);
        cmd.write_stdin(stdin).assert()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.command("alice").arg("stop").output();
    }
}

/// A field of `whoami` for `who`: `pubkey` or `pseudonym`.
fn whoami(env: &Env, who: &str, field: &str) -> String {
    env.json(who, &["whoami"])[field]
        .as_str()
        .expect("string field")
        .to_owned()
}

/// alice bootstraps the owner, bob registers, and alice creates `general` with bob as a member
/// (the creator is always a member), so bob's posts reach alice's inbox.
fn general_with_bob(env: &Env) {
    env.json("alice", &["init", "--owner"]);
    let bob = env.json("bob", &["init"])["pubkey"]
        .as_str()
        .expect("pubkey")
        .to_owned();
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
}

/// bob posts `text` to `general`.
fn say(env: &Env, text: &str) {
    env.json("bob", &["chat", "general", text]);
}

/// The texts of alice's unacked inbox messages, oldest first.
fn unread_texts(env: &Env) -> Vec<String> {
    env.json("alice", &["inbox"])["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| m["text"].as_str().expect("text").to_owned())
        .collect()
}

/// The fixture as captured, with `cwd` moved to alice's folder and `session_id` set to `session`.
fn payload(env: &Env, raw: &str, session: &str) -> Value {
    let mut v: Value = serde_json::from_str(raw).expect("fixture is JSON");
    let cwd = env.folder("alice");
    let fields = v.as_object_mut().expect("fixture is an object");
    fields.insert("cwd".to_owned(), json!(cwd.to_str().expect("utf-8")));
    fields.insert("session_id".to_owned(), json!(session));
    v
}

/// Runs `agora hook <args>` with `stdin`, requires exit 0 and an empty stderr, and returns stdout.
fn run(env: &Env, args: &[&str], stdin: impl Into<Vec<u8>>) -> String {
    let assert = env.hook(args, stdin).code(0);
    let out = assert.get_output();
    assert!(out.stderr.is_empty(), "stderr {:?}", out.stderr);
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

/// Runs the hook with `stdin` and requires exit 0 and no stdout (stderr is checked by `run`).
fn assert_silent(env: &Env, args: &[&str], stdin: impl Into<Vec<u8>>, what: &str) {
    let stdout = run(env, args, stdin);
    assert!(stdout.is_empty(), "{what}: stdout {stdout:?}");
}

/// The one JSON object a hook printed: a single line, anything else fails the test.
fn one_json_line(stdout: &str) -> Value {
    let body = stdout.strip_suffix('\n').unwrap_or(stdout);
    assert!(!body.contains('\n'), "exactly one line: {stdout:?}");
    serde_json::from_str(body).expect("stdout is one JSON object")
}

/// The additionalContext of a `hookSpecificOutput` line for `event`; the line holds nothing else.
fn context_of(stdout: &str, event: &str) -> String {
    let v = one_json_line(stdout);
    assert_eq!(v.as_object().map(|o| o.len()), Some(1), "{stdout}");
    let specific = &v["hookSpecificOutput"];
    assert_eq!(specific.as_object().map(|o| o.len()), Some(2), "{stdout}");
    assert_eq!(specific["hookEventName"], event, "{stdout}");
    specific["additionalContext"]
        .as_str()
        .expect("additionalContext string")
        .to_owned()
}

/// The reason of a Codex-shaped Stop block line: `decision` and `reason` only.
fn codex_block_reason(stdout: &str) -> String {
    let v = one_json_line(stdout);
    assert_eq!(v.as_object().map(|o| o.len()), Some(2), "{stdout}");
    assert_eq!(v["decision"], "block", "{stdout}");
    v["reason"].as_str().expect("reason string").to_owned()
}

/// The heartbeat file of `mechanism` for the identity with public key `pk`, under the harness
/// slug `harness` (`harness_slug` in `agora_client::heartbeat`, e.g. `codex`).
fn heartbeat_file(env: &Env, harness: &str, pk: &str, mechanism: &str) -> PathBuf {
    env.home
        .path()
        .join("delivery/heartbeats")
        .join(pk)
        .join(harness)
        .join(mechanism)
}

fn heartbeat_ms(path: &Path) -> i64 {
    let text = fs::read_to_string(path).expect("heartbeat file");
    text.trim().parse().expect("ms timestamp")
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    i64::try_from(since_epoch.as_millis()).expect("ms fits in i64")
}

#[test]
fn codex_session_start_names_alices_identity_in_hook_specific_output() {
    let env = Env::new();
    general_with_bob(&env);
    let input = payload(&env, CODEX_START, "codex-start");

    let stdout = run(
        &env,
        &["session-start", "--dialect", "codex"],
        input.to_string(),
    );
    let context = context_of(&stdout, "SessionStart");
    let who = whoami(&env, "alice", "pseudonym");
    assert!(context.contains(&who), "identity {who}: {context}");
}

#[test]
fn codex_user_prompt_submit_delivers_an_unread_message_as_framed_text() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "codex prompt message");
    let input = payload(&env, CODEX_PROMPT, "codex-prompt");

    let stdout = run(&env, &["context", "--dialect", "codex"], input.to_string());
    let context = context_of(&stdout, "UserPromptSubmit");
    assert!(context.contains("<untrusted-message "), "{context}");
    assert!(
        context.contains("codex prompt message\n</untrusted-message>"),
        "{context}"
    );
}

#[test]
fn codex_user_prompt_submit_is_silent_on_the_second_identical_run() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "delivered once");
    let input = payload(&env, CODEX_PROMPT, "codex-repeat").to_string();
    let first = run(&env, &["context", "--dialect", "codex"], input.clone());
    assert!(first.contains("delivered once"), "{first}");

    assert_silent(
        &env,
        &["context", "--dialect", "codex"],
        input,
        "second run",
    );
}

#[test]
fn codex_stop_blocks_while_a_message_is_unread_and_quotes_it_in_the_reason() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "codex stop message");
    let input = payload(&env, CODEX_STOP, "codex-stop");

    let stdout = run(&env, &["stop", "--dialect", "codex"], input.to_string());
    let reason = codex_block_reason(&stdout);
    assert!(reason.contains("codex stop message"), "{reason}");
}

#[test]
fn codex_stop_is_silent_when_stop_hook_active_is_true() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "pending but already continuing");
    let input = payload(&env, CODEX_STOP_ACTIVE, "codex-active");

    assert_silent(
        &env,
        &["stop", "--dialect", "codex"],
        input.to_string(),
        "active",
    );
}

#[test]
fn codex_session_end_is_silent_for_every_hook_it_could_reach() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "pending during session end");
    let input = payload(&env, CODEX_END, "codex-end").to_string();

    for kind in ["session-start", "context", "stop"] {
        let what = format!("SessionEnd through {kind}");
        assert_silent(&env, &[kind, "--dialect", "codex"], input.clone(), &what);
    }
}

#[test]
fn codex_post_tool_use_on_the_agora_chat_tool_delivers_a_new_message() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "seen after a chat call");
    let mut input = payload(&env, CODEX_POST_TOOL, "codex-post");
    input["tool_name"] = json!("mcp__agora__chat");

    let stdout = run(&env, &["context", "--dialect", "codex"], input.to_string());
    let context = context_of(&stdout, "PostToolUse");
    assert!(
        context.contains("seen after a chat call\n</untrusted-message>"),
        "{context}"
    );
}

#[test]
fn codex_hooks_write_hook_inject_and_stop_continue_heartbeats_for_alice() {
    let env = Env::new();
    general_with_bob(&env);
    let pk = whoami(&env, "alice", "pubkey");
    say(&env, "beat");

    let before = now_ms();
    let prompt = payload(&env, CODEX_PROMPT, "codex-beats").to_string();
    run(&env, &["context", "--dialect", "codex"], prompt);
    let stop = payload(&env, CODEX_STOP, "codex-beats").to_string();
    run(&env, &["stop", "--dialect", "codex"], stop);
    let after = now_ms();

    for mechanism in ["hook_inject", "stop_continue"] {
        let beat = heartbeat_ms(&heartbeat_file(&env, "codex", &pk, mechanism));
        assert!((before..=after).contains(&beat), "{mechanism}: {beat}");
    }
}

#[test]
fn codex_delivery_leaves_the_messages_unacked_in_alices_inbox() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "still unacked");
    let prompt = payload(&env, CODEX_PROMPT, "codex-unacked").to_string();
    run(&env, &["context", "--dialect", "codex"], prompt);
    let stop = payload(&env, CODEX_STOP, "codex-unacked").to_string();
    run(&env, &["stop", "--dialect", "codex"], stop);

    assert_eq!(unread_texts(&env), ["still unacked"]);
}

#[test]
fn vscode_session_start_names_alices_identity_for_both_dialect_names() {
    let env = Env::new();
    general_with_bob(&env);
    let who = whoami(&env, "alice", "pseudonym");

    for dialect in VSCODE_DIALECTS {
        let input = payload(&env, VSCODE_START, &format!("vscode-{dialect}"));
        let stdout = run(
            &env,
            &["session-start", "--dialect", dialect],
            input.to_string(),
        );
        let context = context_of(&stdout, "SessionStart");
        assert!(context.contains(&who), "{dialect}: {context}");
    }
}

#[test]
fn vscode_stop_with_an_unread_message_blocks_in_hook_specific_output() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "vscode stop message");

    for dialect in VSCODE_DIALECTS {
        let input = payload(&env, VSCODE_STOP, &format!("stop-{dialect}"));
        let stdout = run(&env, &["stop", "--dialect", dialect], input.to_string());
        let v = one_json_line(&stdout);
        let specific = &v["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "Stop", "{dialect}: {stdout}");
        assert_eq!(specific["decision"], "block", "{dialect}: {stdout}");
        let reason = specific["reason"].as_str().expect("reason string");
        assert!(
            reason.contains("vscode stop message"),
            "{dialect}: {reason}"
        );
        assert_eq!(v["decision"], "block", "{dialect}: {stdout}");
    }
}

#[test]
fn vscode_payloads_with_an_empty_session_id_are_silent_for_every_hook() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "pending for an anonymous session");

    for dialect in VSCODE_DIALECTS {
        for (kind, raw) in VSCODE_HOOKS {
            let mut input = payload(&env, raw, "unused");
            input["session_id"] = json!("");
            let what = format!("{dialect} {kind} with an empty session_id");
            assert_silent(
                &env,
                &[kind, "--dialect", dialect],
                input.to_string(),
                &what,
            );
        }
    }
}

#[test]
fn garbage_stdin_is_silent_with_exit_0_for_codex_and_vscode_hooks() {
    let env = Env::new();
    general_with_bob(&env);
    // A real unread message: only the malformed input may keep each hook quiet.
    say(&env, "pending under garbage");
    let inputs: [(&str, Vec<u8>); 5] = [
        ("not json", b"{not json".to_vec()),
        ("invalid utf-8", vec![0xff, 0xfe, b'{', b'}']),
        ("an array", b"[]".to_vec()),
        ("a number", b"42".to_vec()),
        ("empty", Vec::new()),
    ];
    for dialect in ["codex", "copilot-vscode", "copilot"] {
        for kind in ["session-start", "context", "stop"] {
            for (label, bytes) in &inputs {
                let what = format!("{dialect} {kind} with {label} stdin");
                assert_silent(&env, &[kind, "--dialect", dialect], bytes.clone(), &what);
            }
        }
    }
}

#[test]
fn a_utf8_bom_before_codex_json_is_stripped_and_the_payload_parses() {
    let env = Env::new();
    general_with_bob(&env);
    let who = whoami(&env, "alice", "pseudonym");
    let mut bytes = BOM.to_vec();
    bytes.extend(
        payload(&env, CODEX_START, "codex-bom")
            .to_string()
            .into_bytes(),
    );

    let stdout = run(&env, &["session-start", "--dialect", "codex"], bytes);
    let context = context_of(&stdout, "SessionStart");
    assert!(context.contains(&who), "{context}");
}

#[test]
fn a_utf8_bom_before_vscode_json_is_stripped_and_the_payload_parses() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "bom stop message");
    let mut bytes = BOM.to_vec();
    bytes.extend(
        payload(&env, VSCODE_STOP, "vscode-bom")
            .to_string()
            .into_bytes(),
    );

    let stdout = run(&env, &["stop", "--dialect", "copilot-vscode"], bytes);
    let specific = one_json_line(&stdout)["hookSpecificOutput"].clone();
    assert_eq!(specific["decision"], "block", "{stdout}");
    let reason = specific["reason"].as_str().expect("reason string");
    assert!(reason.contains("bom stop message"), "{reason}");
}

#[test]
fn an_unknown_dialect_runs_nothing_and_exits_0_for_every_hook() {
    let env = Env::new();
    general_with_bob(&env);
    // Each payload is one a known dialect acts on, so silence means the hook did not run.
    say(&env, "pending for an unknown dialect");

    for (kind, raw) in CODEX_HOOKS {
        let input = payload(&env, raw, "unknown-dialect").to_string();
        assert_silent(&env, &[kind, "--dialect", "zzz"], input, kind);
    }
}
