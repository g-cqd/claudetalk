//! Unit tests for the Cursor installer (SPEC 2.6 and 4.3 cursor row; DOCS-ONLY shapes).

use super::*;
use crate::install::claude::{Mode, Options, Scope};
use crate::install::instructions::cursor_rule;
use crate::install::paths::ClientPaths;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;

/// The user's own `mcp.json`: one other server and a top-level key that must survive.
const USER_MCP: &str = "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"node\",\n      \"args\": [\"server.js\"]\n    }\n  },\n  \"theme\": \"dark\"\n}\n";
/// The user's own `hooks.json`: version 1 and one user `stop` hook.
const USER_HOOKS: &str = "{\n  \"version\": 1,\n  \"hooks\": {\n    \"stop\": [{\"command\": \"/usr/local/bin/mine\", \"timeout\": 5}]\n  }\n}\n";
/// A project rule that agora did not write (no marker).
const FOREIGN_RULE: &str = "# my own rule\n\nBe kind.\n";

/// A temp dir with `home/` (the user home), `proj/` and the options for one scope.
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

    /// `~/.cursor` in user scope, `<proj>/.cursor` in project scope.
    fn cursor_dir(&self) -> PathBuf {
        match self.opts.scope {
            Scope::User => self.home.join(".cursor"),
            Scope::Project => self.proj.join(".cursor"),
        }
    }

    fn mcp_path(&self) -> PathBuf {
        self.cursor_dir().join("mcp.json")
    }

    fn hooks_path(&self) -> PathBuf {
        self.cursor_dir().join("hooks.json")
    }

    fn rule_path(&self) -> PathBuf {
        self.proj.join(".cursor/rules/agora.mdc")
    }

    /// Writes the user's own `mcp.json` and `hooks.json` in this scope.
    fn write_user_files(&self) {
        fs::create_dir_all(self.cursor_dir()).expect("cursor dir");
        fs::write(self.mcp_path(), USER_MCP).expect("mcp");
        fs::write(self.hooks_path(), USER_HOOKS).expect("hooks");
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json")
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("json")
}

fn results(report: &ClientReport) -> Vec<&str> {
    report.actions.iter().map(|a| a.result.as_str()).collect()
}

/// What one path under the snapshot holds.
#[derive(Debug, PartialEq, Eq)]
enum Node {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every path under `dir` (directories and symlinks included, symlinks not followed) with what it
/// holds, so a test can prove nothing changed.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Node> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            let kind = fs::symlink_metadata(&path).expect("lstat").file_type();
            let node = if kind.is_symlink() {
                Node::Link(fs::read_link(&path).expect("read_link"))
            } else if kind.is_dir() {
                stack.push(path.clone());
                Node::Dir
            } else {
                Node::File(fs::read(&path).expect("file"))
            };
            out.insert(path, node);
        }
    }
    out
}

#[test]
fn fresh_user_install_writes_exact_shapes() {
    let f = Fixture::new(Scope::User);
    let report = install(&f.ctx()).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote"]);

    let home = f.opts.agora_home.display().to_string();
    let mcp = read_json(&f.mcp_path());
    assert_eq!(
        mcp["mcpServers"]["agora"],
        json!({
            "type": "stdio",
            "command": "/opt/agora/bin/agora",
            "args": ["--home", home, "mcp"],
            "env": {"AGORA_FOLDER": "${workspaceFolder}"},
        })
    );

    // The command comes from `Ctx::hook_cmd`, which quotes the exe and home only when needed.
    let command = |kind: &str| f.ctx().hook_cmd(kind, "cursor", None).expect("hook cmd");
    let hooks = read_json(&f.hooks_path());
    assert_eq!(hooks["version"], 1);
    assert_eq!(
        hooks["hooks"]["sessionStart"],
        json!([{"command": command("session-start"), "timeout": 10}])
    );
    assert_eq!(
        hooks["hooks"]["stop"],
        json!([{"command": command("stop"), "timeout": 10, "loop_limit": 5}])
    );
    for event in ["postToolUse", "afterMCPExecution"] {
        assert_eq!(
            hooks["hooks"][event],
            json!([{"command": command("context"), "timeout": 10}])
        );
    }

    assert!(report.notes.iter().any(|n| n.contains("DOCS-ONLY")));
    assert!(report.notes.iter().any(|n| {
        n.contains("Cursor Settings → Rules") && n.contains(".cursor/rules/agora.mdc")
    }));
    assert_eq!(report.caps.scopes, ["user"]);
    assert!(report.caps.mcp_configured && report.caps.context_hook && report.caps.stop_hook);
    assert_eq!(report.caps.hooks_trusted, None);
    assert_eq!(report.caps.files.len(), 2);
    assert!(!f.rule_path().exists());
}

#[test]
fn stop_loop_limit_is_the_shared_hooks_constant() {
    let f = Fixture::new(Scope::User);
    install(&f.ctx()).expect("install");

    let hooks = read_json(&f.hooks_path());
    let stop = hooks["hooks"]["stop"].as_array().expect("stop array");
    assert_eq!(
        stop[0]["loop_limit"],
        json!(agora_hooks::dialect::LOOP_LIMIT)
    );
}

#[test]
fn project_install_writes_owned_rule_with_frontmatter() {
    let f = Fixture::new(Scope::Project);
    let report = install(&f.ctx()).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);

    let rule = fs::read_to_string(f.rule_path()).expect("rule");
    assert_eq!(rule, cursor_rule());
    assert!(rule.starts_with(
        "---\ndescription: Agora inbox and coordination rules\nalwaysApply: true\n---\n"
    ));
    assert!(markers::has_block(&rule));
    assert!(report.caps.files.iter().any(|p| p.ends_with("agora.mdc")));
    assert_eq!(report.caps.scopes.len(), 1);
    assert!(report.caps.scopes[0].starts_with("project:"));
    assert!(!report.notes.iter().any(|n| n.contains("Cursor Settings")));
}

