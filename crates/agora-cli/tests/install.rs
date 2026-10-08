//! `agora install` / `agora uninstall` end to end. Each test runs the real `agora` binary with
//! HOME, AGORA_HOME and the working directory pointed into a fresh temp tree, a PATH without a
//! real `claude`, and a hard 30 s cap. The assertions read what lands on disk and the `--json`
//! output, because the files the installer writes are its contract.

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// A PATH that holds no `claude`.
const SAFE_PATH: &str = "/usr/bin:/bin";
/// Infix of every backup the installer keeps: `<name>.agora-bak-<ms>`.
const BACKUP_TAG: &str = ".agora-bak-";

/// One test's directory tree. `home` is HOME, `agora_home` is AGORA_HOME, `project` is the
/// working directory, `bin` holds stub executables. Removed when the test ends.
struct World {
    _root: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    agora_home: PathBuf,
    project: PathBuf,
    bin: PathBuf,
}

impl World {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let base = root.path().to_path_buf();
        let home = base.join("home");
        let project = base.join("project");
        let bin = base.join("bin");
        for dir in [&home, &project, &bin] {
            fs::create_dir_all(dir).expect("create test directory");
        }
        Self {
            _root: root,
            agora_home: base.join("agora"),
            // The installer records the canonical working directory for project scope.
            project: fs::canonicalize(&project).expect("canonical project dir"),
            home,
            bin,
            base,
        }
    }

    fn settings(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }

    fn project_settings(&self) -> PathBuf {
        self.project.join(".claude").join("settings.json")
    }

    fn project_mcp(&self) -> PathBuf {
        self.project.join(".mcp.json")
    }

    fn market(&self) -> PathBuf {
        self.agora_home.join("claude").join("marketplace")
    }

    fn capabilities(&self) -> PathBuf {
        self.agora_home.join("capabilities.json")
    }

    /// `agora <args>` in the test environment, text output.
    fn agora_text(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.args(args)
            .env("HOME", &self.home)
            .env("AGORA_HOME", &self.agora_home)
            .env("PATH", SAFE_PATH)
            .env_remove("AGORA_CLAUDE_BIN")
            .current_dir(&self.project)
            .timeout(CMD_TIMEOUT);
        cmd
    }

    /// `agora --json <args>` in the test environment.
    fn agora(&self, args: &[&str]) -> Command {
        let mut cmd = self.agora_text(&[]);
        cmd.arg("--json").args(args);
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

/// The `result` of the action on `path`; panics when the output has no action for it.
fn result_of(out: &Value, path: &Path) -> String {
    let want = path.display().to_string();
    out["actions"]
        .as_array()
        .expect("actions array")
        .iter()
        .find(|a| a["path"] == want)
        .and_then(|a| a["result"].as_str())
        .unwrap_or_else(|| panic!("no action for {want} in {out}"))
        .to_owned()
}

/// Every regular file under `root` mapped to its bytes, keyed by path relative to `root`.
/// Empty when `root` does not exist.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let rel = path.strip_prefix(root).expect("under root").to_path_buf();
                files.insert(rel, fs::read(&path).expect("read file"));
            }
        }
    }
    files
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

fn write_json(path: &Path, value: &Value) {
    let text = serde_json::to_string_pretty(value).expect("serialize");
    write_file(path, text.as_bytes());
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// The binary under test, as invoked (not canonicalized), as the installer records it.
fn exe() -> String {
    assert_cmd::cargo::cargo_bin("agora").display().to_string()
}

/// An executable `claude` stub at `dir/claude`: `--version` prints `version`, `--help` prints a
/// usage line that does not mention channels.
fn write_stub(dir: &Path, version: &str) -> PathBuf {
    let path = dir.join("claude");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"{version}\";;\n  --help) echo \"Usage: claude [options]\";;\nesac\n"
    );
    write_file(&path, script.as_bytes());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("stub mode");
    path
}

/// The repo's `plugin/claude/<rel>`, parsed.
fn plugin_file(rel: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("plugin")
        .join("claude")
        .join(rel);
    read_json(&path)
}

