//! Process tests of `agora hook <kind> --dialect <d> --event <E>` for the Copilot CLI, Antigravity
//! (`agy`) and Cursor dialects, against a real auto-spawned daemon, as `hooks.rs` does for Claude
//! Code. Each run gets a native payload from `crates/agora-hooks/tests/fixtures/` with alice's
//! folder and a fresh session id in place of the captured ones. The copilot payloads captured from
//! 0.0.369 carry no session id and must be ignored; the docs-derived ones carry one and must get
//! the native output. Protocol output is asserted on stdout, exit codes with `.code()`.

use agora_hooks::DEFAULT_MAX_BLOCKS;
use agora_hooks::state;
use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

const COPILOT_SESSION_START: &[&str] = &[
    "session-start",
    "--dialect",
    "copilot",
    "--event",
    "sessionStart",
];
const COPILOT_POST_TOOL_USE: &[&str] =
    &["context", "--dialect", "copilot", "--event", "postToolUse"];
const COPILOT_PROMPT: &[&str] = &[
    "context",
    "--dialect",
    "copilot",
    "--event",
    "userPromptSubmitted",
];
const COPILOT_AGENT_STOP: &[&str] = &["stop", "--dialect", "copilot", "--event", "agentStop"];
const AGY_PRE_INVOCATION: &[&str] = &["context", "--dialect", "agy", "--event", "PreInvocation"];
const AGY_POST_TOOL_USE: &[&str] = &["context", "--dialect", "agy", "--event", "PostToolUse"];
const AGY_STOP: &[&str] = &["stop", "--dialect", "agy", "--event", "Stop"];
const CURSOR_SESSION_START: &[&str] = &["session-start", "--dialect", "cursor"];
const CURSOR_CONTEXT: &[&str] = &["context", "--dialect", "cursor"];
const CURSOR_STOP: &[&str] = &["stop", "--dialect", "cursor"];

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

    /// An `agora` invocation run as `who`: that user's folder is the identity, the shared home
    /// holds the daemon.
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

    /// Run with `--json`, require success, and return the parsed stdout document.
    fn json(&self, who: &str, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let assert = self.command(who).args(&full).assert().success();
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is one JSON document")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.command("alice").arg("stop").output();
    }
}

/// One string field of `agora whoami` for `who`, such as `pubkey` or `pseudonym`.
fn whoami(env: &Env, who: &str, field: &str) -> String {
    env.json(who, &["whoami"])[field]
        .as_str()
        .expect("whoami string")
        .to_owned()
}

/// alice bootstraps the owner, bob registers, and alice creates `general` with bob as a member
/// (the creator is always a member), so bob's posts reach alice's inbox.
fn general_with_bob(env: &Env) {
    env.json("alice", &["init", "--owner"]);
    let bob = env.json("bob", &["init"])["pubkey"]
        .as_str()
        .expect("pubkey string")
        .to_owned();
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
}

/// bob posts `text` to `general`.
fn say(env: &Env, text: &str) {
    env.json("bob", &["chat", "general", text]);
}

/// Alice's unacked inbox message count.
fn unread_count(env: &Env) -> usize {
    env.json("alice", &["inbox"])["messages"]
        .as_array()
        .expect("messages array")
        .len()
}

/// The native payload `name` from the agora-hooks fixtures (`copilot/hook_sessionStart.json`),
/// aimed at alice's folder and `session`. Each identity field the fixture has is replaced; a
/// field it lacks stays absent, so a 0.0.369 copilot payload keeps no session id.
fn payload(env: &Env, name: &str, session: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agora-hooks/tests/fixtures")
        .join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut value: Value = serde_json::from_str(&text).expect("fixture is JSON");
    let folder = env.folder("alice").to_str().expect("utf-8 path").to_owned();
    for (key, field) in value.as_object_mut().expect("object payload").iter_mut() {
        match key.as_str() {
            "cwd" => *field = json!(folder),
            "workspacePaths" | "workspace_roots" => *field = json!([folder]),
            "sessionId" | "conversationId" | "conversation_id" | "session_id" => {
                *field = json!(session);
            }
            _ => {}
        }
    }
    value
}

