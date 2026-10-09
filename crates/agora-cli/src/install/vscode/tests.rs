use super::*;
use crate::install::claude::{Mode, Options, Scope};
use crate::install::copilot_cli;
use crate::install::paths::ClientPaths;
use std::ffi::OsString;
use std::fs;

/// A VS Code `mcp.json` the user already has: one server of their own and an `inputs` array.
const USER_MCP: &str = r#"{"servers":{"other":{"type":"stdio","command":"/usr/bin/other","args":[]}},"inputs":[{"type":"promptString","id":"token","description":"t"}]}"#;
/// A hooks file that is not ours.
const FOREIGN_HOOKS: &str =
    r#"{"version":1,"hooks":{"sessionStart":[{"type":"command","bash":"echo mine"}]}}"#;
/// A user instructions file with no agora block.
const FOREIGN_INSTRUCTIONS: &str = "# my own instructions\n";

/// A temp dir holding `home/`, `proj/` and the VS Code user dir (`AGORA_VSCODE_USER_DIR`).
struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    proj: PathBuf,
    vscode: PathBuf,
    paths: ClientPaths,
    opts: Options,
}

impl Fixture {
    fn new(scope: Scope, dry_run: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let proj = root.join("proj");
        let vscode = root.join("Code/User");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&proj).expect("proj");
        let vscode_env = vscode.clone();
        let env = move |key: &str| -> Option<OsString> {
            (key == "AGORA_VSCODE_USER_DIR").then(|| vscode_env.clone().into_os_string())
        };
        let paths = ClientPaths::from_env(&home, &proj, &env).expect("paths");
        let opts = Options {
            scope,
            mode: Mode::Entries,
            dry_run,
            agora_home: root.join("agora"),
            user_home: home.clone(),
            project_dir: proj.clone(),
            exe: PathBuf::from("/opt/agora/bin/agora"),
        };
        Self {
            _dir: dir,
            home,
            proj,
            vscode,
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

    fn user_mcp(&self) -> PathBuf {
        self.vscode.join("mcp.json")
    }

    fn user_hooks(&self) -> PathBuf {
        self.home.join(".copilot/hooks/agora.json")
    }

    fn user_instructions(&self) -> PathBuf {
        self.home
            .join(".copilot/instructions/agora.instructions.md")
    }

    fn proj_mcp(&self) -> PathBuf {
        self.proj.join(".vscode/mcp.json")
    }

    fn proj_hooks(&self) -> PathBuf {
        self.proj.join(".github/hooks/agora.json")
    }

    fn proj_instructions(&self) -> PathBuf {
        self.proj.join(".github/copilot-instructions.md")
    }

    /// Create the VS Code user dir and put `text` into the user `mcp.json`.
    fn write_user_mcp(&self, text: &str) {
        fs::create_dir_all(&self.vscode).expect("vscode dir");
        fs::write(self.user_mcp(), text).expect("write mcp");
    }
}

fn json_file(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read");
    serde_json::from_str(&text).expect("json")
}

fn json_literal(text: &str) -> Value {
    serde_json::from_str(text).expect("json literal")
}

/// The bytes the shared hooks file must hold: `copilot_hooks_doc`, pretty, with a final newline.
fn expected_hooks_bytes(f: &Fixture) -> String {
    let doc = copilot_hooks_doc(&f.ctx()).expect("hooks doc");
    let mut text = serde_json::to_string_pretty(&doc).expect("pretty");
    text.push('\n');
    text
}

/// Every path under `root` with what it holds (a directory, a file's bytes, a symlink's target),
/// sorted, so two snapshots compare equal exactly when the trees are byte-identical.
fn snapshot(root: &Path) -> Vec<String> {
    fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let rel = path.strip_prefix(root).expect("prefix").display();
            let meta = fs::symlink_metadata(&path).expect("lstat");
            if meta.file_type().is_symlink() {
                out.push(format!(
                    "{rel} -> {}",
                    fs::read_link(&path).expect("link").display()
                ));
            } else if meta.is_dir() {
                out.push(format!("{rel}/"));
                collect(root, &path, out);
            } else {
                out.push(format!("{rel} = {:?}", fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort();
    out
}

/// The action whose path is `path`.
fn action_for<'a>(report: &'a ClientReport, path: &Path) -> &'a Action {
    let key = path.display().to_string();
    report
        .actions
        .iter()
        .find(|a| a.path == key)
        .expect("an action for the path")
}

#[test]
fn fresh_user_install_writes_exact_shapes() {
    let f = Fixture::new(Scope::User, false);
    let report = install(&f.ctx()).expect("install");
    let agora_home = f.opts.agora_home.to_str().expect("utf8").to_owned();

    assert_eq!(
        json_file(&f.user_mcp()),
        json!({"servers": {"agora": {
            "type": "stdio",
            "command": "/opt/agora/bin/agora",
            "args": ["--home", agora_home, "mcp"],
            "env": {"AGORA_FOLDER": "${workspaceFolder}"}
        }}})
    );
    assert_eq!(
        fs::read_to_string(f.user_hooks()).expect("hooks"),
        expected_hooks_bytes(&f)
    );
    let instructions = fs::read_to_string(f.user_instructions()).expect("instructions");
    assert_eq!(instructions, instructions::vscode_user_instructions());
    assert!(instructions.starts_with("---\napplyTo: \"**\"\n---\n"));
    assert!(report.actions.iter().all(|a| a.result == "wrote"));
    assert!(report.caps.mcp_configured);
    assert!(report.caps.context_hook && report.caps.stop_hook);
    assert_eq!(report.caps.scopes, vec!["user".to_owned()]);
    assert_eq!(report.caps.files.len(), 3);
    assert_eq!(report.notes.len(), 2);
    assert!(report.notes[0].contains("chat.useHooks"));
    assert!(report.notes[1].contains("inbox_wait"));
}

#[test]
fn copilot_cli_hooks_file_is_shared_and_unchanged() {
    let f = Fixture::new(Scope::User, false);
    copilot_cli::install(&f.ctx()).expect("copilot-cli install");
    let report = install(&f.ctx()).expect("vscode install");

    assert_eq!(action_for(&report, &f.user_hooks()).result, "unchanged");
}

#[test]
fn existing_mcp_config_keeps_other_server_and_inputs() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp(USER_MCP);
    install(&f.ctx()).expect("install");

    let mcp = json_file(&f.user_mcp());
    let before = json_literal(USER_MCP);
    assert_eq!(mcp["servers"]["other"], before["servers"]["other"]);
    assert_eq!(mcp["inputs"], before["inputs"]);
    assert_eq!(
        mcp["servers"]["agora"]["env"]["AGORA_FOLDER"],
        "${workspaceFolder}"
    );
}

#[test]
fn foreign_mcp_entry_is_replaced_with_a_warning() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp(r#"{"servers":{"agora":{"type":"stdio","command":"/usr/bin/other"}}}"#);
    let report = install(&f.ctx()).expect("install");

    let mcp = action_for(&report, &f.user_mcp());
    assert_eq!(mcp.result, "wrote");
    assert!(mcp.detail.contains("was not ours"), "{}", mcp.detail);
    assert_eq!(
        json_file(&f.user_mcp())["servers"]["agora"]["command"],
        "/opt/agora/bin/agora"
    );
}

#[test]
fn second_install_is_unchanged() {
    let f = Fixture::new(Scope::User, false);
    install(&f.ctx()).expect("first install");
    let report = install(&f.ctx()).expect("second install");

    assert_eq!(report.actions.len(), 3);
    assert!(report.actions.iter().all(|a| a.result == "unchanged"));
}

#[test]
fn uninstall_restores_user_content() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp(USER_MCP);
    install(&f.ctx()).expect("install");
    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");

