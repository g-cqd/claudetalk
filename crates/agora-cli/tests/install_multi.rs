//! `agora install` / `agora uninstall` with several clients: `--client all`, comma lists, one
//! failing client among others, the Copilot hooks file that both Copilot clients share, the
//! capabilities backup, and the flags that apply to one client only. Each test runs the real
//! `agora` binary with every client's home inside a fresh temp tree (HOME, AGORA_HOME, CODEX_HOME,
//! COPILOT_HOME, XDG_CONFIG_HOME, AGORA_VSCODE_USER_DIR, the working directory), a PATH without a
//! real client, and a hard 30 s cap. The assertions read what lands on disk and the `--json`
//! document, because the files the installer writes are its contract.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// A PATH that holds no client binary.
const SAFE_PATH: &str = "/usr/bin:/bin";
/// Infix of every backup the installer keeps: `<name>.agora-bak-<ms>`.
const BACKUP_TAG: &str = ".agora-bak-";
/// Every client, in the order `--client all` runs them.
const ALL_CLIENTS: [&str; 7] = [
    "claude",
    "codex",
    "copilot-cli",
    "copilot-vscode",
    "agy",
    "opencode",
    "cursor",
];

/// One test's directory tree. `home` is HOME (and so `~/.copilot` is also `COPILOT_HOME`), the
/// other fields are the client roots the environment points at. Removed when the test ends.
struct World {
    _root: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
    agora_home: PathBuf,
    codex_home: PathBuf,
    xdg_config: PathBuf,
    vscode_user: PathBuf,
}

impl World {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let base = root.path().to_path_buf();
        let home = base.join("home");
        let project = base.join("project");
        for dir in [&home, &project] {
            fs::create_dir_all(dir).expect("create test directory");
        }
        Self {
            _root: root,
            agora_home: base.join("agora"),
            codex_home: base.join("codex"),
            xdg_config: base.join("xdg"),
            vscode_user: base.join("vscode").join("User"),
            // The installer records the canonical working directory for project scope.
            project: fs::canonicalize(&project).expect("canonical project dir"),
            home,
            base,
        }
    }

    fn capabilities(&self) -> PathBuf {
        self.agora_home.join("capabilities.json")
    }

    /// The Copilot hooks file user scope writes. `copilot-cli` (under `COPILOT_HOME`, which is
    /// `~/.copilot` here) and `copilot-vscode` (under `~/.copilot`) name the same file.
    fn copilot_hooks(&self) -> PathBuf {
        self.home.join(".copilot").join("hooks").join("agora.json")
    }

    /// `agora --json <args>` in the test environment. Every client root is inside the temp tree.
    fn agora(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("PATH", SAFE_PATH)
            .env("AGORA_HOME", &self.agora_home)
            .env("CODEX_HOME", &self.codex_home)
            .env("COPILOT_HOME", self.home.join(".copilot"))
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("AGORA_VSCODE_USER_DIR", &self.vscode_user)
            .current_dir(&self.project)
            .timeout(CMD_TIMEOUT)
            .arg("--json")
            .args(args);
        cmd
    }
}

/// The outcome of one invocation: whether it exited 0, and its stdout document.
struct Run {
    ok: bool,
    json: Value,
}

/// Run `cmd`; with `--json` its stdout is one JSON document, success or error.
fn run(cmd: &mut Command) -> Run {
    let out = cmd.output().expect("agora runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {stdout}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    Run {
        ok: out.status.success(),
        json,
    }
}

/// Run `cmd` and expect exit 0; returns the JSON document.
fn run_ok(cmd: &mut Command) -> Value {
    let out = run(cmd);
    assert!(out.ok, "expected exit 0, got {}", out.json);
    out.json
}

/// The message of an error document: `{"error":{"code":..,"message":..}}`.
fn error_message(out: &Run) -> String {
    out.json["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message in {}", out.json))
        .to_owned()
}

/// The names of the clients in a multi-client document, in output order.
fn client_names(out: &Value) -> Vec<String> {
    out["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .map(|c| c["client"].as_str().expect("client name").to_owned())
        .collect()
}

/// The per-client item of `client` in a multi-client document.
fn item<'a>(out: &'a Value, client: &str) -> &'a Value {
    out["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .find(|c| c["client"] == client)
        .unwrap_or_else(|| panic!("no item for {client} in {out}"))
}