/// The repo's `hooks/hooks.json` with each `agora hook <sub>` command rewritten to the installed
/// form `'<exe>' --home '<home>' hook <sub>`. Modules, descriptions, timeouts and matchers stay as
/// they are in the file.
fn expected_hooks_doc(exe: &str, home: &str) -> Value {
    let mut doc = plugin_file("hooks/hooks.json");
    let events = doc["hooks"].as_object_mut().expect("hooks object");
    for groups in events.values_mut() {
        for group in groups.as_array_mut().expect("groups array") {
            for handler in group["hooks"].as_array_mut().expect("handlers array") {
                let cmd = handler["command"].as_str().expect("command").to_owned();
                let sub = cmd.strip_prefix("agora hook ").expect("agora hook command");
                handler["command"] = json!(format!("'{exe}' --home '{home}' hook {sub}"));
            }
        }
    }
    doc
}

/// The settings `hooks` entries mode adds for the plugin's hooks: one group per handler, carrying
/// the event's matcher when the file has one.
fn expected_entry_hooks(exe: &str, home: &str) -> Map<String, Value> {
    let doc = expected_hooks_doc(exe, home);
    let mut out = Map::new();
    for (event, groups) in doc["hooks"].as_object().expect("hooks object") {
        let mut list = Vec::new();
        for group in groups.as_array().expect("groups array") {
            for handler in group["hooks"].as_array().expect("handlers array") {
                let mut one = Map::new();
                if let Some(matcher) = group.get("matcher") {
                    one.insert("matcher".to_owned(), matcher.clone());
                }
                one.insert("hooks".to_owned(), json!([handler]));
                list.push(Value::Object(one));
            }
        }
        out.insert(event.clone(), Value::Array(list));
    }
    out
}

/// The `mcpServers.agora` entry that entries mode writes.
fn expected_mcp_entry(exe: &str, home: &str) -> Value {
    json!({
        "type": "stdio",
        "command": exe,
        "args": ["--home", home, "mcp"],
        "env": {"MCP_PROTOCOL_NEGOTIATION": "legacy"},
    })
}

/// The JSON array on the one `const AGORA = [...];` line of register.js.
fn agora_line(src: &str) -> Value {
    let lines: Vec<&str> = src
        .lines()
        .filter(|l| l.starts_with("const AGORA = "))
        .collect();
    assert_eq!(lines.len(), 1, "exactly one AGORA line in register.js");
    let body = lines[0]
        .trim_start_matches("const AGORA = ")
        .trim_end_matches(';');
    serde_json::from_str(body).expect("AGORA line holds a JSON array")
}

/// The `command` of the handler under `event` whose command ends with `suffix`.
fn hook_command(hooks_doc: &Value, event: &str, suffix: &str) -> String {
    for group in hooks_doc["hooks"][event].as_array().expect("event array") {
        for handler in group["hooks"].as_array().expect("handlers array") {
            if let Some(cmd) = handler["command"].as_str()
                && cmd.ends_with(suffix)
            {
                return cmd.to_owned();
            }
        }
    }
    panic!("no {event} handler ending with {suffix:?}");
}

fn install_plugin(w: &World) -> Value {
    run_ok(&mut w.agora(&["install", "--client", "claude"]))
}

#[test]
fn plugin_install_materializes_the_marketplace_with_absolute_commands() {
    let w = World::new();
    install_plugin(&w);
    let exe = exe();
    let home = w.agora_home.display().to_string();
    let plugin = w.market().join("agora");

    let market = read_json(&w.market().join(".claude-plugin").join("marketplace.json"));
    assert_eq!(market["name"], "agora");
    assert_eq!(market["plugins"][0]["name"], "agora");
    assert_eq!(market["plugins"][0]["source"], "./agora");

    let manifest = read_json(&plugin.join(".claude-plugin").join("plugin.json"));
    assert_eq!(manifest["name"], "agora");

    let mcp = read_json(&plugin.join(".mcp.json"));
    let server = &mcp["mcpServers"]["agora"];
    assert_eq!(server["command"], exe);
    assert_eq!(server["args"], json!(["--home", home, "mcp"]));
    assert_eq!(server["env"]["MCP_PROTOCOL_NEGOTIATION"], "legacy");
    assert_eq!(server["env"]["AGORA_HOME"], home);
    assert_eq!(server["env"]["AGORA_FOLDER"], "${CLAUDE_PROJECT_DIR}");

    assert_eq!(
        read_json(&plugin.join("hooks").join("hooks.json")),
        expected_hooks_doc(&exe, &home),
        "hook commands are absolute; modules, timeouts and matchers are kept"
    );

    let register = fs::read_to_string(plugin.join("hooks").join("register.js")).expect("register");
    assert_eq!(agora_line(&register), json!([exe, "--home", home]));
}