    assert_eq!(json_file(&f.user_mcp()), json_literal(USER_MCP));
    assert!(!f.user_hooks().exists());
    assert!(!f.user_instructions().exists());
    assert_eq!(report.actions.len(), 3);
    assert!(report.actions.iter().all(|a| a.result == "removed"));
}

#[test]
fn second_uninstall_reports_absent() {
    let f = Fixture::new(Scope::User, false);
    install(&f.ctx()).expect("install");
    uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("first uninstall");
    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("second uninstall");

    assert!(report.actions.iter().all(|a| a.result == "absent"));
}

#[test]
fn uninstall_reports_this_scopes_three_files() {
    for scope in [Scope::User, Scope::Project] {
        let f = Fixture::new(scope, false);
        install(&f.ctx()).expect("install");
        let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");

        let paths = match scope {
            Scope::User => [f.user_mcp(), f.user_hooks(), f.user_instructions()],
            Scope::Project => [f.proj_mcp(), f.proj_hooks(), f.proj_instructions()],
        };
        assert_eq!(report.caps.files, paths.map(|p| p.display().to_string()));
    }
}

#[test]
fn unparsable_mcp_config_gets_the_add_by_hand_hint() {
    let f = Fixture::new(Scope::User, false);
    let text = "// VS Code comment\n{\"servers\":{}}";
    f.write_user_mcp(text);

    let Err(err) = install(&f.ctx()) else {
        panic!("a commented mcp.json must be refused");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("VS Code mcp.json may contain comments"),
        "{msg}"
    );
    assert!(
        msg.contains("add this entry by hand under \"servers\": {"),
        "{msg}"
    );
    assert!(msg.contains("/opt/agora/bin/agora"), "{msg}");
    assert_eq!(fs::read_to_string(f.user_mcp()).expect("read"), text);
    assert!(!f.user_hooks().exists());
}