/// The `result` of the action on `path` inside one client item; panics when there is none.
fn result_in(item: &Value, path: &Path) -> String {
    let want = path.display().to_string();
    item["actions"]
        .as_array()
        .expect("actions array")
        .iter()
        .find(|a| a["path"] == want)
        .and_then(|a| a["result"].as_str())
        .unwrap_or_else(|| panic!("no action for {want} in {item}"))
        .to_owned()
}

/// Every `result` reported for `path` across all client items of a document.
fn results_for(out: &Value, path: &Path) -> Vec<String> {
    let want = path.display().to_string();
    let mut found = Vec::new();
    for client in out["clients"].as_array().expect("clients array") {
        for action in client["actions"].as_array().expect("actions array") {
            if action["path"] == want {
                found.push(action["result"].as_str().expect("result").to_owned());
            }
        }
    }
    found
}

/// Every entry under `root`, directories and files alike, keyed by path relative to `root`:
/// `None` for a directory, the bytes for a file. Empty when `root` does not exist.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(list) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in list {
            let path = entry.expect("directory entry").path();
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            if path.is_dir() {
                entries.insert(rel, None);
                pending.push(path);
            } else {
                entries.insert(rel, Some(fs::read(&path).expect("read file")));
            }
        }
    }
    entries
}

/// Backups of `path` (`<name>.agora-bak-<ms>`), sorted.
fn backups_of(path: &Path) -> Vec<PathBuf> {
    let name = path
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}{BACKUP_TAG}");
    let mut found: Vec<PathBuf> = fs::read_dir(path.parent().expect("parent"))
        .expect("read parent dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    found.sort();
    found
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent dir");
    fs::write(path, bytes).expect("write file");
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Whether the JSON array `list` holds the string `want`.
fn lists(list: &Value, want: &str) -> bool {
    list.as_array()
        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(want)))
}

#[test]
fn dry_run_of_all_clients_lists_seven_items_in_order_and_writes_nothing() {
    let w = World::new();
    let before = tree(&w.base);

    let out = run_ok(&mut w.agora(&["install", "--client", "all", "--dry-run"]));

    assert_eq!(out["ok"], true);
    assert_eq!(client_names(&out), ALL_CLIENTS);
    for client in out["clients"].as_array().expect("clients array") {
        assert_eq!(client["dry_run"].as_bool(), Some(true), "{client}");
    }
    assert_eq!(tree(&w.base), before, "no file or directory is created");
}

#[test]
fn install_of_two_clients_writes_both_and_records_both_in_capabilities() {
    let w = World::new();

    let out = run_ok(&mut w.agora(&["install", "--client", "codex,cursor"]));

    assert_eq!(out["ok"], true);
    assert_eq!(client_names(&out), ["codex", "cursor"]);
    let codex_config = w.codex_home.join("config.toml");
    let cursor_mcp = w.home.join(".cursor").join("mcp.json");
    assert!(codex_config.is_file(), "codex config written");
    assert!(cursor_mcp.is_file(), "cursor MCP file written");

    let caps = read_json(&w.capabilities());
    assert_eq!(caps["codex"]["scopes"], json!(["user"]));
    assert_eq!(caps["cursor"]["scopes"], json!(["user"]));
    assert!(lists(
        &caps["codex"]["files"],
        &codex_config.display().to_string()
    ));
    assert!(lists(
        &caps["cursor"]["files"],
        &cursor_mcp.display().to_string()
    ));
    assert!(caps.get("claude").is_none(), "no claude entry: {caps}");
}

#[test]
fn duplicate_names_in_the_client_list_run_once_in_first_seen_order() {
    let w = World::new();

    let out = run_ok(&mut w.agora(&["install", "--client", "cursor,codex,cursor", "--dry-run"]));

    assert_eq!(client_names(&out), ["cursor", "codex"]);
}

#[test]
fn unknown_client_name_is_a_usage_error_and_nothing_is_written() {
    let w = World::new();
    let before = tree(&w.base);

    let out = run(&mut w.agora(&["install", "--client", "codex,bogus"]));

    assert!(!out.ok, "{}", out.json);
    assert!(
        error_message(&out).contains("unknown client 'bogus'"),
        "{}",
        out.json
    );
    assert_eq!(tree(&w.base), before, "codex is not written either");
}

