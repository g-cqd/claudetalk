//! End-to-end tests of the `agora` binary against a real auto-spawned daemon.
//!
//! Every test gets its own `Env`: one AGORA_HOME shared by all users of that test, and
//! one folder per user. A folder is an identity (the machine seed is per home, the key
//! per folder), so "alice" and "bob" are distinct members of one local organisation.
//! `Env::drop` stops the daemon, so no daemon outlives its test.

use agora_daemon::Paths;
use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

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
        for who in ["alice", "bob", "carol"] {
            fs::create_dir(folders.path().join(who)).expect("user folder");
        }
        Self { home, folders }
    }

    fn paths(&self) -> Paths {
        Paths::new(self.home.path())
    }

    fn folder(&self, who: &str) -> PathBuf {
        self.folders.path().join(who)
    }

    /// An `agora` invocation run as `who`: that user's folder is the identity, the
    /// shared home holds the daemon.
    fn command(&self, who: &str, log: &str) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.env("AGORA_HOME", self.home.path())
            .env("AGORA_LOG", log)
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .current_dir(self.folder(who))
            .timeout(CMD_TIMEOUT);
        cmd
    }

    fn agora(&self, who: &str, args: &[&str]) -> Assert {
        self.command(who, "warn").args(args).assert()
    }

    /// Run with `--json`, require success, and return the parsed stdout document.
    fn json(&self, who: &str, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let assert = self.agora(who, &full).success();
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is one JSON document")
    }

    /// Run with `--json`, require exit code 1, and return the parsed error document.
    fn json_error(&self, who: &str, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let assert = self.agora(who, &full).code(1);
        serde_json::from_slice(&assert.get_output().stdout).expect("error is one JSON document")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.command("alice", "warn").arg("stop").output();
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn pubkey(env: &Env, who: &str) -> String {
    env.json(who, &["whoami"])["pubkey"]
        .as_str()
        .expect("pubkey string")
        .to_owned()
}

/// alice bootstraps the owner, bob registers. Returns bob's public key.
fn owner_and_member(env: &Env) -> String {
    env.json("alice", &["init", "--owner"]);
    env.json("bob", &["init"])["pubkey"]
        .as_str()
        .expect("pubkey string")
        .to_owned()
}

/// 4 KiB holding every byte value, so any corruption shows.
fn sample_bytes() -> Vec<u8> {
    (0..=255u8).cycle().take(4096).collect()
}

/// alice creates `general` with bob as a member and posts `bytes` as `report.bin`.
fn post_attachment(env: &Env, bob: &str, bytes: &[u8]) {
    let path = env.folder("alice").join("report.bin");
    fs::write(&path, bytes).expect("write attachment");
    let path = path.to_str().expect("utf-8 path");
    env.json("alice", &["channel", "create", "general", "--member", bob]);
    env.json("alice", &["chat", "general", "see file", "--attach", path]);
}

#[test]
fn owner_init_registers_this_folder_as_owner() {
    let env = Env::new();
    let who = env.json("alice", &["init", "--owner"]);
    assert_eq!(who["role"], "owner");
    assert!(is_hex64(who["pubkey"].as_str().expect("pubkey string")));
    assert_eq!(who["registered"], true);
    assert_eq!(who["revoked"], false);
    assert_eq!(who["owner_exists"], true);
}

#[test]
fn whoami_is_stable_per_folder_and_differs_between_folders() {
    let env = Env::new();
    let first = pubkey(&env, "alice");
    let again = pubkey(&env, "alice");
    let other = pubkey(&env, "bob");
    assert!(is_hex64(&first));
    assert_eq!(first, again);
    assert_ne!(first, other);
}

#[test]
fn first_command_auto_spawns_the_daemon() {
    let env = Env::new();
    let socket = env.paths().socket;
    assert!(!socket.exists(), "no daemon before the first command");
    env.json("alice", &["whoami"]);
    assert!(socket.exists(), "the command left a daemon socket behind");
}

#[test]
fn stop_shuts_the_daemon_down_and_a_second_stop_reports_not_running() {
    let env = Env::new();
    env.json("alice", &["whoami"]);
    assert_eq!(env.json("alice", &["stop"]), json!({"stopped": true}));
    assert_eq!(env.json("alice", &["stop"]), json!({"stopped": false}));
}

#[test]
fn member_inbox_shows_the_plain_message_verified_and_from_the_sender() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    let alice = pubkey(&env, "alice");
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
    env.json("alice", &["chat", "general", "hello"]);

    let inbox = env.json("bob", &["inbox"]);
    let messages = inbox["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "exactly the one posted message: {inbox}");
    let m = &messages[0];
    assert_eq!(m["text"], "hello");
    assert_eq!(m["verified"], true);
    assert_eq!(m["sender"], alice);
    assert_eq!(m["channel"], "general");
    assert_eq!(m["encrypted"], false);
    assert_eq!(m["attachments"], json!([]));
}

#[test]
fn read_returns_the_same_messages_as_the_inbox() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
    env.json("alice", &["chat", "general", "hello"]);

    let inbox = env.json("bob", &["inbox"]);
    let read = env.json("bob", &["read", "general"]);
    assert_eq!(read["messages"], inbox["messages"]);
    assert_eq!(read["messages"][0]["text"], "hello");
}

