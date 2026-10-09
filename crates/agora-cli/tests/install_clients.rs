//! `agora install|uninstall --client <c>` for the non-Claude clients, through the real binary. The
//! table has one row per client (codex, copilot-cli, copilot-vscode, agy, opencode, cursor); each
//! row runs the same six behaviours against its own MCP file and container key. Codex trust and
//! Cursor project scope follow. Every run has HOME, AGORA_HOME, CODEX_HOME, COPILOT_HOME,
//! AGORA_VSCODE_USER_DIR and XDG_CONFIG_HOME inside one temp root, and the working directory is a
//! temp project, so nothing outside the root is read or written. Assertions read the files the
//! installer writes and its `--json` output, because those files are the contract.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::DocumentMut;

/// Hard cap on one invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// A PATH that holds no client binary, so no probe can find one.
const SAFE_PATH: &str = "/usr/bin:/bin";
/// The server name every client config uses for agora.
const SERVER: &str = "agora";
/// Codex hooks.json has four agora handlers (SessionStart, UserPromptSubmit, PostToolUse, Stop).
const CODEX_HOOK_COUNT: usize = 4;

/// One test's directory tree. Every client location lives under `base`.
struct World {
    _root: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    agora_home: PathBuf,
    project: PathBuf,
    codex_home: PathBuf,
    copilot_home: PathBuf,
    vscode_user: PathBuf,
    xdg: PathBuf,
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
            // The installer records the canonical working directory for project scope.
            project: fs::canonicalize(&project).expect("canonical project dir"),
            codex_home: base.join("codex"),
            copilot_home: base.join("copilot"),
            vscode_user: base.join("vscode-user"),
            xdg: base.join("xdg"),
            home,
            base,
        }
    }

    fn capabilities(&self) -> PathBuf {
        self.agora_home.join("capabilities.json")
    }

    /// `agora --json <args>` with every client location pointed into the temp root.
    fn agora(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .env("AGORA_HOME", &self.agora_home)
            .env("CODEX_HOME", &self.codex_home)
            .env("COPILOT_HOME", &self.copilot_home)
            .env("AGORA_VSCODE_USER_DIR", &self.vscode_user)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env("PATH", SAFE_PATH)
            .env_remove("AGORA_CLAUDE_BIN")
            .current_dir(&self.project)
            .timeout(CMD_TIMEOUT);
        cmd
    }
}

/// Run `cmd`; with `--json` its stdout is one JSON document. Returns whether it exited 0, and
/// the document.
fn run(cmd: &mut Command) -> (bool, Value) {
    let out = cmd.output().expect("agora runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        panic!("stdout is not JSON ({e}): {stdout}; stderr: {stderr}")
    });
    (out.status.success(), json)
}

/// Run `cmd` and expect exit 0; returns the JSON document.
fn run_ok(cmd: &mut Command) -> Value {
    let (ok, json) = run(cmd);
    assert!(ok, "expected exit 0, got {json}");
    json
}

/// The actions of the one client a single-client install or uninstall document reports.
fn actions_of(doc: &Value) -> &[Value] {
    doc["clients"][0]["actions"]
        .as_array()
        .expect("actions array")
}

fn assert_all_results(actions: &[Value], want: &str) {
    for action in actions {
        assert_eq!(action["result"], want, "{action}");
    }
}

/// Every file and directory under `root`: directories map to `None`, files to their bytes. Keys
/// are relative to `root`. Empty when `root` does not exist.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("directory entry").path();
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            if path.is_dir() {
                found.insert(rel, None);
                pending.push(path);
            } else {
                found.insert(rel, Some(fs::read(&path).expect("read file")));
            }
        }
    }
    found
}

/// Only the files of `tree(root)`, with their bytes.
fn file_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    tree(root)
        .into_iter()
        .filter_map(|(path, bytes)| bytes.map(|b| (path, b)))
        .collect()
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent dir");
    fs::write(path, bytes).expect("write file");
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// The binary under test, as invoked (not canonicalized), which is what the installer records.
fn exe() -> String {
    assert_cmd::cargo::cargo_bin("agora").display().to_string()
}

