//! Unit tests for the agy installer (SPEC 4.3 agy row; DOCS-ONLY shapes).

use super::*;
use crate::install::claude::{Mode, Options, Scope};
use crate::install::paths::ClientPaths;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;

/// A user's own `mcp_config.json` with one other server and a top-level key that must survive.
const USER_MCP: &str = "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"node\",\n      \"args\": [\"server.js\"]\n    }\n  },\n  \"theme\": \"dark\"\n}\n";
/// A user's own `hooks.json` with one other named hook that must survive.
const USER_HOOKS: &str = "{\n  \"mine\": {\n    \"enabled\": false,\n    \"Stop\": [{\"type\": \"command\", \"command\": \"/usr/local/bin/mine\", \"timeout\": 5}]\n  }\n}\n";
/// The user's own AGENTS.md text, before any agora block.
const USER_AGENTS: &str = "# My rules\n\nBe kind.\n";

/// A temp dir holding `home/` (the user home, so `~/.gemini` lands inside), `proj/` and the
/// options for one scope.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    proj: PathBuf,
    paths: ClientPaths,
    opts: Options,
}

impl Fixture {
    fn new(scope: Scope) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let proj = root.join("proj");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&proj).expect("proj");
        let env = |_: &str| -> Option<OsString> { None };
        let paths = ClientPaths::from_env(&home, &proj, &env).expect("paths");
        let opts = Options {
            scope,
            mode: Mode::Entries,
            dry_run: false,
            agora_home: root.join("agora"),
            user_home: home.clone(),
            project_dir: proj.clone(),
            exe: PathBuf::from("/opt/agora/bin/agora"),
        };
        Self {
            _dir: dir,
            root,
            home,
            proj,
            paths,
            opts,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            o: &self.opts,
            paths: &self.paths,
            trust_hooks: false,
        }
    }

    /// The user-scope files: `(mcp_config.json, hooks.json, AGENTS.md)`.
    fn user_files(&self) -> (PathBuf, PathBuf, PathBuf) {
        let gemini = self.home.join(".gemini");
        (
            gemini.join("config/mcp_config.json"),
            gemini.join("config/hooks.json"),
            gemini.join("AGENTS.md"),
        )
    }

    /// Writes the user's own MCP config, hooks and AGENTS.md (user scope).
    fn write_user_files(&self) {
        let (mcp, hooks, instr) = self.user_files();
        fs::create_dir_all(mcp.parent().expect("parent")).expect("config dir");
        fs::write(mcp, USER_MCP).expect("mcp");
        fs::write(hooks, USER_HOOKS).expect("hooks");
        fs::write(instr, USER_AGENTS).expect("agents");
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path)).expect("json")
}

fn results(report: &ClientReport) -> Vec<&str> {
    report.actions.iter().map(|a| a.result.as_str()).collect()
}

/// Every file under `dir` with its bytes, so a test can prove nothing changed.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(path.clone(), fs::read(&path).expect("file"));
            }
        }
    }
    out
}

#[test]
fn fresh_user_install_writes_exact_shapes() {
    let f = Fixture::new(Scope::User);
    let report = install(&f.ctx()).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);

    let (mcp, hooks, instr) = f.user_files();
    let home = f.opts.agora_home.display().to_string();
    let mcp_doc = read_json(&mcp);
    let server = &mcp_doc["mcpServers"]["agora"];
    assert_eq!(server["command"], "/opt/agora/bin/agora");
    assert_eq!(server["args"], json!(["--home", home, "mcp"]));

    let hooks_doc = read_json(&hooks);
    let hook = &hooks_doc["agora"];
    assert_eq!(hook["enabled"], true);
    let pre = hook["PreInvocation"].as_array().expect("PreInvocation");
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0]["type"], "command");
    assert_eq!(pre[0]["timeout"], 10);
    assert!(pre[0]["command"].as_str().is_some_and(|c| {
        c.contains("--dialect agy --event PreInvocation") && c.contains(" hook context ")
    }));
    let stop = hook["Stop"].as_array().expect("Stop");
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0]["timeout"], 10);
    assert!(
        stop[0]["command"]
            .as_str()
            .is_some_and(|c| c.contains("--dialect agy --event Stop") && c.contains(" hook stop "))
    );

    assert!(markers::has_block(&read(&instr)));
}

#[test]
fn fresh_install_reports_caps_and_docs_only_note() {
    let f = Fixture::new(Scope::User);
    let report = install(&f.ctx()).expect("install");
    let caps = &report.caps;
    assert_eq!(caps.scopes, vec!["user".to_owned()]);
    assert!(caps.mcp_configured && caps.context_hook && caps.stop_hook);
    assert_eq!(caps.hooks_trusted, None);
    assert!(!caps.owned_session);
    assert_eq!(caps.files.len(), 3);
    assert!(report.notes.iter().any(|n| n.contains("DOCS-ONLY")));
}

#[test]
fn existing_files_keep_other_entries() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    let report = install(&f.ctx()).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);

    let (mcp, hooks, instr) = f.user_files();
    let mcp_doc = read_json(&mcp);
    assert_eq!(mcp_doc["mcpServers"]["other"]["command"], "node");
    assert_eq!(mcp_doc["theme"], "dark");
    assert!(mcp_doc["mcpServers"]["agora"].is_object());

    let hooks_doc = read_json(&hooks);
    assert_eq!(
        hooks_doc["mine"]["Stop"][0]["command"],
        "/usr/local/bin/mine"
    );
    assert!(hooks_doc["agora"]["Stop"].is_array());

    let text = read(&instr);
    assert!(text.starts_with(USER_AGENTS));
    assert!(markers::has_block(&text));
}