#[test]
fn ack_through_a_seq_empties_the_inbox() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
    env.json("alice", &["chat", "general", "hello"]);

    let seq = env.json("bob", &["inbox"])["messages"][0]["seq"]
        .as_i64()
        .expect("seq");
    env.json("bob", &["ack", "general", &seq.to_string()]);

    assert_eq!(env.json("bob", &["inbox"])["messages"], json!([]));
}

#[test]
fn encrypted_message_is_readable_by_a_member_and_marked_encrypted() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json(
        "alice",
        &["channel", "create", "sec", "--encrypt", "--member", &bob],
    );
    env.json("alice", &["chat", "sec", "secret"]);

    let read = env.json("bob", &["read", "sec"]);
    let m = &read["messages"][0];
    assert_eq!(m["text"], "secret");
    assert_eq!(m["encrypted"], true);
    assert_eq!(m["verified"], true);
}

#[test]
fn removed_member_cannot_read_an_encrypted_channel() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json(
        "alice",
        &["channel", "create", "sec", "--encrypt", "--member", &bob],
    );
    env.json("alice", &["chat", "sec", "secret"]);
    env.json("alice", &["channel", "remove", "sec", &bob]);

    let err = env.json_error("bob", &["read", "sec"]);
    assert_eq!(err["error"]["code"], "not_member");
}

#[test]
fn inbox_wait_on_an_empty_inbox_times_out_after_the_requested_seconds() {
    let env = Env::new();
    owner_and_member(&env);

    let started = Instant::now();
    let inbox = env.json("bob", &["inbox", "--wait", "1"]);
    let elapsed = started.elapsed();

    assert_eq!(inbox["messages"], json!([]));
    assert_eq!(inbox["timed_out"], true);
    assert_eq!(inbox["cursor"], 0);
    assert!(
        elapsed >= Duration::from_secs(1),
        "returned after {elapsed:?}"
    );
}

#[test]
fn read_lists_the_attachment_with_its_name_and_hash() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    post_attachment(&env, &bob, &sample_bytes());

    let read = env.json("bob", &["read", "general"]);
    let attachments = read["messages"][0]["attachments"]
        .as_array()
        .expect("attachments array");
    assert_eq!(attachments.len(), 1, "one attachment: {read}");
    assert_eq!(attachments[0]["name"], "report.bin");
    assert_eq!(attachments[0]["mime"], "application/octet-stream");
    assert!(is_hex64(
        attachments[0]["hash"].as_str().expect("hash string")
    ));
}

#[test]
fn fetch_writes_the_attachment_bytes_unchanged() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    let bytes = sample_bytes();
    post_attachment(&env, &bob, &bytes);
    let hash = env.json("bob", &["read", "general"])["messages"][0]["attachments"][0]["hash"]
        .as_str()
        .expect("hash string")
        .to_owned();

    let out = env.folder("bob").join("fetched.bin");
    let out_str = out.to_str().expect("utf-8 path");
    let fetched = env.json("bob", &["fetch", &hash, "--out", out_str]);

    assert_eq!(fetched["bytes"], json!(bytes.len()));
    assert_eq!(fs::read(&out).expect("fetched file"), bytes);
}

#[test]
fn card_created_by_owner_is_listed() {
    let env = Env::new();
    env.json("alice", &["init", "--owner"]);
    env.json("alice", &["card", "create", "c1", "Title"]);

    let list = env.json("alice", &["card", "list"]);
    let cards = list["cards"].as_array().expect("cards array");
    assert!(
        cards
            .iter()
            .any(|c| c["id"] == "c1" && c["title"] == "Title"),
        "c1 is listed: {list}"
    );
}

#[test]
fn published_note_is_found_by_full_text_search() {
    let env = Env::new();
    env.json("alice", &["init", "--owner"]);
    env.json("alice", &["note", "publish", "n1", "T", "body words"]);

    let found = env.json("alice", &["note", "search", "words"]);
    let notes = found["notes"].as_array().expect("notes array");
    assert!(
        notes.iter().any(|n| n["id"] == "n1"),
        "n1 is found: {found}"
    );
}