/// `capabilities.json[harness]`, or None when the file or the key is absent.
fn caps_entry(w: &World, harness: &str) -> Option<Value> {
    let bytes = fs::read(w.capabilities()).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get(harness)
        .cloned()
}

/// `mcp_servers.<name>.command` of a Codex config.toml, when present.
fn toml_command(text: &str, name: &str) -> Option<String> {
    let doc: DocumentMut = text
        .parse()
        .unwrap_or_else(|e| panic!("not TOML ({e}): {text}"));
    let servers = doc.get("mcp_servers")?.as_table_like()?;
    let command = servers.get(name)?.as_table_like()?.get("command")?;
    command.as_str().map(str::to_owned)
}

/// The `command` of the agora server in one client's MCP file. For opencode, which stores argv
/// as an array, the first element.
fn agora_command(case: &Case, text: &str) -> Option<String> {
    match case.shape {
        Shape::Toml => toml_command(text, SERVER),
        Shape::Json(container, argv) => {
            let index = if argv { "/0" } else { "" };
            let pointer = format!("/{container}/{SERVER}/command{index}");
            json_of(text).pointer(&pointer)?.as_str().map(str::to_owned)
        }
    }
}

/// The other server a user already has, in the shape of its client's file.
fn other_entry(argv: bool) -> Value {
    if argv {
        json!({"type": "local", "command": ["node", "docs.js"], "enabled": true})
    } else {
        json!({"type": "stdio", "command": "node", "args": ["docs.js"]})
    }
}

/// The user's config before agora: one other server and an unrelated value.
fn user_seed(case: &Case) -> String {
    match case.shape {
        Shape::Toml => {
            "# keep me\n[mcp_servers.other]\ncommand = \"node\"\nargs = [\"docs.js\"]\n".to_owned()
        }
        Shape::Json(container, argv) => {
            json!({"theme": "dark", container: {"userdocs": other_entry(argv)}}).to_string()
        }
    }
}

/// Whether the content `user_seed` wrote is still in `text`.
fn user_content_kept(case: &Case, text: &str) -> bool {
    match case.shape {
        Shape::Toml => {
            text.contains("# keep me") && toml_command(text, "other").as_deref() == Some("node")
        }
        Shape::Json(container, argv) => {
            let root = json_of(text);
            root["theme"] == "dark" && root[container]["userdocs"] == other_entry(argv)
        }
    }
}