#[test]
fn plugin_install_enables_the_marketplace_in_user_settings() {
    let w = World::new();
    install_plugin(&w);
    let market = w.market().display().to_string();
    assert_eq!(
        read_json(&w.settings()),
        json!({
            "enabledPlugins": {"agora@agora": true},
            "extraKnownMarketplaces": {
                "agora": {"source": {"source": "directory", "path": market}}
            },
        })
    );
}

#[test]
fn plugin_install_records_capabilities_without_a_probe() {
    let w = World::new();
    install_plugin(&w);
    let file = read_json(&w.capabilities());
    assert_eq!(file["schema"], 1);
    let claude = &file["claude"];
    assert_eq!(claude["install_mode"], "plugin");
    assert_eq!(claude["scope"], "user");
    assert_eq!(claude["stop_hook"].as_bool(), Some(true));
    assert_eq!(claude["context_hook"].as_bool(), Some(true));
    assert_eq!(claude["async_rewake"].as_bool(), Some(true));
    assert_eq!(claude["mods"].as_bool(), Some(false), "no probe, no mods");
    assert_eq!(mode_of(&w.capabilities()), 0o600);
}

#[test]
fn plugin_install_output_names_scope_mode_and_each_written_file() {
    let w = World::new();
    let out = install_plugin(&w);
    assert_eq!(out["client"], "claude");
    assert_eq!(out["scope"], "user");
    assert_eq!(out["mode"], "plugin");
    assert_eq!(out["dry_run"].as_bool(), Some(false));
    assert!(out["probe"].is_null(), "no --probe, no probe document");
    assert_eq!(result_of(&out, &w.settings()), "wrote");
    assert_eq!(
        result_of(
            &out,
            &w.market().join(".claude-plugin").join("marketplace.json")
        ),
        "wrote"
    );
}

#[test]
fn plugin_install_text_output_gives_the_marketplace_add_command() {
    let w = World::new();
    let out = w
        .agora_text(&["install", "--client", "claude"])
        .output()
        .expect("agora runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let hint = format!(
        "claude plugin marketplace add {} && claude plugin install agora@agora",
        w.market().display()
    );
    assert!(text.contains(&hint), "{text}");
}

#[test]
fn second_plugin_install_changes_nothing() {
    let w = World::new();
    install_plugin(&w);
    let before = snapshot(&w.base);

    let out = install_plugin(&w);
    for action in out["actions"].as_array().expect("actions array") {
        let path = action["path"].as_str().expect("path");
        if !path.ends_with("capabilities.json") {
            assert_eq!(action["result"], "unchanged", "{path}");
        }
    }
    let after = snapshot(&w.base);
    assert!(
        after
            .keys()
            .all(|k| !k.to_string_lossy().contains(BACKUP_TAG)),
        "no backup is made for an unchanged file"
    );
    assert_eq!(after, before);
}

#[test]
fn plugin_install_merges_into_existing_settings_and_backs_up_the_original() {
    let w = World::new();
    let original = br#"{"theme":"dark","enabledPlugins":{"other@x":true},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo user"}]}]}}"#;
    write_file(&w.settings(), original);

    install_plugin(&w);

    let market = w.market().display().to_string();
    assert_eq!(
        read_json(&w.settings()),
        json!({
            "theme": "dark",
            "enabledPlugins": {"other@x": true, "agora@agora": true},
            "extraKnownMarketplaces": {
                "agora": {"source": {"source": "directory", "path": market}}
            },
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo user"}]}]},
        })
    );
    let backups = backups_of(&w.settings());
    assert_eq!(backups.len(), 1, "one backup of the file it replaced");
    assert_eq!(fs::read(&backups[0]).expect("backup"), original);
}

