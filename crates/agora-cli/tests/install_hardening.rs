//! Installer hardening, through the real `agora` binary. Three behaviours: an install into an existing
//! JSON file keeps that file's key order, and so does an uninstall; a project write cannot leave the
//! project through a symlinked ancestor, while user scope keeps working through one; and a
//! same-scope untrusted Codex reinstall lowers the recorded hook trust, while an untrusted install of
//! another scope does not. Every run has HOME, AGORA_HOME, CODEX_HOME and the other client roots
//! inside one temp tree, and the working directory is a temp project, so nothing outside the tree is
//! touched. Rewritten JSON is indented with two spaces, so these tests compare parsed values and key
//! order, never the raw bytes.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// A PATH that holds no client binary, so no probe can find one.
const SAFE_PATH: &str = "/usr/bin:/bin";
/// An existing user config: keys out of alphabetical order, at both levels.
const SEED: &str = r#"{"mcpServers":{"b":{},"a":{}},"theme":"dark"}"#;

/// One test's directory tree. Every client location lives under `base`.
struct World {
    _root: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
    agora_home: PathBuf,
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
            home,
            base,
        }
    }

    fn capabilities(&self) -> PathBuf {
        self.agora_home.join("capabilities.json")
    }

    /// `agora --json <args>` run from `exe`, with every client location pointed into the temp root.
    fn command(&self, exe: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(exe);
        cmd.arg("--json")
            .args(args)
            .env("HOME", &self.home)
            .env("AGORA_HOME", &self.agora_home)
            .env("CODEX_HOME", self.base.join("codex"))
            .env("COPILOT_HOME", self.base.join("copilot"))
            .env("AGORA_VSCODE_USER_DIR", self.base.join("vscode-user"))
            .env("XDG_CONFIG_HOME", self.base.join("xdg"))
            .env("PATH", SAFE_PATH)
            .env_remove("AGORA_CLAUDE_BIN")
            .current_dir(&self.project)
            .timeout(CMD_TIMEOUT);
        cmd
    }

    fn agora(&self, args: &[&str]) -> Command {
        self.command(&assert_cmd::cargo::cargo_bin("agora"), args)
    }

    /// A copy of the binary at another path. The hook commands name the binary's path, so running
    /// the copy rewrites the hooks with new commands, as a moved or upgraded binary would.
    fn moved_binary(&self) -> PathBuf {
        let dir = self.base.join("moved");
        fs::create_dir_all(&dir).expect("create moved dir");
        let target = dir.join("agora");
        fs::copy(assert_cmd::cargo::cargo_bin("agora"), &target).expect("copy binary");
        target
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

fn read_value(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The keys of a JSON object, in the order the file lists them.
fn keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect()
}

/// `capabilities.json[codex].hooks_trusted`.
fn codex_trust(w: &World) -> Value {
    read_value(&w.capabilities())["codex"]["hooks_trusted"].clone()
}

#[test]
fn install_keeps_the_key_order_of_an_existing_json_file() {
    let w = World::new();
    let mcp = w.home.join(".cursor/mcp.json");
    fs::create_dir_all(mcp.parent().expect("parent")).expect("create .cursor");
    fs::write(&mcp, SEED).expect("seed");

    run_ok(&mut w.agora(&["install", "--client", "cursor"]));

    let doc = read_value(&mcp);
    assert_eq!(keys(&doc), ["mcpServers", "theme"]);
    assert_eq!(keys(&doc["mcpServers"]), ["b", "a", "agora"]);
    assert_eq!(doc["theme"], "dark");
}

#[test]
fn install_then_uninstall_leaves_the_parsed_value_and_key_order() {
    let w = World::new();
    let mcp = w.home.join(".cursor/mcp.json");
    fs::create_dir_all(mcp.parent().expect("parent")).expect("create .cursor");
    fs::write(&mcp, SEED).expect("seed");
    let original = read_value(&mcp);

    run_ok(&mut w.agora(&["install", "--client", "cursor"]));
    run_ok(&mut w.agora(&["uninstall", "--client", "cursor"]));

    let doc = read_value(&mcp);
    assert_eq!(doc, original);
    assert_eq!(keys(&doc), ["mcpServers", "theme"]);
    assert_eq!(keys(&doc["mcpServers"]), ["b", "a"]);
}

#[test]
fn project_write_through_a_symlinked_ancestor_outside_the_project_is_refused() {
    let w = World::new();
    let outside = w.base.join("elsewhere");
    fs::create_dir_all(&outside).expect("create outside dir");
    std::os::unix::fs::symlink(&outside, w.project.join(".cursor")).expect("symlink .cursor");

    let (ok, doc) = run(&mut w.agora(&["install", "--client", "cursor", "--scope", "project"]));

    assert!(!ok, "expected a failure, got {doc}");
    assert!(
        doc.to_string().contains("resolves outside the project"),
        "{doc}"
    );
    assert_eq!(
        fs::read_dir(&outside).expect("read outside dir").count(),
        0,
        "nothing may be written outside the project"
    );
}

#[test]
fn user_scope_writes_through_a_symlinked_config_directory_still_work() {
    let w = World::new();
    let dotfiles = w.base.join("dotfiles/cursor");
    fs::create_dir_all(&dotfiles).expect("create dotfiles dir");
    std::os::unix::fs::symlink(&dotfiles, w.home.join(".cursor")).expect("symlink ~/.cursor");

    run_ok(&mut w.agora(&["install", "--client", "cursor"]));

    assert!(
        dotfiles.join("mcp.json").is_file(),
        "user config written through the link"
    );
}

#[test]
fn same_scope_untrusted_reinstall_lowers_the_recorded_codex_trust() {
    let w = World::new();
    run_ok(&mut w.agora(&["install", "--client", "codex", "--trust-hooks"]));
    assert_eq!(codex_trust(&w), json!(true));

    // The moved binary writes hooks with new commands, so the trust entries no longer match.
    let moved = w.moved_binary();
    let out = run_ok(&mut w.command(&moved, &["install", "--client", "codex"]));

    assert!(out.to_string().contains("Codex skips"), "{out}");
    assert_eq!(codex_trust(&w), json!(false));
}

#[test]
fn untrusted_install_of_another_codex_scope_keeps_the_recorded_trust() {
    let w = World::new();
    run_ok(&mut w.agora(&["install", "--client", "codex", "--trust-hooks"]));
    assert_eq!(codex_trust(&w), json!(true));

    let out = run_ok(&mut w.agora(&["install", "--client", "codex", "--scope", "project"]));

    assert!(out.to_string().contains("Codex skips"), "{out}");
    assert_eq!(codex_trust(&w), json!(true));
}