/// How a client's MCP file holds the agora server.
#[derive(Clone, Copy)]
enum Shape {
    /// Codex: `[mcp_servers.agora]` in `config.toml`.
    Toml,
    /// A JSON object under the container key; the flag marks an argv-array command (opencode).
    Json(&'static str, bool),
}

const MCP_SERVERS: Shape = Shape::Json("mcpServers", false);

/// One row of the table.
struct Case {
    client: &'static str,
    /// The client's key in `capabilities.json` (`Harness::as_str()`).
    harness: &'static str,
    /// The client's MCP config file under the temp root.
    mcp: fn(&World) -> PathBuf,
    shape: Shape,
    /// Bytes that are not a valid MCP config for this client.
    malformed: &'static [u8],
}

fn codex_mcp(w: &World) -> PathBuf {
    w.codex_home.join("config.toml")
}

fn copilot_cli_mcp(w: &World) -> PathBuf {
    w.copilot_home.join("mcp-config.json")
}

fn copilot_vscode_mcp(w: &World) -> PathBuf {
    w.vscode_user.join("mcp.json")
}

fn agy_mcp(w: &World) -> PathBuf {
    w.home.join(".gemini/config/mcp_config.json")
}

fn opencode_mcp(w: &World) -> PathBuf {
    w.xdg.join("opencode/opencode.json")
}

fn cursor_mcp(w: &World) -> PathBuf {
    w.home.join(".cursor/mcp.json")
}

const CODEX: Case = Case {
    client: "codex",
    harness: "codex",
    mcp: codex_mcp,
    shape: Shape::Toml,
    malformed: b"[[",
};
const COPILOT_CLI: Case = Case {
    client: "copilot-cli",
    harness: "copilot_cli",
    mcp: copilot_cli_mcp,
    shape: MCP_SERVERS,
    malformed: b"{not json",
};
const COPILOT_VSCODE: Case = Case {
    client: "copilot-vscode",
    harness: "copilot_vscode",
    mcp: copilot_vscode_mcp,
    shape: Shape::Json("servers", false),
    malformed: b"{not json",
};
const AGY: Case = Case {
    client: "agy",
    harness: "antigravity",
    mcp: agy_mcp,
    shape: MCP_SERVERS,
    malformed: b"{not json",
};
const OPENCODE: Case = Case {
    client: "opencode",
    harness: "opencode",
    mcp: opencode_mcp,
    shape: Shape::Json("mcp", true),
    malformed: b"{not json",
};
const CURSOR: Case = Case {
    client: "cursor",
    harness: "cursor",
    mcp: cursor_mcp,
    shape: MCP_SERVERS,
    malformed: b"{not json",
};

fn check_fresh_install(case: &Case) {
    let w = World::new();
    let out = run_ok(&mut w.agora(&["install", "--client", case.client]));
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["clients"].as_array().map(Vec::len), Some(1), "{out}");
    let text = read_text(&(case.mcp)(&w));
    assert_eq!(agora_command(case, &text), Some(exe()), "{text}");
    let entry = caps_entry(&w, case.harness)
        .unwrap_or_else(|| panic!("no {} entry in capabilities", case.harness));
    assert_eq!(entry["scopes"], json!(["user"]), "{entry}");
}

fn check_merge_keeps_user_servers(case: &Case) {
    let w = World::new();
    let mcp = (case.mcp)(&w);
    write_file(&mcp, user_seed(case).as_bytes());
    run_ok(&mut w.agora(&["install", "--client", case.client]));
    let text = read_text(&mcp);
    assert_eq!(agora_command(case, &text), Some(exe()), "{text}");
    assert!(user_content_kept(case, &text), "user content lost: {text}");
}

fn check_second_install_changes_nothing(case: &Case) {
    let w = World::new();
    write_file(&(case.mcp)(&w), user_seed(case).as_bytes());
    run_ok(&mut w.agora(&["install", "--client", case.client]));
    let before = tree(&w.base);
    let out = run_ok(&mut w.agora(&["install", "--client", case.client]));
    let actions = actions_of(&out);
    assert!(!actions.is_empty(), "{out}");
    assert_all_results(actions, "unchanged");
    let caps_actions = out["capabilities_actions"].as_array().expect("array");
    assert_all_results(caps_actions, "unchanged");
    assert_eq!(tree(&w.base), before, "no file rewritten, no backup added");
}

fn check_uninstall_keeps_user_config(case: &Case) {
    let w = World::new();
    let mcp = (case.mcp)(&w);
    write_file(&mcp, user_seed(case).as_bytes());
    run_ok(&mut w.agora(&["install", "--client", case.client]));
    assert!(
        caps_entry(&w, case.harness).is_some(),
        "install recorded the capabilities key"
    );
    run_ok(&mut w.agora(&["uninstall", "--client", case.client]));
    let text = read_text(&mcp);
    assert_eq!(
        agora_command(case, &text),
        None,
        "our entry is gone: {text}"
    );
    assert!(user_content_kept(case, &text), "user content lost: {text}");
    assert!(
        caps_entry(&w, case.harness).is_none(),
        "the capabilities key is removed"
    );
}

fn check_dry_run_writes_nothing(case: &Case) {
    let w = World::new();
    let before = tree(&w.base);
    let out = run_ok(&mut w.agora(&["install", "--client", case.client, "--dry-run"]));
    let actions = actions_of(&out);
    assert!(!actions.is_empty(), "{out}");
    assert_all_results(actions, "dry_run");
    assert_eq!(tree(&w.base), before, "no file or directory created");
}