#[test]
fn plugin_install_refuses_unparseable_settings_and_writes_nothing() {
    let w = World::new();
    write_file(&w.settings(), b"{not json");
    let before = snapshot(&w.base);

    let out = run(&mut w.agora(&["install", "--client", "claude"]));
    assert!(!out.ok, "{}", out.json);
    let message = error_message(&out);
    assert!(
        message.contains(&w.settings().display().to_string()),
        "the error names the file: {message}"
    );
    assert_eq!(
        snapshot(&w.base),
        before,
        "nothing is written, not even the marketplace"
    );
}

#[test]
fn dry_run_reports_each_action_and_writes_nothing() {
    let w = World::new();
    write_file(&w.settings(), br#"{"theme":"dark"}"#);
    let before = snapshot(&w.base);

    let out = run_ok(&mut w.agora(&["install", "--client", "claude", "--dry-run"]));
    assert_eq!(out["dry_run"].as_bool(), Some(true));
    let actions = out["actions"].as_array().expect("actions array");
    assert!(!actions.is_empty());
    for action in actions {
        assert_eq!(action["result"], "dry_run", "{action}");
    }
    assert_eq!(snapshot(&w.base), before, "no file created or changed");
}

#[test]
fn plugin_uninstall_removes_the_wiring_and_keeps_user_keys() {
    let w = World::new();
    write_json(
        &w.settings(),
        &json!({"theme": "dark", "enabledPlugins": {"other@x": true}}),
    );
    install_plugin(&w);

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "claude"]));

    assert_eq!(
        read_json(&w.settings()),
        json!({"theme": "dark", "enabledPlugins": {"other@x": true}}),
        "our enable flag and marketplace entry are gone; the user's keys are not"
    );
    assert!(!w.market().exists(), "the marketplace directory is deleted");
    assert_eq!(result_of(&out, &w.market()), "removed");
    let caps = read_json(&w.capabilities());
    assert!(caps.get("claude").is_none(), "no claude entry: {caps}");
}

#[test]
fn plugin_uninstall_keeps_a_marketplace_entry_that_points_elsewhere() {
    let w = World::new();
    let elsewhere = w.base.join("elsewhere").display().to_string();
    write_json(
        &w.settings(),
        &json!({
            "enabledPlugins": {"agora@agora": true},
            "extraKnownMarketplaces": {
                "agora": {"source": {"source": "directory", "path": elsewhere}}
            },
        }),
    );

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "claude"]));

    assert_eq!(
        read_json(&w.settings()),
        json!({
            "extraKnownMarketplaces": {
                "agora": {"source": {"source": "directory", "path": elsewhere}}
            },
        }),
        "only the enable flag goes; a marketplace this install did not add is kept"
    );
    assert_eq!(result_of(&out, &w.market()), "absent");
}

#[test]
fn second_plugin_uninstall_reports_nothing_to_remove() {
    let w = World::new();
    install_plugin(&w);
    run_ok(&mut w.agora(&["uninstall", "--client", "claude"]));
    let before = snapshot(&w.base);

    let out = run_ok(&mut w.agora(&["uninstall", "--client", "claude"]));
    assert_eq!(result_of(&out, &w.settings()), "unchanged");
    assert_eq!(result_of(&out, &w.market()), "absent");
    assert_eq!(result_of(&out, &w.capabilities()), "unchanged");
    assert_eq!(snapshot(&w.base), before);
}

/// Seeds for project scope: a user MCP server and user hooks that must survive install and
/// uninstall untouched.
fn seed_project(w: &World) -> (Value, Value) {
    let mcp = json!({
        "mcpServers": {"userdocs": {"type": "stdio", "command": "node", "args": ["docs.js"]}},
    });
    let settings = json!({
        "theme": "dark",
        "hooks": {
            // The second command ends like ours but names no agora: it is not ours to remove.
            "Stop": [{"hooks": [
                {"type": "command", "command": "echo user"},
                {"type": "command", "command": "/usr/local/bin/lint hook stop"},
            ]}],
            "PostToolUse": [{
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "echo bash"}],
            }],
        },
    });
    write_json(&w.project_mcp(), &mcp);
    write_json(&w.project_settings(), &settings);
    (mcp, settings)
}

