use super::*;
use crate::install::claude::{Mode, Options};
use crate::install::paths::ClientPaths;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;

/// A user's own Codex config: comments, a trusted project, key order that must survive.
const USER_CONFIG: &str = "# Codex user config.\nmodel = \"gpt-5.1-codex\"  # keep\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
/// The user's own AGENTS.md text, before any agora block.
const USER_AGENTS: &str = "# My rules\n\nBe kind.\n";
/// (trust label, hook kind) of the four handlers agora writes, in `HOOK_EVENTS` order.
const TRUST_CASES: [(&str, &str); 4] = [
    ("session_start", "session-start"),
    ("user_prompt_submit", "context"),
    ("post_tool_use", "context"),
    ("stop", "stop"),
];

fn user_hooks() -> Value {
    json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/usr/local/bin/user-stop", "timeout": 5}]}]}})
}

/// A temp dir holding `codex/` (CODEX_HOME), `home/`, `proj/` and the options for one scope.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    codex: PathBuf,
    paths: ClientPaths,
    opts: Options,
}

impl Fixture {
    fn new(scope: Scope) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let proj = root.join("proj");
        let codex = root.join("codex");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&proj).expect("proj");
        let codex_env = codex.clone();
        let env = move |key: &str| -> Option<OsString> {
            (key == "CODEX_HOME").then(|| codex_env.clone().into_os_string())
        };
        let paths = ClientPaths::from_env(&home, &proj, &env).expect("paths");
        let opts = Options {
            scope,
            mode: Mode::Entries,
            dry_run: false,
            agora_home: root.join("agora"),
            user_home: home,
            project_dir: proj,
            exe: PathBuf::from("/opt/agora/bin/agora"),
        };
        Self {
            _dir: dir,
            root,
            codex,
            paths,
            opts,
        }
    }

    fn ctx(&self, trust_hooks: bool) -> Ctx<'_> {
        Ctx {
            o: &self.opts,
            paths: &self.paths,
            trust_hooks,
        }
    }

    /// A file in the Codex home (the user scope targets).
    fn path(&self, name: &str) -> PathBuf {
        self.codex.join(name)
    }

    /// Writes the user's own config, hooks and AGENTS.md (the user-scope targets).
    fn write_user_files(&self) {
        fs::create_dir_all(&self.codex).expect("codex home");
        fs::write(self.path("config.toml"), USER_CONFIG).expect("config");
        fs::write(self.path("hooks.json"), user_hooks().to_string()).expect("hooks");
        fs::write(self.path("AGENTS.md"), USER_AGENTS).expect("agents");
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

/// Every entry under `dir` with its bytes, so a test can prove nothing changed. A directory maps to
/// the marker `<dir>` (an empty directory a write would create is a change); a symlink maps to
/// `<link> <target>` and is never read through.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let kind = entry.file_type().expect("file type");
            if kind.is_symlink() {
                let target = fs::read_link(&path).expect("read_link");
                out.insert(path, format!("<link> {}", target.display()).into_bytes());
            } else if kind.is_dir() {
                out.insert(path.clone(), b"<dir>".to_vec());
                stack.push(path);
            } else {
                out.insert(path.clone(), fs::read(&path).expect("file"));
            }
        }
    }
    out
}

#[test]
fn fresh_install_writes_mcp_entry_hooks_and_instructions() {
    let f = Fixture::new(Scope::User);
    let ctx = f.ctx(false);
    let report = install(&ctx).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);

    let config: toml_edit::DocumentMut = read(&f.path("config.toml")).parse().expect("toml");
    let server = &config["mcp_servers"]["agora"];
    assert_eq!(server["command"].as_str(), Some("/opt/agora/bin/agora"));
    let home = f.opts.agora_home.display().to_string();
    let args: Vec<&str> = server["args"]
        .as_array()
        .expect("args")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(args, vec!["--home", home.as_str(), "mcp"]);

    let hooks = read_json(&f.path("hooks.json"));
    for (event, kind) in HOOK_EVENTS {
        let groups = hooks["hooks"][event].as_array().expect("event groups");
        assert_eq!(groups.len(), 1, "{event}");
        let handler = &groups[0]["hooks"][0];
        assert_eq!(handler["type"], "command");
        assert_eq!(handler["timeout"], 10);
        assert_eq!(
            handler["command"],
            json!(ctx.hook_cmd(kind, "codex", None).expect("cmd"))
        );
        assert!(groups[0].get("matcher").is_none(), "{event} has no matcher");
    }

    let agents = read(&f.path("AGENTS.md"));
    assert!(agents.contains(markers::BEGIN) && agents.contains(markers::END));

    assert!(report.caps.mcp_configured && report.caps.context_hook && report.caps.stop_hook);
    assert!(!report.caps.owned_session);
    assert_eq!(report.caps.hooks_trusted, Some(false));
    assert_eq!(report.caps.scopes, vec!["user".to_owned()]);
    assert_eq!(report.caps.files.len(), 3);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("Codex skips new hooks"))
    );
}

