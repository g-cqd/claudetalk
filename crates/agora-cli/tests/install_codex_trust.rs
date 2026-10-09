//! Codex hook trust through the real binary. Only the handlers one install run wrote, matched by exact
//! `(event, command)`, are trusted; a handler that merely carries agora's magic substrings is not.
//! Uninstall removes exactly the trust keys of those handlers, and a rerun after a failed write still
//! finds them. Every run has its own temp root, and the working directory is a temp project.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// A PATH that holds no client binary, so no probe can find one.
const SAFE_PATH: &str = "/usr/bin:/bin";
/// A handler that carries agora's magic substrings but is not a command this install writes.
const SPOOF: &str = "echo # --home x hook stop --dialect codex";
/// Trust entries of the four handlers agora writes for Codex.
const CODEX_HOOK_COUNT: usize = 4;

/// One test's directory tree. Every client location lives under the temp root.
struct World {
    _root: tempfile::TempDir,
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
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&project).expect("project");
        Self {
            // The installer records the canonical working directory for project scope.
            project: fs::canonicalize(&project).expect("canonical project"),
            home,
            agora_home: base.join("agora"),
            codex_home: base.join("codex"),
            copilot_home: base.join("copilot"),
            vscode_user: base.join("vscode-user"),
            xdg: base.join("xdg"),
            _root: root,
        }
    }

    /// `agora <args>` with every client location pointed into the temp root.
    fn agora(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.args(args)
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

    /// Codex user config: holds the trust entries of user-scope installs and of project installs.
    fn user_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }

    fn user_hooks(&self) -> PathBuf {
        self.codex_home.join("hooks.json")
    }

    fn project_hooks(&self) -> PathBuf {
        self.project.join(".codex/hooks.json")
    }
}

/// Sets a directory's mode for its lifetime and restores 0o755 on drop, so a failed assertion still
/// lets the temp root be removed.
struct DirMode(PathBuf);

impl DirMode {
    fn set(path: &Path, mode: u32) -> Self {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
        Self(path.to_path_buf())
    }
}

impl Drop for DirMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Run `cmd` and expect exit 0.
fn run_ok(cmd: &mut Command) {
    let out = cmd.output().expect("agora runs");
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("parent dir")).expect("create dir");
    fs::write(path, value.to_string()).expect("write json");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read file")
}

/// Number of `trusted_hash` entries in a Codex config.
fn trust_count(text: &str) -> usize {
    text.matches("trusted_hash = \"sha256:").count()
}

/// A spoofed PreToolUse handler, carrying the magic substrings, gets no trust entry; the four
/// handlers this run wrote are still trusted.
#[test]
fn spoofed_pre_tool_use_handler_is_not_trusted() {
    let w = World::new();
    write_json(
        &w.project_hooks(),
        &json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": SPOOF}
        ]}]}}),
    );

    run_ok(&mut w.agora(&[
        "install",
        "--client",
        "codex",
        "--scope",
        "project",
        "--trust-hooks",
    ]));

    let text = read(&w.user_config());
    assert_eq!(trust_count(&text), CODEX_HOOK_COUNT, "{text}");
    assert!(!text.contains(":pre_tool_use:"), "{text}");
}

/// A spoofed handler in the same group as our Stop handler gets no trust entry. The install
/// replaces the first spoof with ours, so the second spoof sits at handler index 1.
#[test]
fn spoofed_handler_next_to_ours_is_not_trusted() {
    let w = World::new();
    write_json(
        &w.project_hooks(),
        &json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": SPOOF},
            {"type": "command", "command": SPOOF}
        ]}]}}),
    );

    run_ok(&mut w.agora(&[
        "install",
        "--client",
        "codex",
        "--scope",
        "project",
        "--trust-hooks",
    ]));

    let text = read(&w.user_config());
    assert_eq!(trust_count(&text), CODEX_HOOK_COUNT, "{text}");
    assert!(!text.contains(":stop:0:1"), "{text}");
}

/// Uninstall removes the trust keys of our handlers and leaves a user's own trust entry alone.
#[test]
fn uninstall_removes_our_trust_and_keeps_other_trust() {
    let w = World::new();
    run_ok(&mut w.agora(&["install", "--client", "codex", "--trust-hooks"]));
    let hooks = fs::canonicalize(w.user_hooks()).expect("canonical hooks.json");
    let other = "/elsewhere/hooks.json:stop:0:0";
    let mut text = read(&w.user_config());
    assert_eq!(trust_count(&text), CODEX_HOOK_COUNT, "{text}");
    text.push_str(&format!(
        "\n[hooks.state.\"{other}\"]\ntrusted_hash = \"sha256:{}\"\n",
        "0".repeat(64)
    ));
    fs::write(w.user_config(), &text).expect("add a user trust entry");

    run_ok(&mut w.agora(&["uninstall", "--client", "codex"]));

    let left = read(&w.user_config());
    assert_eq!(trust_count(&left), 1, "{left}");
    assert!(left.contains(other), "{left}");
    assert!(!left.contains(&hooks.display().to_string()), "{left}");
}

/// An uninstall whose user-config write fails leaves hooks.json in place, so a rerun still removes the
/// trust entries. The user config is written before hooks.json, so this failure comes before any
/// handler is removed.
#[test]
fn failed_uninstall_write_then_rerun_removes_trust() {
    let w = World::new();
    run_ok(&mut w.agora(&[
        "install",
        "--client",
        "codex",
        "--scope",
        "project",
        "--trust-hooks",
    ]));
    assert_eq!(trust_count(&read(&w.user_config())), CODEX_HOOK_COUNT);

    let read_only = DirMode::set(&w.codex_home, 0o555);
    let failed = !w
        .agora(&["uninstall", "--client", "codex", "--scope", "project"])
        .output()
        .expect("agora runs")
        .status
        .success();
    assert!(
        failed,
        "uninstall must fail while the user config cannot be written"
    );
    let hooks = read(&w.project_hooks());
    assert!(hooks.contains("hook stop --dialect codex"), "{hooks}");
    assert_eq!(trust_count(&read(&w.user_config())), CODEX_HOOK_COUNT);
    drop(read_only);

    run_ok(&mut w.agora(&["uninstall", "--client", "codex", "--scope", "project"]));

    let left = read(&w.user_config());
    assert_eq!(trust_count(&left), 0, "{left}");
    assert!(!read(&w.project_hooks()).contains("hook stop --dialect codex"));
}