#[test]
fn note_search_without_a_match_returns_no_notes() {
    let env = Env::new();
    env.json("alice", &["init", "--owner"]);
    env.json("alice", &["note", "publish", "n1", "T", "body words"]);

    let found = env.json("alice", &["note", "search", "zebra"]);
    assert_eq!(found["notes"], json!([]));
}

#[test]
fn concurrent_first_commands_spawn_exactly_one_daemon() {
    let env = Env::new();
    let outputs: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..5)
            .map(|_| {
                scope.spawn(|| {
                    let mut cmd = env.command("alice", "info");
                    cmd.args(["whoami", "--json"]);
                    let assert = cmd.assert().success();
                    serde_json::from_slice::<Value>(&assert.get_output().stdout)
                        .expect("stdout is one JSON document")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("whoami thread"))
            .collect()
    });

    let first = &outputs[0];
    assert_eq!(outputs.len(), 5);
    for out in &outputs {
        assert_eq!(out["pubkey"], first["pubkey"]);
        assert_eq!(out["owner_exists"], false);
    }
    assert_eq!(pubkey(&env, "alice"), first["pubkey"]);

    let log = fs::read_to_string(env.paths().log).expect("daemon log");
    let listening = log.lines().filter(|l| l.contains("listening")).count();
    assert_eq!(listening, 1, "one daemon served all five: {log}");
}

/// The `text` fields of a `read --json` document, oldest first.
fn texts(read: &Value) -> Vec<Value> {
    read["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| m["text"].clone())
        .collect()
}

/// The `epoch` fields of a `read --json` document, oldest first.
fn epochs(read: &Value) -> Vec<Value> {
    read["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| m["epoch"].clone())
        .collect()
}

/// alice creates the encrypted channel `sec` with bob as a member.
fn encrypted_with_bob(env: &Env, bob: &str) {
    env.json(
        "alice",
        &["channel", "create", "sec", "--encrypt", "--member", bob],
    );
}

/// Writes `bytes` to `name` in `who`'s folder and returns the path.
fn write_in(env: &Env, who: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let path = env.folder(who).join(name);
    fs::write(&path, bytes).expect("write file");
    path
}

/// alice posts `bytes` as `report.bin` to the encrypted channel `sec`; returns the blob
/// hash that bob sees on the message.
fn post_sealed_attachment(env: &Env, bob: &str, bytes: &[u8]) -> String {
    encrypted_with_bob(env, bob);
    let path = write_in(env, "alice", "report.bin", bytes);
    let path = path.to_str().expect("utf-8 path");
    env.json("alice", &["chat", "sec", "see file", "--attach", path]);
    env.json("bob", &["read", "sec"])["messages"][0]["attachments"][0]["hash"]
        .as_str()
        .expect("hash string")
        .to_owned()
}

#[test]
fn encrypted_attachment_is_listed_for_a_member_with_its_name() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    encrypted_with_bob(&env, &bob);
    let path = write_in(&env, "alice", "report.bin", &sample_bytes());
    let path = path.to_str().expect("utf-8 path");
    env.json("alice", &["chat", "sec", "see file", "--attach", path]);

    let read = env.json("bob", &["read", "sec"]);
    let m = &read["messages"][0];
    assert_eq!(m["encrypted"], true);
    assert_eq!(m["text"], "see file");
    let attachments = m["attachments"].as_array().expect("attachments array");
    assert_eq!(attachments.len(), 1, "one attachment: {read}");
    assert_eq!(attachments[0]["name"], "report.bin");
    assert!(is_hex64(
        attachments[0]["hash"].as_str().expect("hash string")
    ));
}

#[test]
fn fetch_of_an_encrypted_attachment_writes_the_original_bytes() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    let bytes = sample_bytes();
    let hash = post_sealed_attachment(&env, &bob, &bytes);

    let out = env.folder("bob").join("fetched.bin");
    let out_str = out.to_str().expect("utf-8 path");
    let fetched = env.json(
        "bob",
        &["fetch", &hash, "--out", out_str, "--channel", "sec"],
    );

    assert_eq!(fetched["bytes"], json!(bytes.len()));
    assert_eq!(fs::read(&out).expect("fetched file"), bytes);
}