fn install_entries(w: &World) -> Value {
    run_ok(&mut w.agora(&[
        "install", "--client", "claude", "--scope", "project", "--mode", "entries",
    ]))
}

fn uninstall_entries(w: &World) -> Value {
    run_ok(&mut w.agora(&[
        "uninstall",
        "--client",
        "claude",
        "--scope",
        "project",
        "--mode",
        "entries",
    ]))
}

#[test]
fn entries_install_adds_the_mcp_server_and_hooks_beside_user_entries() {
    let w = World::new();
    let (mut mcp, settings) = seed_project(&w);
    let out = install_entries(&w);
    assert_eq!(out["scope"], "project");
    assert_eq!(out["mode"], "entries");

    let exe = exe();
    let home = w.agora_home.display().to_string();
    mcp["mcpServers"]["agora"] = expected_mcp_entry(&exe, &home);
    assert_eq!(read_json(&w.project_mcp()), mcp);

    let mut expected = settings;
    let hooks = expected["hooks"].as_object_mut().expect("hooks object");
    for (event, groups) in expected_entry_hooks(&exe, &home) {
        let list = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        list.as_array_mut()
            .expect("event array")
            .extend(groups.as_array().expect("groups").iter().cloned());
    }
    assert_eq!(read_json(&w.project_settings()), expected);

    // The two rewake handlers: async, with their timeouts from the plugin file.
    let installed = read_json(&w.project_settings());
    let stop = &installed["hooks"]["Stop"];
    // Index 0 is the user's group; ours are the stop and rewake groups after it.
    assert_eq!(stop[2]["hooks"][0]["asyncRewake"].as_bool(), Some(true));
    assert_eq!(stop[2]["hooks"][0]["timeout"], 3600);
}

#[test]
fn entries_uninstall_keeps_the_user_entries_exactly() {
    let w = World::new();
    let (mcp, settings) = seed_project(&w);
    install_entries(&w);

    uninstall_entries(&w);

    assert_eq!(read_json(&w.project_mcp()), mcp);
    assert_eq!(read_json(&w.project_settings()), settings);
}

#[test]
fn entries_uninstall_keeps_an_agora_server_whose_command_is_not_ours() {
    let w = World::new();
    let mcp = json!({
        "mcpServers": {"agora": {"type": "stdio", "command": "node", "args": ["mine.js"]}},
    });
    write_json(&w.project_mcp(), &mcp);

    uninstall_entries(&w);

    assert_eq!(read_json(&w.project_mcp()), mcp);
}

#[test]
fn entries_uninstall_drops_the_containers_it_emptied() {
    let w = World::new();
    install_entries(&w);

    uninstall_entries(&w);

    assert_eq!(read_json(&w.project_mcp()), json!({}), "mcpServers dropped");
    assert_eq!(read_json(&w.project_settings()), json!({}), "hooks dropped");
}

#[test]
fn entries_reinstall_migrates_an_older_exe_instead_of_duplicating_the_hook() {
    let w = World::new();
    install_entries(&w);
    let first = read_json(&w.project_settings());

    // A hook written by an install from another binary location.
    let home = w.agora_home.display().to_string();
    let mut stale = first.clone();
    stale["hooks"]["Stop"][0]["hooks"][0]["command"] =
        json!(format!("'/old/agora' --home '{home}' hook stop"));
    write_json(&w.project_settings(), &stale);

    install_entries(&w);

    assert_eq!(
        read_json(&w.project_settings()),
        first,
        "the stale handler is replaced in place: one stop handler, the current exe"
    );
}

#[test]
fn probe_reads_the_claude_version_and_enables_mods() {
    let w = World::new();
    let stub = write_stub(&w.bin, "2.1.293 (Claude Code)");

    let mut cmd = w.agora(&["install", "--client", "claude", "--probe"]);
    cmd.env("PATH", format!("{}:{SAFE_PATH}", w.bin.display()));
    let out = run_ok(&mut cmd);

    assert_eq!(out["probe"]["claude_bin"], stub.display().to_string());
    assert_eq!(out["probe"]["version"], "2.1.293 (Claude Code)");
    assert_eq!(out["probe"]["mods_supported"].as_bool(), Some(true));
    assert_eq!(out["probe"]["channels_flag"].as_bool(), Some(true));
    assert_eq!(out["capabilities"]["mods"].as_bool(), Some(true));
    assert_eq!(out["capabilities"]["claude_version"], "2.1.293");
    assert_eq!(
        read_json(&w.capabilities())["claude"]["mods"].as_bool(),
        Some(true)
    );
}