/// `agora hook <args>` as alice, with `input` on stdin.
fn hook(env: &Env, args: &[&str], input: &Value) -> Assert {
    let mut cmd = Command::cargo_bin("agora").expect("agora binary");
    cmd.env("AGORA_HOME", env.home.path())
        .env("CLAUDE_PROJECT_DIR", env.folder("alice"))
        .env_remove("AGORA_FOLDER")
        .env_remove("AGORA_NO_SPAWN")
        .env_remove("AGORA_HOOK_BUDGET_MS")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .current_dir(env.folder("alice"))
        .timeout(CMD_TIMEOUT)
        .arg("hook")
        .args(args);
    cmd.write_stdin(input.to_string()).assert()
}

/// Runs the hook, requires exit 0 and nothing on stderr, and returns its stdout.
fn stdout_of(env: &Env, args: &[&str], input: &Value) -> String {
    let assert = hook(env, args, input).code(0);
    let out = assert.get_output();
    assert!(
        out.stderr.is_empty(),
        "hooks print nothing on stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

/// Runs the hook and requires exactly one JSON object as its one line of stdout.
fn run(env: &Env, args: &[&str], input: &Value) -> Value {
    let text = stdout_of(env, args, input);
    let body = text.strip_suffix('\n').unwrap_or(text.as_str());
    assert!(!body.contains('\n'), "exactly one line: {text:?}");
    serde_json::from_str(body).expect("stdout is one JSON object")
}

/// Runs the hook and requires that it says nothing.
fn silent(env: &Env, args: &[&str], input: &Value) {
    let text = stdout_of(env, args, input);
    assert!(text.is_empty(), "{args:?}: stdout {text:?}");
}

/// The object's keys, sorted.
fn keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

/// The string at a JSON pointer, which must exist.
fn text_at(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} in {v}"))
        .to_owned()
}

/// The heartbeat file of `mechanism` for the identity with public key `pk`, under the harness
/// slug `harness` (`harness_slug` in `agora_client::heartbeat`, e.g. `copilot`).
fn heartbeat_file(env: &Env, harness: &str, pk: &str, mechanism: &str) -> PathBuf {
    env.home
        .path()
        .join(format!("delivery/heartbeats/{pk}/{harness}/{mechanism}"))
}

#[test]
fn copilot_0_0_369_payloads_without_a_session_id_are_silent_and_leave_no_state() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "legacy note");
    let start = payload(&env, "copilot/hook_sessionStart.json", "");
    let tool = payload(&env, "copilot/hook_postToolUse.json", "");

    silent(&env, COPILOT_SESSION_START, &start);
    silent(&env, COPILOT_POST_TOOL_USE, &tool);

    let pk = whoami(&env, "alice", "pubkey");
    for mechanism in ["hook_inject", "stop_continue", "async_rewake_hook"] {
        assert!(
            !heartbeat_file(&env, "copilot", &pk, mechanism).exists(),
            "{mechanism} heartbeat written for an ignored payload"
        );
    }
    assert!(
        !env.home.path().join("hooks").join("sessions").exists(),
        "no hook session state for an ignored payload"
    );
}

#[test]
fn copilot_newer_session_start_adds_the_identity_as_additional_context() {
    let env = Env::new();
    general_with_bob(&env);
    let pseudonym = whoami(&env, "alice", "pseudonym");
    let start = payload(&env, "copilot/newer_sessionStart.json", "s-cop-start");

    let v = run(&env, COPILOT_SESSION_START, &start);
    assert_eq!(keys(&v), ["additionalContext"]);
    let context = text_at(&v, "/additionalContext");
    assert!(context.contains(&pseudonym), "identity line: {context}");
}

#[test]
fn copilot_newer_post_tool_use_adds_a_bob_message_as_framed_additional_context() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "copilot tool note");
    let mut tool = payload(&env, "copilot/newer_postToolUse.json", "s-cop-tool");
    // The docs example reads the inbox, and an inbox read absorbs what it showed instead of
    // delivering. This test is about another tool's result, so the message is delivered.
    tool["toolName"] = json!("agora-chat");

    let v = run(&env, COPILOT_POST_TOOL_USE, &tool);
    assert_eq!(keys(&v), ["additionalContext"]);
    let context = text_at(&v, "/additionalContext");
    assert!(context.contains("copilot tool note"), "{context}");
    assert!(context.contains("<untrusted-message "), "framed: {context}");
}