#[test]
fn install_keeps_user_content_and_a_second_run_changes_nothing() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx(false)).expect("first install");

    let config = read(&f.path("config.toml"));
    assert!(
        config.contains("# Codex user config.")
            && config.contains("model = \"gpt-5.1-codex\"  # keep")
    );
    assert!(config.contains("trust_level = \"trusted\""));
    let agents = read(&f.path("AGENTS.md"));
    assert!(agents.starts_with(USER_AGENTS));
    let stop = read_json(&f.path("hooks.json"))["hooks"]["Stop"].clone();
    assert_eq!(
        stop.as_array().map(Vec::len),
        Some(2),
        "user group plus ours"
    );
    assert_eq!(stop[0], user_hooks()["hooks"]["Stop"][0]);

    let again = install(&f.ctx(false)).expect("second install");
    assert_eq!(results(&again), ["unchanged", "unchanged", "unchanged"]);
}

#[test]
fn trust_hooks_writes_state_entries_hashed_for_our_commands() {
    let f = Fixture::new(Scope::User);
    let ctx = f.ctx(true);
    let report = install(&ctx).expect("install");
    assert_eq!(report.caps.hooks_trusted, Some(true));
    assert!(report.notes.iter().all(|n| !n.contains("Codex skips")));

    let canonical = fs::canonicalize(f.path("hooks.json")).expect("canonical hooks.json");
    let config: toml_edit::DocumentMut = read(&f.path("config.toml")).parse().expect("toml");
    assert_eq!(
        config["hooks"]["state"].as_table().map(|t| t.len()),
        Some(4)
    );
    for (label, kind) in TRUST_CASES {
        let command = ctx.hook_cmd(kind, "codex", None).expect("cmd");
        let key = format!("{}:{label}:0:0", canonical.display());
        let hash = config["hooks"]["state"][key.as_str()]["trusted_hash"]
            .as_str()
            .expect("trusted_hash");
        assert_eq!(
            hash,
            codex_trust::trusted_hash(label, None, &command, HOOK_TIMEOUT_S)
        );
    }
}

#[test]
fn uninstall_restores_user_content_and_removes_trust() {
    let f = Fixture::new(Scope::User);
    f.write_user_files();
    install(&f.ctx(true)).expect("install with trust");

    let report = uninstall(&f.ctx(false), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["wrote", "wrote", "wrote"]);
    assert_eq!(read(&f.path("config.toml")), USER_CONFIG);
    assert_eq!(read_json(&f.path("hooks.json")), user_hooks());
    assert_eq!(read(&f.path("AGENTS.md")), USER_AGENTS);

    let again = uninstall(&f.ctx(false), &CapabilitiesFile::default()).expect("second uninstall");
    assert_eq!(results(&again), ["unchanged", "unchanged", "unchanged"]);
}