#[test]
fn symlinked_mcp_config_is_refused_and_tree_is_unchanged() {
    let f = Fixture::new(Scope::User, false);
    let root = f.home.parent().expect("root");
    let real = root.join("real-mcp.json");
    fs::write(&real, USER_MCP).expect("write real");
    fs::create_dir_all(&f.vscode).expect("vscode dir");
    std::os::unix::fs::symlink(&real, f.user_mcp()).expect("symlink");
    let before = snapshot(root);

    // Install and uninstall both refuse the symlink before any write.
    let Err(err) = install(&f.ctx()) else {
        panic!("a symlinked mcp.json must be refused");
    };
    let msg = err.to_string();
    assert!(msg.contains("refusing to read through symlink"), "{msg}");
    assert!(!msg.contains("may contain comments"), "{msg}");
    assert_eq!(snapshot(root), before);
    assert!(uninstall(&f.ctx(), &CapabilitiesFile::default()).is_err());
    assert_eq!(snapshot(root), before);
}

#[test]
fn dry_run_writes_nothing() {
    let f = Fixture::new(Scope::User, true);
    let report = install(&f.ctx()).expect("dry-run install");

    assert!(!f.user_mcp().exists());
    assert!(!f.user_hooks().exists());
    assert!(!f.user_instructions().exists());
    assert!(report.actions.iter().all(|a| a.result == "dry_run"));
}

#[test]
fn foreign_hooks_file_is_refused_and_untouched() {
    let f = Fixture::new(Scope::User, false);
    fs::create_dir_all(f.user_hooks().parent().expect("parent")).expect("hooks dir");
    fs::write(f.user_hooks(), FOREIGN_HOOKS).expect("write foreign");

    let Err(err) = install(&f.ctx()) else {
        panic!("a foreign hooks file must be refused");
    };
    assert!(err.to_string().contains("not managed by agora"), "{err}");
    assert_eq!(
        fs::read_to_string(f.user_hooks()).expect("read"),
        FOREIGN_HOOKS
    );
    assert!(!f.user_mcp().exists(), "nothing may be written");
    assert!(!f.user_instructions().exists());
}

#[test]
fn foreign_user_instructions_file_is_refused_and_untouched() {
    let f = Fixture::new(Scope::User, false);
    fs::create_dir_all(f.user_instructions().parent().expect("parent")).expect("instr dir");
    fs::write(f.user_instructions(), FOREIGN_INSTRUCTIONS).expect("write foreign");

    let Err(err) = install(&f.ctx()) else {
        panic!("a foreign instructions file must be refused");
    };
    assert!(err.to_string().contains("not managed by agora"), "{err}");
    assert_eq!(
        fs::read_to_string(f.user_instructions()).expect("read"),
        FOREIGN_INSTRUCTIONS
    );
    assert!(!f.user_mcp().exists(), "nothing may be written");
    assert!(!f.user_hooks().exists());
}

#[test]
fn malformed_mcp_config_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp("{not json");

    assert!(install(&f.ctx()).is_err());
    assert_eq!(fs::read_to_string(f.user_mcp()).expect("read"), "{not json");
    assert!(!f.user_hooks().exists());
    assert!(!f.user_instructions().exists());
}

