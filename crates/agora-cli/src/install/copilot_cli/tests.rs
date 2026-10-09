use super::*;
use crate::install::claude::{Mode, Options};
use crate::install::paths::ClientPaths;
use std::collections::BTreeMap;
use std::ffi::OsString;

/// An MCP config the user already has, with one server of their own.
const USER_MCP: &str = r#"{"mcpServers":{"other":{"type":"local","command":"/usr/bin/other","args":[],"tools":["*"]}}}"#;
/// A hooks file that is not ours.
const FOREIGN_HOOKS: &str =
    r#"{"version":1,"hooks":{"sessionStart":[{"type":"command","bash":"echo mine"}]}}"#;

/// A temp dir holding `home/`, `proj/` and `copilot/` (COPILOT_HOME), with the options for one scope.
struct Fixture {
    _dir: tempfile::TempDir,
    copilot: PathBuf,
    proj: PathBuf,
    paths: ClientPaths,
    opts: Options,
}

impl Fixture {
    fn new(scope: Scope, dry_run: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let proj = root.join("proj");
        let copilot = root.join("copilot");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&proj).expect("proj");
        let copilot_env = copilot.clone();
        let env = move |key: &str| -> Option<OsString> {
            (key == "COPILOT_HOME").then(|| copilot_env.clone().into_os_string())
        };
        let paths = ClientPaths::from_env(&home, &proj, &env).expect("paths");
        let opts = Options {
            scope,
            mode: Mode::Entries,
            dry_run,
            agora_home: root.join("agora"),
            user_home: home,
            project_dir: proj.clone(),
            exe: PathBuf::from("/opt/agora/bin/agora"),
        };
        Self {
            _dir: dir,
            copilot,
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

    fn user_mcp(&self) -> PathBuf {
        self.copilot.join("mcp-config.json")
    }

    fn user_hooks(&self) -> PathBuf {
        self.copilot.join("hooks/agora.json")
    }

    fn user_instructions(&self) -> PathBuf {
        self.copilot.join("copilot-instructions.md")
    }

    /// Create the user config dir and put `text` into the user MCP file.
    fn write_user_mcp(&self, text: &str) {
        fs::create_dir_all(&self.copilot).expect("copilot dir");
        fs::write(self.user_mcp(), text).expect("write mcp");
    }
}

fn json_file(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read");
    serde_json::from_str(&text).expect("json")
}

/// One entry of a tree snapshot: a directory, a file with its bytes, or a symlink with its target.
#[derive(Debug, PartialEq, Eq)]
enum Node {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every entry under `dir`, directories included, so a test proves that nothing was created,
/// removed or rewritten. Symlinks are recorded as links, never followed.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Node> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("lstat");
            let node = if meta.file_type().is_symlink() {
                Node::Link(fs::read_link(&path).expect("read_link"))
            } else if meta.is_dir() {
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

/// The hooks document an install with `agora_home` must produce.
fn expected_hooks_doc(agora_home: &Path) -> Value {
    // Safe paths are emitted unquoted (`sh_arg`); the fixture's temp paths are all safe characters.
    let command = |kind: &str, event: &str| {
        format!(
            "/opt/agora/bin/agora --home {} hook {kind} --dialect copilot --event {event}",
            agora_home.display()
        )
    };
    let handler = |kind: &str, event: &str| json!({"type": "command", "bash": command(kind, event), "cwd": ".", "timeoutSec": 10});
    json!({"version": 1, "hooks": {
        "sessionStart": [handler("session-start", "sessionStart")],
        "userPromptSubmitted": [handler("context", "userPromptSubmitted")],
        "postToolUse": [handler("context", "postToolUse")],
        "agentStop": [handler("stop", "agentStop")]
    }})
}

#[test]
fn fresh_user_install_writes_mcp_hooks_and_instructions() {
    let f = Fixture::new(Scope::User, false);
    let report = install(&f.ctx()).expect("install");
    let agora_home = f.opts.agora_home.clone();

    assert_eq!(
        json_file(&f.user_mcp()),
        json!({"mcpServers": {"agora": {
            "type": "local",
            "command": "/opt/agora/bin/agora",
            "args": ["--home", agora_home.to_str().expect("utf8"), "mcp"],
            "tools": ["*"]
        }}})
    );
    assert_eq!(json_file(&f.user_hooks()), expected_hooks_doc(&agora_home));
    let instructions = fs::read_to_string(f.user_instructions()).expect("instructions");
    assert_eq!(
        instructions,
        markers::upsert_block("", instructions::snippet()).expect("block")
    );
    assert!(report.actions.iter().all(|a| a.result == "wrote"));
    assert!(report.caps.mcp_configured);
    assert!(!report.caps.context_hook && !report.caps.stop_hook);
    assert_eq!(report.caps.scopes, vec!["user".to_owned()]);
    assert_eq!(report.caps.files.len(), 3);
    assert_eq!(report.notes.len(), 2);
}

#[test]
fn existing_mcp_config_keeps_other_servers() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp(USER_MCP);
    install(&f.ctx()).expect("install");

    let mcp = json_file(&f.user_mcp());
    assert_eq!(
        mcp["mcpServers"]["other"],
        json_literal(USER_MCP)["mcpServers"]["other"]
    );
    assert_eq!(mcp["mcpServers"]["agora"]["tools"], json!(["*"]));
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
    let instructions = fs::read_to_string(f.user_instructions()).expect("instructions");
    assert!(!markers::has_block(&instructions));
    assert!(report.actions.iter().all(|a| a.result == "removed"));
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
fn malformed_mcp_config_is_refused_and_nothing_is_written() {
    let f = Fixture::new(Scope::User, false);
    f.write_user_mcp("{not json");

    assert!(install(&f.ctx()).is_err());
    assert_eq!(fs::read_to_string(f.user_mcp()).expect("read"), "{not json");
    assert!(!f.user_hooks().exists());
    assert!(!f.user_instructions().exists());
}

#[test]
fn hooks_file_shared_with_vscode_is_kept_on_uninstall() {
    let f = Fixture::new(Scope::User, false);
    install(&f.ctx()).expect("install");
    let hooks_path = f.user_hooks().display().to_string();
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::CopilotVsCode,
        Some(HarnessCapabilities {
            files: vec![hooks_path.clone()],
            ..HarnessCapabilities::default()
        }),
    );
    let report = uninstall(&f.ctx(), &caps).expect("uninstall");

    let hooks = report
        .actions
        .iter()
        .find(|a| a.path == hooks_path)
        .expect("hooks action");
    assert_eq!(hooks.result, "kept");
    assert_eq!(
        hooks.detail,
        format!("still used by {}", Harness::CopilotVsCode.as_str())
    );
    assert!(f.user_hooks().exists());
}

#[test]
fn project_scope_uses_github_paths() {
    let f = Fixture::new(Scope::Project, false);
    let report = install(&f.ctx()).expect("project install");
    let agora_home = f.opts.agora_home.clone();

    let mcp = f.proj.join(".github/mcp.json");
    assert_eq!(
        json_file(&mcp)["mcpServers"]["agora"]["tools"],
        json!(["*"])
    );
    assert_eq!(
        json_file(&f.proj.join(".github/hooks/agora.json")),
        expected_hooks_doc(&agora_home)
    );
    let instructions =
        fs::read_to_string(f.proj.join(".github/copilot-instructions.md")).expect("instructions");
    assert!(markers::has_block(&instructions));
    assert!(
        !f.copilot.exists(),
        "the user Copilot home must stay untouched"
    );
    assert!(report.notes[0].contains(".github/mcp.json"));
    assert!(report.notes[0].contains("1.0.61"));
}

#[test]
fn uninstall_reports_this_scopes_three_paths_even_when_nothing_is_there() {
    let f = Fixture::new(Scope::User, false);
    let report = uninstall(&f.ctx(), &CapabilitiesFile::default()).expect("uninstall");

    assert!(report.actions.iter().all(|a| a.result == "absent"));
    assert_eq!(
        report.caps.files,
        vec![
            f.user_mcp().display().to_string(),
            f.user_hooks().display().to_string(),
            f.user_instructions().display().to_string(),
        ]
    );
}

#[test]
fn symlinked_mcp_config_is_refused_and_the_tree_is_untouched() {
    let f = Fixture::new(Scope::User, false);
    let root = f.copilot.parent().expect("root").to_path_buf();
    let target = root.join("elsewhere.json");
    fs::write(&target, USER_MCP).expect("target");
    fs::create_dir_all(&f.copilot).expect("copilot dir");
    std::os::unix::fs::symlink(&target, f.user_mcp()).expect("symlink");
    let before = snapshot(&root);

    let Err(err) = install(&f.ctx()) else {
        panic!("a symlinked mcp-config.json must be refused");
    };
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(snapshot(&root), before);

    let Err(err) = uninstall(&f.ctx(), &CapabilitiesFile::default()) else {
        panic!("a symlinked mcp-config.json must be refused on uninstall");
    };
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(snapshot(&root), before);
}

/// Parse a JSON literal used as the expected value.
fn json_literal(text: &str) -> Value {
    serde_json::from_str(text).expect("json literal")
}