#[test]
fn dry_run_writes_nothing_for_install_or_uninstall() {
    let mut f = Fixture::new(Scope::User);
    f.opts.dry_run = true;
    let before = snapshot(&f.root);

    let report = install(&f.ctx(true)).expect("dry-run install");
    assert!(report.actions.iter().all(|a| a.result == "dry_run"));
    assert_eq!(report.caps.hooks_trusted, Some(true));
    assert_eq!(snapshot(&f.root), before);

    let removal =
        uninstall(&f.ctx(false), &CapabilitiesFile::default()).expect("dry-run uninstall");
    assert_eq!(results(&removal), ["absent", "absent", "absent"]);
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn malformed_config_fails_before_any_write() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(&f.codex).expect("codex home");
    fs::write(f.path("config.toml"), "[mcp_servers\n").expect("config");
    let before = snapshot(&f.root);

    let err = install(&f.ctx(true)).expect_err("malformed config must fail");
    assert!(err.to_string().contains("config.toml"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn non_object_hooks_member_fails_before_any_write() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(&f.codex).expect("codex home");
    fs::write(f.path("hooks.json"), r#"{"hooks": []}"#).expect("hooks");
    let before = snapshot(&f.root);

    let err = install(&f.ctx(false)).expect_err("non-object hooks must fail");
    assert!(err.to_string().contains("hooks.json"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn project_scope_writes_under_the_project_and_notes_trust() {
    let f = Fixture::new(Scope::Project);
    let proj = f.opts.project_dir.clone();
    let report = install(&f.ctx(false)).expect("project install");

    assert!(proj.join(".codex/config.toml").is_file());
    assert!(proj.join(".codex/hooks.json").is_file());
    assert!(proj.join("AGENTS.md").is_file());
    assert!(
        !f.path("config.toml").exists(),
        "no user config write without trust"
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("trust_level = \"trusted\""))
    );
    assert_eq!(
        report.caps.scopes,
        vec![format!("project:{}", proj.display())]
    );
    assert_eq!(report.caps.hooks_trusted, Some(false));
}

#[test]
fn uninstall_keeps_agents_block_still_listed_by_another_client() {
    let f = Fixture::new(Scope::User);
    install(&f.ctx(false)).expect("install");
    let agents = f.path("AGENTS.md").display().to_string();
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::CopilotCli,
        Some(HarnessCapabilities {
            files: vec![agents.clone()],
            ..Default::default()
        }),
    );

    let report = uninstall(&f.ctx(false), &caps).expect("uninstall");
    let instructions = report
        .actions
        .iter()
        .find(|a| a.path == agents)
        .expect("instructions action");
    assert_eq!(instructions.result, "kept");
    assert_eq!(
        instructions.detail,
        format!("still used by {}", Harness::CopilotCli.as_str())
    );
    assert!(read(&f.path("AGENTS.md")).contains(markers::BEGIN));
}

#[test]
fn uninstall_reports_absent_when_nothing_was_installed() {
    let f = Fixture::new(Scope::User);
    let report = uninstall(&f.ctx(false), &CapabilitiesFile::default()).expect("uninstall");
    assert_eq!(results(&report), ["absent", "absent", "absent"]);
    assert!(!f.codex.exists());
}

#[test]
fn non_array_event_fails_before_any_write() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(&f.codex).expect("codex home");
    fs::write(f.path("hooks.json"), r#"{"hooks":{"Stop":{}}}"#).expect("hooks");
    let before = snapshot(&f.root);

    let err = install(&f.ctx(true)).expect_err("non-array event must fail");
    assert!(err.to_string().contains("Stop"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}

#[test]
fn trust_key_follows_the_user_stop_groups() {
    let f = Fixture::new(Scope::User);
    fs::create_dir_all(&f.codex).expect("codex home");
    let user_group =
        |cmd: &str| json!({"hooks": [{"type": "command", "command": cmd, "timeout": 5}]});
    let doc = json!({"hooks": {"Stop": [user_group("/usr/local/bin/one"), user_group("/usr/local/bin/two")]}});
    fs::write(f.path("hooks.json"), doc.to_string()).expect("hooks");

    let ctx = f.ctx(true);
    install(&ctx).expect("install with trust");

    // Our Stop group is third (index 2) and its handler is the first (index 0).
    let canonical = fs::canonicalize(f.path("hooks.json")).expect("canonical hooks.json");
    let key = format!("{}:stop:2:0", canonical.display());
    let command = ctx.hook_cmd("stop", "codex", None).expect("cmd");
    let expected = codex_trust::trusted_hash("stop", None, &command, HOOK_TIMEOUT_S);
    let config: toml_edit::DocumentMut = read(&f.path("config.toml")).parse().expect("toml");
    assert_eq!(
        config["hooks"]["state"][key.as_str()]["trusted_hash"].as_str(),
        Some(expected.as_str())
    );
    assert_eq!(
        config["hooks"]["state"].as_table().map(|t| t.len()),
        Some(4),
        "only our four handlers are trusted, not the user's groups"
    );
}

#[test]
fn project_trust_is_written_to_the_user_config_and_removed_by_uninstall() {
    let f = Fixture::new(Scope::Project);
    install(&f.ctx(true)).expect("project install with trust");

    let user_cfg = f.path("config.toml");
    let project_cfg = f.opts.project_dir.join(".codex/config.toml");
    assert!(
        read(&user_cfg).contains("trusted_hash"),
        "trust in the user config"
    );
    assert!(
        !read(&project_cfg).contains("trusted_hash"),
        "no trust in the project"
    );

    let report = uninstall(&f.ctx(false), &CapabilitiesFile::default()).expect("uninstall");
    let user_path = user_cfg.display().to_string();
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.path == user_path && a.result == "wrote"),
        "the user config is edited"
    );
    let left: toml_edit::DocumentMut = read(&user_cfg).parse().expect("toml");
    assert!(left.get("hooks").is_none(), "agora trust removed");
}

#[cfg(unix)]
#[test]
fn symlinked_config_is_refused_and_nothing_changes() {
    let f = Fixture::new(Scope::User);
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");
    fs::write(elsewhere.join("config.toml"), USER_CONFIG).expect("real config");
    fs::create_dir_all(&f.codex).expect("codex home");
    std::os::unix::fs::symlink(elsewhere.join("config.toml"), f.path("config.toml"))
        .expect("symlink");
    let before = snapshot(&f.root);

    let err = install(&f.ctx(true)).expect_err("symlinked config must be refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}

#[cfg(unix)]
#[test]
fn symlinked_hooks_file_is_refused_and_nothing_changes() {
    let f = Fixture::new(Scope::User);
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");
    fs::write(elsewhere.join("hooks.json"), user_hooks().to_string()).expect("real hooks");
    fs::create_dir_all(&f.codex).expect("codex home");
    std::os::unix::fs::symlink(elsewhere.join("hooks.json"), f.path("hooks.json"))
        .expect("symlink");
    let before = snapshot(&f.root);

    let err = install(&f.ctx(false)).expect_err("symlinked hooks must be refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(snapshot(&f.root), before);
}