fn check_malformed_config_is_refused(case: &Case) {
    let w = World::new();
    let mcp = (case.mcp)(&w);
    write_file(&mcp, case.malformed);
    let before = file_bytes(&w.base);
    let (ok, json) = run(&mut w.agora(&["install", "--client", case.client]));
    assert!(!ok, "expected a failure, got {json}");
    assert_eq!(json["ok"], false, "{json}");
    let message = json["clients"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message in {json}"));
    assert!(
        message.contains(&mcp.display().to_string()),
        "names the file: {message}"
    );
    assert_eq!(fs::read(&mcp).expect("read"), case.malformed);
    assert_eq!(file_bytes(&w.base), before, "nothing else written");
}

/// Codex `--trust-hooks` writes one trust entry per agora hook into the user config.toml.
#[test]
fn codex_trust_hooks_writes_one_trust_entry_per_hook() {
    let w = World::new();
    run_ok(&mut w.agora(&["install", "--client", "codex", "--trust-hooks"]));
    let text = read_text(&codex_mcp(&w));
    assert!(text.contains("[hooks.state.\""), "{text}");
    let trusted = text.matches("trusted_hash = \"sha256:").count();
    assert_eq!(trusted, CODEX_HOOK_COUNT, "{text}");
}

/// Without `--trust-hooks` the item tells the user where to review the hooks.
#[test]
fn codex_without_trust_hooks_notes_the_hooks_review() {
    let w = World::new();
    let out = run_ok(&mut w.agora(&["install", "--client", "codex"]));
    let notes = out["clients"][0]["notes"].as_array().expect("notes array");
    let mentions_review = notes
        .iter()
        .filter_map(Value::as_str)
        .any(|n| n.contains("/hooks"));
    assert!(mentions_review, "{out}");
}

/// Trust is opt-in: without `--trust-hooks` the user config gets no hook trust entries.
#[test]
fn codex_without_trust_hooks_writes_no_trust_entries() {
    let w = World::new();
    run_ok(&mut w.agora(&["install", "--client", "codex"]));
    let text = read_text(&codex_mcp(&w));
    assert!(!text.contains("hooks.state"), "{text}");
}

/// Cursor project scope writes its owned rule file into the project directory.
#[test]
fn cursor_project_scope_writes_the_agora_rule_in_the_project() {
    let w = World::new();
    let out = run_ok(&mut w.agora(&["install", "--client", "cursor", "--scope", "project"]));
    let rule = w.project.join(".cursor/rules/agora.mdc");
    let path = rule.display().to_string();
    let wrote = actions_of(&out)
        .iter()
        .any(|a| a["path"] == path && a["result"] == "wrote");
    assert!(wrote, "{out}");
    let text = read_text(&rule);
    assert!(text.contains("<!-- agora:begin"), "{text}");
}

/// The six behaviours, one module per client so a failure names its client.
macro_rules! client_suite {
    ($($module:ident => $case:ident),* $(,)?) => {
        $(
            mod $module {
                use super::*;

                #[test]
                fn fresh_install() { check_fresh_install(&$case); }

                #[test]
                fn merge_keeps_user_servers() { check_merge_keeps_user_servers(&$case); }

                #[test]
                fn second_install_changes_nothing() { check_second_install_changes_nothing(&$case); }

                #[test]
                fn uninstall_keeps_user_config() { check_uninstall_keeps_user_config(&$case); }

                #[test]
                fn dry_run_writes_nothing() { check_dry_run_writes_nothing(&$case); }

                #[test]
                fn malformed_config_is_refused() { check_malformed_config_is_refused(&$case); }
            }
        )*
    };
}

client_suite! {
    codex => CODEX,
    copilot_cli => COPILOT_CLI,
    copilot_vscode => COPILOT_VSCODE,
    agy => AGY,
    opencode => OPENCODE,
    cursor => CURSOR,
}