#[test]
fn a_failing_client_does_not_stop_the_others_and_the_command_exits_non_zero() {
    let w = World::new();
    let cursor_mcp = w.home.join(".cursor").join("mcp.json");
    write_file(&cursor_mcp, b"{bad");

    let out = run(&mut w.agora(&["install", "--client", "codex,cursor"]));

    assert!(
        !out.ok,
        "a failed client makes the exit non-zero: {}",
        out.json
    );
    assert_eq!(out.json["ok"], false);
    let codex = item(&out.json, "codex");
    assert!(codex.get("error").is_none(), "{codex}");
    assert_eq!(result_in(codex, &w.codex_home.join("config.toml")), "wrote");

    let cursor = item(&out.json, "cursor");
    let message = cursor["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains(&cursor_mcp.display().to_string()),
        "the error names the file: {message}"
    );
    assert_eq!(fs::read(&cursor_mcp).expect("cursor file"), b"{bad");
    assert!(
        !w.home.join(".cursor").join("hooks.json").exists(),
        "nothing is written for the failed client"
    );

    let caps = read_json(&w.capabilities());
    assert!(caps.get("codex").is_some(), "codex is recorded: {caps}");
    assert!(caps.get("cursor").is_none(), "cursor is not: {caps}");
}

#[test]
fn second_copilot_client_finds_the_shared_hooks_file_unchanged() {
    let w = World::new();
    let hooks = w.copilot_hooks();
    let first = run_ok(&mut w.agora(&["install", "--client", "copilot-cli"]));
    assert_eq!(result_in(item(&first, "copilot-cli"), &hooks), "wrote");

    let second = run_ok(&mut w.agora(&["install", "--client", "copilot-vscode"]));

    assert_eq!(
        result_in(item(&second, "copilot-vscode"), &hooks),
        "unchanged"
    );
}

#[test]
fn uninstalling_one_copilot_client_keeps_the_hooks_file_the_other_still_uses() {
    let w = World::new();
    let hooks = w.copilot_hooks();
    run_ok(&mut w.agora(&["install", "--client", "copilot-cli,copilot-vscode"]));
    let before = fs::read(&hooks).expect("shared hooks file");

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "copilot-cli"]));

    assert_eq!(result_in(item(&out, "copilot-cli"), &hooks), "kept");
    assert_eq!(fs::read(&hooks).expect("hooks file kept"), before);
}

#[test]
fn uninstalling_the_last_copilot_client_removes_the_shared_hooks_file() {
    let w = World::new();
    let hooks = w.copilot_hooks();
    run_ok(&mut w.agora(&["install", "--client", "copilot-cli,copilot-vscode"]));
    run_ok(&mut w.agora(&["uninstall", "--client", "copilot-cli"]));

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "copilot-vscode"]));

    assert_eq!(result_in(item(&out, "copilot-vscode"), &hooks), "removed");
    assert!(!hooks.exists(), "the file is gone");
}

#[test]
fn uninstalling_both_copilot_clients_in_one_call_removes_the_shared_hooks_file() {
    let w = World::new();
    let hooks = w.copilot_hooks();
    run_ok(&mut w.agora(&["install", "--client", "copilot-cli,copilot-vscode"]));

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "copilot-cli,copilot-vscode"]));

    let results = results_for(&out, &hooks);
    assert!(results.contains(&"removed".to_owned()), "{results:?}");
    assert!(!results.contains(&"kept".to_owned()), "{results:?}");
    assert!(!hooks.exists(), "the file is gone");
}

#[test]
fn an_unparseable_capabilities_file_is_backed_up_and_replaced() {
    let w = World::new();
    write_file(&w.capabilities(), b"{bad");

    let out = run_ok(&mut w.agora(&["install", "--client", "codex"]));

    assert_eq!(out["ok"], true);
    let backups = backups_of(&w.capabilities());
    assert_eq!(backups.len(), 1, "one backup of the file it replaced");
    assert_eq!(fs::read(&backups[0]).expect("backup"), b"{bad");
    let caps = read_json(&w.capabilities());
    assert_eq!(caps["schema"], 1);
    assert!(caps["codex"].is_object(), "codex entry recorded: {caps}");
}

#[test]
fn mode_entries_with_a_client_other_than_claude_is_a_usage_error() {
    let w = World::new();

    let out = run(&mut w.agora(&["install", "--client", "codex", "--mode", "entries"]));

    assert!(!out.ok, "{}", out.json);
    assert!(
        error_message(&out).contains("--mode applies to --client claude only"),
        "{}",
        out.json
    );
}

#[test]
fn a_single_claude_client_keeps_the_single_client_output_shape() {
    let w = World::new();

    let out = run_ok(&mut w.agora(&["install", "--client", "claude", "--dry-run"]));

    assert_eq!(out["client"], "claude");
    assert!(out.get("clients").is_none(), "no clients array: {out}");
    assert!(out["actions"].is_array(), "{out}");
}