#[test]
fn copilot_newer_agent_stop_blocks_while_a_message_is_unread() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "copilot stop note");
    let stop = payload(&env, "copilot/newer_agentStop.json", "s-cop-stop");

    let v = run(&env, COPILOT_AGENT_STOP, &stop);
    assert_eq!(keys(&v), ["decision", "reason"]);
    assert_eq!(v["decision"], "block");
    let reason = text_at(&v, "/reason");
    assert!(reason.contains("copilot stop note"), "{reason}");
}

#[test]
fn copilot_newer_agent_stop_with_the_stop_hook_already_active_is_silent() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "copilot active note");
    let stop = payload(&env, "copilot/newer_agentStop_active.json", "s-cop-active");

    silent(&env, COPILOT_AGENT_STOP, &stop);
}

#[test]
fn copilot_user_prompt_submitted_is_silent_because_the_cli_cannot_display_context() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "copilot prompt note");
    // The 0.0.369 capture has no session id; the newer shape adds one.
    let mut prompt = payload(&env, "copilot/hook_userPromptSubmitted.json", "");
    prompt["sessionId"] = json!("s-cop-prompt");

    silent(&env, COPILOT_PROMPT, &prompt);
}

#[test]
fn agy_pre_invocation_injects_a_bob_message_as_an_ephemeral_step_once() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "agy pre note");
    let pre = payload(&env, "agy/PreInvocation.json", "s-agy-pre");

    let v = run(&env, AGY_PRE_INVOCATION, &pre);
    assert_eq!(keys(&v), ["injectSteps"]);
    let steps = v["injectSteps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 1, "{v}");
    assert_eq!(keys(&steps[0]), ["ephemeralMessage"]);
    let context = text_at(&v, "/injectSteps/0/ephemeralMessage");
    assert!(context.contains("agy pre note"), "{context}");
    assert!(context.contains("<untrusted-message "), "framed: {context}");

    silent(&env, AGY_PRE_INVOCATION, &pre);
    assert_eq!(unread_count(&env), 1, "delivery never acks the message");
}

#[test]
fn agy_stop_with_unread_continues_the_loop_with_the_message_in_the_reason() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "agy stop note");
    let stop = payload(&env, "agy/Stop.json", "s-agy-stop");

    let v = run(&env, AGY_STOP, &stop);
    assert_eq!(keys(&v), ["decision", "reason"]);
    assert_eq!(v["decision"], "continue");
    let reason = text_at(&v, "/reason");
    assert!(reason.contains("agy stop note"), "{reason}");
}

#[test]
fn agy_stop_with_nothing_unread_is_silent_so_the_loop_can_end() {
    let env = Env::new();
    general_with_bob(&env);
    let stop = payload(&env, "agy/Stop.json", "s-agy-empty");

    silent(&env, AGY_STOP, &stop);
}

#[test]
fn agy_post_tool_use_is_silent_even_with_a_message_unread() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "agy tool note");
    let tool = payload(&env, "agy/PostToolUse.json", "s-agy-tool");

    silent(&env, AGY_POST_TOOL_USE, &tool);
}

#[test]
fn agy_stop_blocks_at_most_the_default_cap_of_consecutive_times_then_goes_silent() {
    let env = Env::new();
    general_with_bob(&env);
    let stop = payload(&env, "agy/Stop.json", "s-agy-cap");

    for i in 1..=DEFAULT_MAX_BLOCKS {
        let text = format!("cap note {i}");
        say(&env, &text);
        let v = run(&env, AGY_STOP, &stop);
        assert_eq!(v["decision"], "continue", "block {i}: {v}");
        let reason = text_at(&v, "/reason");
        assert!(reason.contains(&text), "block {i}: {reason}");
    }

    say(&env, &format!("cap note {}", DEFAULT_MAX_BLOCKS + 1));
    silent(&env, AGY_STOP, &stop);
}

#[test]
fn cursor_session_start_adds_the_identity_as_additional_context() {
    let env = Env::new();
    general_with_bob(&env);
    let pseudonym = whoami(&env, "alice", "pseudonym");
    let start = payload(&env, "cursor/sessionStart.json", "s-cur-start");

    let v = run(&env, CURSOR_SESSION_START, &start);
    assert_eq!(keys(&v), ["additional_context"]);
    let context = text_at(&v, "/additional_context");
    assert!(context.contains(&pseudonym), "identity line: {context}");
}