#[test]
fn user_hooks_are_preserved_and_stop_is_appended() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx()).expect("install");

    let hooks = read_json(&f.hooks_path());
    assert_eq!(hooks["version"], 1);
    let stop = hooks["hooks"]["stop"].as_array().expect("stop array");
    assert_eq!(stop.len(), 2);
    assert_eq!(
        stop[0],
        json!({"command": "/usr/local/bin/mine", "timeout": 5})
    );
    assert_eq!(stop[1]["loop_limit"], 5);
    assert!(
        stop[1]["command"]
            .as_str()
            .is_some_and(|c| c.contains(" hook stop --dialect cursor"))
    );
    assert!(
        hooks["hooks"]["sessionStart"]
            .as_array()
            .is_some_and(|a| a.len() == 1)
    );
    assert!(
        hooks["hooks"]["postToolUse"]
            .as_array()
            .is_some_and(|a| a.len() == 1)
    );
    assert!(
        hooks["hooks"]["afterMCPExecution"]
            .as_array()
            .is_some_and(|a| a.len() == 1)
    );

    let mcp = read_json(&f.mcp_path());
    assert!(mcp["mcpServers"]["other"].is_object());
    assert_eq!(mcp["theme"], "dark");
}

#[test]
fn second_install_is_unchanged_and_writes_no_backup() {
    let f = Fixture::new(Scope::Project);
    install(&f.ctx()).expect("first install");
    let before = snapshot(&f.root);

    let report = install(&f.ctx()).expect("second install");
    assert_eq!(results(&report), ["unchanged", "unchanged", "unchanged"]);
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn uninstall_restores_user_files_exactly() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx()).expect("install");

    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    assert_eq!(read_json(&f.mcp_path()), parse(USER_MCP));
    assert_eq!(read_json(&f.hooks_path()), parse(USER_HOOKS));
}

#[test]
fn uninstall_of_our_only_files_leaves_empty_objects_and_removes_rule() {
    let f = Fixture::new(Scope::Project);
    install(&f.ctx()).expect("install");

    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["wrote", "wrote", "removed"]);
    assert_eq!(read_json(&f.mcp_path()), json!({}));
    assert_eq!(read_json(&f.hooks_path()), json!({}));
    assert!(!f.rule_path().exists());

    let again = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("second uninstall");
    assert_eq!(results(&again), ["unchanged", "unchanged", "absent"]);
}

#[test]
fn dry_run_writes_nothing() {
    let mut f = Fixture::new(Scope::Project);
    f.opts.dry_run = true;
    let before = snapshot(&f.root);

    let report = install(&f.ctx()).expect("dry-run install");
    assert_eq!(results(&report), ["dry_run", "dry_run", "dry_run"]);
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn malformed_mcp_json_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(f.cursor_dir()).expect("cursor dir");
    fs::write(f.mcp_path(), "{ not json").expect("mcp");
    let before = snapshot(&f.root);

    let err = install(&f.ctx()).expect_err("malformed mcp.json must fail");
    assert!(err.to_string().contains("mcp.json"));
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn non_object_hooks_member_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(f.cursor_dir()).expect("cursor dir");
    fs::write(f.hooks_path(), "{\"hooks\": []}").expect("hooks");
    let before = snapshot(&f.root);

    let err = install(&f.ctx()).expect_err("non-object hooks must fail");
    assert!(err.to_string().contains("hooks"));
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn non_array_hook_event_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::Project);
    f.write_user_files();
    fs::write(
        f.hooks_path(),
        "{\"version\": 1, \"hooks\": {\"stop\": \"x\"}}",
    )
    .expect("hooks");
    let before = snapshot(&f.root);

    let err = install(&f.ctx()).expect_err("non-array hook event must fail");
    assert!(err.to_string().contains("hooks.stop"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn symlinked_mcp_json_is_refused_and_tree_is_untouched() {
    let f = Fixture::new(Scope::Project);
    let real = f.root.join("elsewhere/real-mcp.json");
    fs::create_dir_all(real.parent().expect("parent")).expect("elsewhere dir");
    fs::write(&real, USER_MCP).expect("real mcp");
    fs::create_dir_all(f.cursor_dir()).expect("cursor dir");
    std::os::unix::fs::symlink(&real, f.mcp_path()).expect("symlink");
    let before = snapshot(&f.root);

    let err = install(&f.ctx()).expect_err("symlinked mcp.json must fail");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(snapshot(&f.root), before);
    assert!(!f.hooks_path().exists());
    assert!(!f.rule_path().exists());
}

#[test]
fn foreign_rule_file_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::Project);
    fs::create_dir_all(f.rule_path().parent().expect("parent")).expect("rules dir");
    fs::write(f.rule_path(), FOREIGN_RULE).expect("rule");
    let before = snapshot(&f.root);

    let err = install(&f.ctx()).expect_err("foreign agora.mdc must fail");
    assert!(err.to_string().contains("not managed by agora"));
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn uninstall_keeps_foreign_rule_file() {
    let f = Fixture::new(Scope::Project);
    fs::create_dir_all(f.rule_path().parent().expect("parent")).expect("rules dir");
    fs::write(f.rule_path(), FOREIGN_RULE).expect("rule");

    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["absent", "absent", "kept"]);
    assert_eq!(
        fs::read_to_string(f.rule_path()).expect("rule"),
        FOREIGN_RULE
    );
}