#[test]
fn non_object_servers_and_non_array_inputs_are_refused() {
    for text in [r#"{"servers":[]}"#, r#"{"inputs":{}}"#] {
        let f = Fixture::new(Scope::User, false);
        f.write_user_mcp(text);

        let Err(err) = install(&f.ctx()) else {
            panic!("{text} must be refused");
        };
        assert!(
            err.to_string()
                .contains(&f.user_mcp().display().to_string()),
            "{err}"
        );
        assert_eq!(fs::read_to_string(f.user_mcp()).expect("read"), text);
        assert!(!f.user_hooks().exists());
    }
}

#[test]
fn hooks_file_shared_with_copilot_cli_is_kept_on_uninstall() {
    let f = Fixture::new(Scope::User, false);
    install(&f.ctx()).expect("install");
    let hooks_path = f.user_hooks().display().to_string();
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::CopilotCli,
        Some(HarnessCapabilities {
            files: vec![hooks_path],
            ..HarnessCapabilities::default()
        }),
    );
    let report = uninstall(&f.ctx(), &caps).expect("uninstall");

    let hooks = action_for(&report, &f.user_hooks());
    assert_eq!(hooks.result, "kept");
    assert_eq!(
        hooks.detail,
        format!("still used by {}", Harness::CopilotCli.as_str())
    );
    assert!(f.user_hooks().exists());
}

#[test]
fn project_scope_uses_project_paths() {
    let f = Fixture::new(Scope::Project, false);
    let report = install(&f.ctx()).expect("project install");

    assert_eq!(
        json_file(&f.proj_mcp())["servers"]["agora"]["env"]["AGORA_FOLDER"],
        "${workspaceFolder}"
    );
    assert_eq!(
        fs::read_to_string(f.proj_hooks()).expect("hooks"),
        expected_hooks_bytes(&f)
    );
    let instructions = fs::read_to_string(f.proj_instructions()).expect("instructions");
    assert_eq!(
        instructions,
        markers::upsert_block("", instructions::snippet()).expect("block")
    );
    assert!(
        !f.vscode.exists(),
        "the user VS Code dir must stay untouched"
    );
    assert!(
        !f.home.join(".copilot").exists(),
        "the user Copilot dir must stay untouched"
    );
    assert_eq!(report.caps.scopes.len(), 1);
    assert!(report.caps.scopes[0].starts_with("project:"));
}

#[test]
fn project_instructions_keep_user_text_on_uninstall() {
    let f = Fixture::new(Scope::Project, false);
    fs::create_dir_all(f.proj.join(".github")).expect("github dir");
    fs::write(f.proj_instructions(), "# mine\n").expect("write user text");
    install(&f.ctx()).expect("install");
    uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");

    assert_eq!(
        fs::read_to_string(f.proj_instructions()).expect("read"),
        "# mine\n"
    );
}

#[test]
fn project_instructions_kept_when_shared_with_copilot_cli() {
    let f = Fixture::new(Scope::Project, false);
    install(&f.ctx()).expect("install");
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::CopilotCli,
        Some(HarnessCapabilities {
            files: vec![f.proj_instructions().display().to_string()],
            ..HarnessCapabilities::default()
        }),
    );
    let report = uninstall(&f.ctx(), &caps).expect("uninstall");

    let instructions = action_for(&report, &f.proj_instructions());
    assert_eq!(instructions.result, "kept");
    assert_eq!(
        instructions.detail,
        format!("still used by {}", Harness::CopilotCli.as_str())
    );
    assert!(markers::has_block(
        &fs::read_to_string(f.proj_instructions()).expect("read")
    ));
}

#[test]
fn project_files_shared_with_copilot_cli_are_unchanged_on_vscode_install() {
    let f = Fixture::new(Scope::Project, false);
    copilot_cli::install(&f.ctx()).expect("copilot-cli install");
    let report = install(&f.ctx()).expect("vscode install");

    assert_eq!(action_for(&report, &f.proj_hooks()).result, "unchanged");
    assert_eq!(
        action_for(&report, &f.proj_instructions()).result,
        "unchanged"
    );
}