#[test]
fn probe_override_selects_an_older_claude_that_gets_no_mods() {
    let w = World::new();
    // A 2.1.293 on PATH must lose to the override.
    write_stub(&w.bin, "2.1.293 (Claude Code)");
    let old = write_stub(&w.base.join("old"), "2.1.200 (Claude Code)");

    let mut cmd = w.agora(&["install", "--client", "claude", "--probe"]);
    cmd.env("PATH", format!("{}:{SAFE_PATH}", w.bin.display()))
        .env("AGORA_CLAUDE_BIN", &old);
    let out = run_ok(&mut cmd);

    assert_eq!(out["probe"]["claude_bin"], old.display().to_string());
    assert_eq!(out["probe"]["mods_supported"].as_bool(), Some(false));
    assert_eq!(out["capabilities"]["mods"].as_bool(), Some(false));
    assert_eq!(out["capabilities"]["claude_version"], "2.1.200");
}

#[test]
fn unknown_client_is_refused_and_points_at_m6() {
    let w = World::new();
    let before = snapshot(&w.base);

    let out = run(&mut w.agora(&["install", "--client", "codex"]));
    assert!(!out.ok, "{}", out.json);
    assert!(error_message(&out).contains("M6"), "{}", out.json);
    assert_eq!(snapshot(&w.base), before, "nothing is written");
}

#[test]
fn symlinked_settings_are_refused_and_the_target_is_untouched() {
    let w = World::new();
    let target = w.base.join("elsewhere.json");
    let original = br#"{"theme":"dark"}"#;
    write_file(&target, original);
    fs::create_dir_all(w.settings().parent().expect("parent")).expect("claude dir");
    std::os::unix::fs::symlink(&target, w.settings()).expect("symlink");

    let out = run(&mut w.agora(&["install", "--client", "claude"]));

    assert!(!out.ok, "{}", out.json);
    assert!(error_message(&out).contains("symlink"), "{}", out.json);
    assert_eq!(fs::read(&target).expect("target"), original);
    assert!(
        fs::symlink_metadata(w.settings())
            .expect("settings link")
            .file_type()
            .is_symlink(),
        "the link itself is left in place"
    );
}

#[test]
fn installed_session_start_hook_runs_under_sh_with_a_space_in_the_home() {
    let mut w = World::new();
    w.agora_home = w.base.join("with space").join("agora");
    install_plugin(&w);
    // The hook loads the machine seed (32 bytes, mode 0600); without one it stays silent, so a
    // non-empty answer proves the quoted --home reached the right directory.
    let seed = w.agora_home.join("machine.seed");
    write_file(&seed, &[7u8; 32]);
    fs::set_permissions(&seed, fs::Permissions::from_mode(0o600)).expect("seed mode");

    let hooks = read_json(&w.market().join("agora").join("hooks").join("hooks.json"));
    let cmd = hook_command(&hooks, "SessionStart", " hook session-start");
    // A real SessionStart payload: the hook answers only for its own event name.
    let event = json!({
        "session_id": "install-test-session",
        "cwd": w.project,
        "hook_event_name": "SessionStart",
        "source": "startup",
    });

    let out = assert_cmd::Command::new("/bin/sh")
        .args(["-c", &cmd])
        .env("PATH", SAFE_PATH)
        .env("HOME", &w.home)
        .current_dir(&w.project)
        .write_stdin(event.to_string())
        .timeout(CMD_TIMEOUT)
        .output()
        .expect("sh runs");

    assert!(out.status.success(), "fail-open hooks exit 0: {cmd}");
    let stdout = String::from_utf8(out.stdout).expect("UTF-8 stdout");
    let doc: Value = serde_json::from_str(&stdout).expect("hook output is JSON");
    let context = doc["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default();
    assert!(!context.is_empty(), "identity context from {cmd}");
}