#[test]
fn cursor_post_tool_use_adds_a_bob_message_as_additional_context() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor tool note");
    let tool = payload(&env, "cursor/postToolUse.json", "s-cur-tool");

    let v = run(&env, CURSOR_CONTEXT, &tool);
    assert_eq!(keys(&v), ["additional_context"]);
    let context = text_at(&v, "/additional_context");
    assert!(context.contains("cursor tool note"), "{context}");
}

#[test]
fn cursor_stop_with_unread_sends_a_followup_message_with_the_message() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor stop note");
    let stop = payload(&env, "cursor/stop.json", "s-cur-stop");

    let v = run(&env, CURSOR_STOP, &stop);
    assert_eq!(keys(&v), ["followup_message"]);
    let followup = text_at(&v, "/followup_message");
    assert!(followup.contains("cursor stop note"), "{followup}");
}

#[test]
fn cursor_stop_below_the_loop_limit_sends_the_followup_and_advances_the_cursor() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor loop note");
    let mut stop = payload(&env, "cursor/stop.json", "s-cur-loop4");
    // Cursor's loop_limit is 5: the fifth follow-up is still delivered.
    stop["loop_count"] = json!(4);

    let v = run(&env, CURSOR_STOP, &stop);
    let followup = text_at(&v, "/followup_message");
    assert!(followup.contains("cursor loop note"), "{followup}");
    assert!(state::load(env.home.path(), "s-cur-loop4").cursor > 0);
}

#[test]
fn cursor_stop_at_the_loop_limit_is_silent_and_leaves_the_cursor_for_later() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor limit note");
    let mut stop = payload(&env, "cursor/stop.json", "s-cur-limit");
    stop["loop_count"] = json!(5);

    silent(&env, CURSOR_STOP, &stop);
    assert_eq!(state::load(env.home.path(), "s-cur-limit").cursor, 0);

    stop["loop_count"] = json!(0);
    let v = run(&env, CURSOR_STOP, &stop);
    let followup = text_at(&v, "/followup_message");
    assert!(followup.contains("cursor limit note"), "{followup}");
}

#[test]
fn cursor_after_mcp_chat_is_silent_and_leaves_the_message_for_the_next_post_tool_use() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor chat note");
    let mut chat = payload(&env, "cursor/afterMCPExecution.json", "s-cur-chat");
    chat["tool_name"] = json!("chat");

    silent(&env, CURSOR_CONTEXT, &chat);

    let tool = payload(&env, "cursor/postToolUse.json", "s-cur-chat");
    let v = run(&env, CURSOR_CONTEXT, &tool);
    let context = text_at(&v, "/additional_context");
    assert!(context.contains("cursor chat note"), "{context}");
}

#[test]
fn cursor_after_mcp_inbox_is_silent_and_moves_the_cursor_to_the_agora_seq() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor inbox note");
    let mut inbox = payload(&env, "cursor/afterMCPExecution.json", "s-cur-inbox");
    inbox["result_json"] = json!(r#"{"messages":[{"seq":7}]}"#);

    silent(&env, CURSOR_CONTEXT, &inbox);
    assert_eq!(state::load(env.home.path(), "s-cur-inbox").cursor, 7);
}

#[test]
fn cursor_after_mcp_inbox_from_another_server_does_not_move_the_cursor() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor foreign note");
    let mut foreign = payload(&env, "cursor/afterMCPExecution.json", "s-cur-foreign");
    foreign["mcp_server_name"] = json!("other");
    foreign["result_json"] = json!(r#"{"messages":[{"seq":999999999}]}"#);

    silent(&env, CURSOR_CONTEXT, &foreign);
    assert_eq!(state::load(env.home.path(), "s-cur-foreign").cursor, 0);
}

#[test]
fn cursor_before_submit_prompt_is_silent_because_it_cannot_add_context() {
    let env = Env::new();
    general_with_bob(&env);
    say(&env, "cursor prompt note");
    let prompt = payload(&env, "cursor/beforeSubmitPrompt.json", "s-cur-prompt");

    silent(&env, CURSOR_CONTEXT, &prompt);
}