#[test]
fn foreign_agora_entries_are_replaced_with_a_warning() {
    let f = Fixture::new(Scope::User);
    let (mcp, hooks, _) = f.user_files();
    fs::create_dir_all(mcp.parent().expect("parent")).expect("dir");
    fs::write(
        &mcp,
        "{\"mcpServers\":{\"agora\":{\"command\":\"/usr/bin/other\"}}}",
    )
    .expect("mcp");
    fs::write(&hooks, "{\"agora\":{\"enabled\":false}}").expect("hooks");
    let report = install(&f.ctx()).expect("install");
    assert!(report.actions[0].detail.contains("replaced"));
    assert!(report.actions[1].detail.contains("replaced"));
    assert_eq!(
        read_json(&mcp)["mcpServers"]["agora"]["command"],
        "/opt/agora/bin/agora"
    );
}

#[test]
fn second_install_is_unchanged_and_makes_no_backup() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx()).expect("first");
    let before = snapshot(&f.root);
    let report = install(&f.ctx()).expect("second");
    assert_eq!(results(&report), ["unchanged", "unchanged", "unchanged"]);
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn uninstall_restores_user_content() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx()).expect("install");
    let caps = CapabilitiesFile::default();
    let report = uninstall(&f.ctx(), &caps).expect("uninstall");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);

    let (mcp, hooks, instr) = f.user_files();
    let mcp_doc = read_json(&mcp);
    assert!(mcp_doc["mcpServers"].get("agora").is_none());
    assert_eq!(mcp_doc["mcpServers"]["other"]["command"], "node");
    assert_eq!(mcp_doc["theme"], "dark");
    let hooks_doc = read_json(&hooks);
    assert!(hooks_doc.get("agora").is_none());
    assert_eq!(
        hooks_doc["mine"]["Stop"][0]["command"],
        "/usr/local/bin/mine"
    );
    assert_eq!(read(&instr), USER_AGENTS);
}

#[test]
fn uninstall_of_fresh_install_leaves_empty_objects_then_absent_or_unchanged() {
    let f = Fixture::new(Scope::User);
    install(&f.ctx()).expect("install");
    let caps = CapabilitiesFile::default();
    uninstall(&f.ctx(), &caps).expect("first uninstall");
    let (mcp, hooks, _) = f.user_files();
    assert_eq!(read_json(&mcp), json!({}));
    assert_eq!(read_json(&hooks), json!({}));
    let again = uninstall(&f.ctx(), &caps).expect("second uninstall");
    assert_eq!(results(&again), ["unchanged", "unchanged", "unchanged"]);
}

#[test]
fn uninstall_on_empty_home_reports_absent() {
    let f = Fixture::new(Scope::User);
    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["absent", "absent", "absent"]);
}

#[test]
fn shared_agents_block_is_kept_for_another_client() {
    let f = Fixture::new(Scope::User);
    install(&f.ctx()).expect("install");
    let (_, _, instr) = f.user_files();
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::Codex,
        Some(HarnessCapabilities {
            files: vec![path_str(&instr)],
            ..Default::default()
        }),
    );
    let report = uninstall(&f.ctx(), &caps).expect("uninstall");
    assert_eq!(results(&report), ["wrote", "wrote", "kept"]);
    assert!(report.actions[2].detail.contains("codex"));
    assert!(markers::has_block(&read(&instr)));
}

#[test]
fn dry_run_writes_nothing() {
    let mut f = Fixture::new(Scope::User);
    f.write_user_files();
    let before = snapshot(&f.root);
    f.opts.dry_run = true;
    let report = install(&f.ctx()).expect("dry install");
    assert_eq!(results(&report), ["dry_run", "dry_run", "dry_run"]);
    assert_eq!(snapshot(&f.root), before);

    f.opts.dry_run = false;
    install(&f.ctx()).expect("real install");
    let before = snapshot(&f.root);
    f.opts.dry_run = true;
    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("dry uninstall");
    assert_eq!(results(&report), ["dry_run", "dry_run", "dry_run"]);
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn malformed_mcp_config_fails_before_any_write() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    let (mcp, _, _) = f.user_files();
    fs::write(&mcp, "{ not json").expect("corrupt");
    let before = snapshot(&f.root);
    let err = install(&f.ctx()).expect_err("malformed must fail");
    assert!(err.to_string().contains("mcp_config.json"));
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn non_object_mcp_servers_fails_before_any_write() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    let (mcp, _, _) = f.user_files();
    fs::write(&mcp, "{\"mcpServers\": [1, 2]}").expect("write");
    let before = snapshot(&f.root);
    assert!(install(&f.ctx()).is_err());
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn unbalanced_agents_markers_fail_before_any_write() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    let (_, _, instr) = f.user_files();
    fs::write(&instr, format!("{}\nno end\n", markers::BEGIN)).expect("write");
    let before = snapshot(&f.root);
    assert!(install(&f.ctx()).is_err());
    assert_eq!(snapshot(&f.root), before);
    assert!(uninstall(&f.ctx(), &CapabilitiesFile::default()).is_err());
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn project_scope_writes_under_the_project_only() {
    let f = Fixture::new(Scope::Project);
    let report = install(&f.ctx()).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);
    assert!(f.proj.join(".agents/mcp_config.json").is_file());
    assert!(f.proj.join(".agents/hooks.json").is_file());
    assert!(markers::has_block(&read(&f.proj.join("AGENTS.md"))));
    assert!(!f.home.join(".gemini").exists());
    assert_eq!(
        report.caps.scopes,
        vec![format!("project:{}", f.proj.display())]
    );
    let removed = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&removed), ["wrote", "wrote", "wrote"]);
    assert!(!markers::has_block(&read(&f.proj.join("AGENTS.md"))));
}