#[test]
fn fetched_encrypted_attachment_file_is_readable_only_by_its_owner() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    let hash = post_sealed_attachment(&env, &bob, &sample_bytes());

    let out = env.folder("bob").join("fetched.bin");
    let out_str = out.to_str().expect("utf-8 path");
    env.json(
        "bob",
        &["fetch", &hash, "--out", out_str, "--channel", "sec"],
    );

    let mode = fs::metadata(&out)
        .expect("fetched file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn member_added_to_an_encrypted_channel_reads_messages_posted_before_and_after_the_add() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json("alice", &["channel", "create", "sec", "--encrypt"]);
    env.json("alice", &["chat", "sec", "one"]);
    env.json("alice", &["channel", "add", "sec", &bob]);
    env.json("alice", &["chat", "sec", "two"]);

    let read = env.json("bob", &["read", "sec"]);
    assert_eq!(texts(&read), vec![json!("one"), json!("two")]);
    assert_eq!(epochs(&read), vec![json!(0), json!(0)]);
}

#[test]
fn rotate_moves_an_encrypted_channel_from_epoch_zero_to_epoch_one() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    encrypted_with_bob(&env, &bob);
    assert_eq!(
        env.json("alice", &["channel", "info", "sec"])["channel"]["epoch"],
        0
    );

    let rotated = env.json("alice", &["channel", "rotate", "sec"]);
    assert_eq!(rotated["epoch"], 1);
    assert_eq!(
        env.json("alice", &["channel", "info", "sec"])["channel"]["epoch"],
        1
    );
}

#[test]
fn after_rotate_a_member_reads_old_epoch_zero_messages_and_new_epoch_one_messages() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    encrypted_with_bob(&env, &bob);
    env.json("alice", &["chat", "sec", "old"]);
    env.json("alice", &["channel", "rotate", "sec"]);
    env.json("alice", &["chat", "sec", "new"]);

    let read = env.json("bob", &["read", "sec"]);
    assert_eq!(texts(&read), vec![json!("old"), json!("new")]);
    assert_eq!(epochs(&read), vec![json!(0), json!(1)]);
}

#[test]
fn no_spawn_without_a_daemon_fails_with_daemon_code_and_starts_none() {
    let env = Env::new();
    let mut cmd = env.command("alice", "warn");
    cmd.env("AGORA_NO_SPAWN", "1").args(["whoami", "--json"]);
    let assert = cmd.assert().code(1);

    let err: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("error is one JSON document");
    assert_eq!(err["error"]["code"], "daemon");
    assert!(!env.paths().socket.exists(), "no daemon socket appeared");
}

#[test]
fn text_read_prints_one_line_per_message_with_control_characters_escaped() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
    env.json("alice", &["chat", "general", "first"]);
    env.json("alice", &["chat", "general", "\n[9] x: fake\x1b[31m"]);

    let assert = env.agora("bob", &["read", "general"]).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "one line per message: {stdout:?}");
    assert!(!stdout.contains('\x1b'), "no raw escape byte: {stdout:?}");
    assert!(
        lines[1].contains("\\n[9] x: fake"),
        "the newline is escaped in place: {stdout:?}"
    );
}

#[test]
fn fetch_of_an_unknown_hash_is_not_found_and_writes_no_file() {
    let env = Env::new();
    owner_and_member(&env);

    let out = env.folder("bob").join("never.bin");
    let out_str = out.to_str().expect("utf-8 path");
    let err = env.json_error("bob", &["fetch", &"0".repeat(64), "--out", out_str]);

    assert_eq!(err["error"]["code"], "not_found");
    assert!(!out.exists(), "a failed fetch leaves no output file");
}

#[test]
fn fetch_refuses_a_hash_that_is_not_64_lowercase_hex_before_asking_the_daemon() {
    let env = Env::new();
    owner_and_member(&env);

    let out = env.folder("bob").join("never.bin");
    let out_str = out.to_str().expect("utf-8 path");
    let uppercase = "A".repeat(64);
    for bad in ["abc", uppercase.as_str()] {
        let err = env.json_error("bob", &["fetch", bad, "--out", out_str]);
        assert_eq!(err["error"]["code"], "usage", "hash {bad:?}");
    }
    assert!(!out.exists(), "a refused fetch leaves no output file");
}

#[test]
fn self_leave_from_an_encrypted_channel_reports_that_rotation_is_required() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    encrypted_with_bob(&env, &bob);

    let removed = env.json("bob", &["channel", "remove", "sec", &bob]);
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["rotation_required"], true);
}

#[test]
fn posts_are_refused_after_a_self_leave_until_the_remaining_members_rotate() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    encrypted_with_bob(&env, &bob);
    env.json("bob", &["channel", "remove", "sec", &bob]);

    let err = env.json_error("alice", &["chat", "sec", "x"]);
    assert_eq!(err["error"]["code"], "invalid");

    env.json("alice", &["channel", "rotate", "sec"]);
    env.json("alice", &["chat", "sec", "x"]);
}
