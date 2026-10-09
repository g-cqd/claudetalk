use super::*;
use crate::install::claude::{Mode, Options, Scope};
use crate::install::markers::has_block;
use crate::install::paths::ClientPaths;
use std::collections::BTreeMap;

const EXE: &str = "/usr/local/bin/agora";

/// A temp tree: `home` (the user's home), `xdg` (`XDG_CONFIG_HOME`), `proj` (the project dir).
struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    proj: PathBuf,
    xdg: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, proj, xdg) = (
            dir.path().join("home"),
            dir.path().join("proj"),
            dir.path().join("xdg"),
        );
        for p in [&home, &proj, &xdg] {
            fs::create_dir_all(p).expect("mkdir");
        }
        Fixture {
            _dir: dir,
            home,
            proj,
            xdg,
        }
    }

    fn paths(&self) -> ClientPaths {
        let xdg = self.xdg.clone().into_os_string();
        ClientPaths::from_env(&self.home, &self.proj, &|key: &str| {
            (key == "XDG_CONFIG_HOME").then(|| xdg.clone())
        })
        .expect("paths")
    }

    fn opts(&self, scope: Scope, dry_run: bool) -> Options {
        Options {
            scope,
            mode: Mode::Entries,
            dry_run,
            agora_home: self.home.join(".agora"),
            user_home: self.home.clone(),
            project_dir: self.proj.clone(),
            exe: PathBuf::from(EXE),
        }
    }

    fn user_cfg(&self) -> PathBuf {
        self.xdg.join("opencode")
    }

    fn user_mcp(&self) -> PathBuf {
        self.user_cfg().join("opencode.json")
    }

    fn user_agents(&self) -> PathBuf {
        self.user_cfg().join("AGENTS.md")
    }

    /// The `mcp.agora` entry this fixture's exe and home must produce.
    fn expected_entry(&self) -> Value {
        let home = self.home.join(".agora").display().to_string();
        json!({"type": "local", "command": [EXE, "--home", home, "mcp"], "enabled": true})
    }
}

fn run_install(f: &Fixture, scope: Scope, dry_run: bool) -> Result<ClientReport, CliError> {
    let paths = f.paths();
    let o = f.opts(scope, dry_run);
    install(&Ctx {
        o: &o,
        paths: &paths,
        trust_hooks: false,
    })
}

fn run_uninstall(
    f: &Fixture,
    scope: Scope,
    caps: &CapabilitiesFile,
) -> Result<ClientReport, CliError> {
    let paths = f.paths();
    let o = f.opts(scope, false);
    uninstall(
        &Ctx {
            o: &o,
            paths: &paths,
            trust_hooks: false,
        },
        caps,
    )
}

fn results(report: &ClientReport) -> Vec<&str> {
    report.actions.iter().map(|a| a.result.as_str()).collect()
}

fn read_value(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read json");
    serde_json::from_str(&text).expect("parse json")
}

fn write_file(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

/// One entry of a tree snapshot: a directory, a file with its bytes, or a symlink with its target.
#[derive(Debug, PartialEq)]
enum Node {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every entry under `dir`, directories included, so a test can prove that nothing was created,
/// removed or rewritten. Symlinks are recorded as links, never followed.
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
                Node::File(fs::read(&path).expect("read"))
            };
            out.insert(path, node);
        }
    }
    out
}

#[test]
fn fresh_user_install_writes_exact_mcp_entry_and_schema() {
    let f = Fixture::new();
    let report = run_install(&f, Scope::User, false).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    assert_eq!(
        read_value(&f.user_mcp()),
        json!({"$schema": SCHEMA, "mcp": {"agora": f.expected_entry()}})
    );
    let agents = fs::read_to_string(f.user_agents()).expect("AGENTS.md");
    assert!(has_block(&agents), "{agents}");
    assert!(report.notes[0].contains("no command hooks"));
    assert_eq!(report.caps.scopes, ["user"]);
    assert!(report.caps.mcp_configured);
    assert!(!report.caps.context_hook && !report.caps.stop_hook);
}

#[test]
fn existing_config_keeps_other_servers_and_keys() {
    let f = Fixture::new();
    let other = json!({"type": "remote", "url": "https://x.example/mcp"});
    write_file(
        &f.user_mcp(),
        &json!({"model": "anthropic/claude", "mcp": {"other": other}}).to_string(),
    );
    let report = run_install(&f, Scope::User, false).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    let doc = read_value(&f.user_mcp());
    assert_eq!(doc["model"], "anthropic/claude");
    assert_eq!(doc["mcp"]["other"], other);
    assert_eq!(doc["mcp"]["agora"], f.expected_entry());
    assert!(doc.get("$schema").is_none(), "{doc}");
}

#[test]
fn second_install_is_unchanged() {
    let f = Fixture::new();
    run_install(&f, Scope::User, false).expect("first install");
    let report = run_install(&f, Scope::User, false).expect("second install");
    assert_eq!(results(&report), ["unchanged", "unchanged"]);
}

#[test]
fn uninstall_restores_user_content() {
    let f = Fixture::new();
    let original = json!({"model": "m", "mcp": {"other": {"type": "remote"}}});
    write_file(&f.user_mcp(), &original.to_string());
    write_file(&f.user_agents(), "# Mine\n");
    run_install(&f, Scope::User, false).expect("install");
    let report = run_uninstall(&f, Scope::User, &CapabilitiesFile::default()).expect("remove");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    assert_eq!(read_value(&f.user_mcp()), original);
    assert_eq!(
        fs::read_to_string(f.user_agents()).expect("read"),
        "# Mine\n"
    );
}

#[test]
fn uninstall_of_fresh_install_keeps_schema_and_empty_agents_file() {
    let f = Fixture::new();
    run_install(&f, Scope::User, false).expect("install");
    let report = run_uninstall(&f, Scope::User, &CapabilitiesFile::default()).expect("remove");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    assert_eq!(read_value(&f.user_mcp()), json!({"$schema": SCHEMA}));
    assert_eq!(fs::read_to_string(f.user_agents()).expect("read"), "");

    let again = run_uninstall(&f, Scope::User, &CapabilitiesFile::default()).expect("again");
    assert_eq!(results(&again), ["absent", "absent"]);
}

#[test]
fn dry_run_writes_nothing() {
    let f = Fixture::new();
    let report = run_install(&f, Scope::User, true).expect("dry run");
    assert_eq!(results(&report), ["dry_run", "dry_run"]);
    assert!(!f.user_cfg().exists());
}

#[test]
fn jsonc_only_is_refused_and_untouched() {
    let f = Fixture::new();
    let jsonc = f.user_cfg().join("opencode.jsonc");
    write_file(&jsonc, "// mine\n{}\n");
    let err = run_install(&f, Scope::User, false).expect_err("refused");
    let msg = err.to_string();
    assert!(
        msg.contains("opencode.jsonc is not edited automatically"),
        "{msg}"
    );
    assert!(msg.contains(&f.expected_entry().to_string()), "{msg}");
    assert_eq!(fs::read_to_string(&jsonc).expect("read"), "// mine\n{}\n");
    assert!(!f.user_mcp().exists());
    assert!(!f.user_agents().exists());
}

#[test]
fn commented_json_is_refused_with_manual_hint() {
    let f = Fixture::new();
    write_file(&f.user_mcp(), "// comment\n{}\n");
    let err = run_install(&f, Scope::User, false).expect_err("refused");
    let msg = err.to_string();
    assert!(
        msg.contains("add this entry by hand under \"mcp\""),
        "{msg}"
    );
    assert!(msg.contains(&f.expected_entry().to_string()), "{msg}");
    assert_eq!(
        fs::read_to_string(f.user_mcp()).expect("read"),
        "// comment\n{}\n"
    );
    assert!(!f.user_agents().exists());
}

#[test]
fn non_object_mcp_is_refused_before_any_write() {
    let f = Fixture::new();
    write_file(&f.user_mcp(), r#"{"mcp": "nope"}"#);
    let err = run_install(&f, Scope::User, false).expect_err("refused");
    assert!(
        err.to_string().contains("\"mcp\" is not a JSON object"),
        "{err}"
    );
    assert_eq!(read_value(&f.user_mcp()), json!({"mcp": "nope"}));
    assert!(!f.user_agents().exists());
}

#[test]
fn project_scope_writes_next_to_the_project() {
    let f = Fixture::new();
    let report = run_install(&f, Scope::Project, false).expect("install");
    assert_eq!(results(&report), ["wrote", "wrote"]);
    let doc = read_value(&f.proj.join("opencode.json"));
    assert_eq!(doc["mcp"]["agora"], f.expected_entry());
    let agents = fs::read_to_string(f.proj.join("AGENTS.md")).expect("AGENTS.md");
    assert!(has_block(&agents));
    assert!(!f.user_mcp().exists());
    assert_eq!(
        report.caps.scopes,
        [format!("project:{}", f.proj.display())]
    );
}

#[test]
fn agents_block_is_kept_when_another_client_lists_it() {
    let f = Fixture::new();
    run_install(&f, Scope::User, false).expect("install");
    let agents = f.user_agents().display().to_string();
    let mut caps = CapabilitiesFile::default();
    caps.set_harness(
        Harness::Codex,
        Some(HarnessCapabilities {
            files: vec![agents],
            ..Default::default()
        }),
    );
    let report = run_uninstall(&f, Scope::User, &caps).expect("remove");
    assert_eq!(results(&report), ["wrote", "kept"]);
    assert!(
        report.actions[1].detail.contains("codex"),
        "{:?}",
        report.actions[1]
    );
    assert!(has_block(
        &fs::read_to_string(f.user_agents()).expect("read")
    ));
    assert!(read_value(&f.user_mcp())["mcp"].get("agora").is_none());
}

#[test]
fn uninstall_reports_this_scopes_mcp_and_agents_files_in_caps() {
    let f = Fixture::new();
    run_install(&f, Scope::Project, false).expect("install");
    let report = run_uninstall(&f, Scope::Project, &CapabilitiesFile::default()).expect("remove");
    assert_eq!(
        report.caps.files,
        [
            f.proj.join("opencode.json").display().to_string(),
            f.proj.join("AGENTS.md").display().to_string(),
        ]
    );
}

#[test]
fn symlinked_opencode_json_is_refused_and_tree_is_unchanged() {
    let f = Fixture::new();
    let real = f.xdg.join("real.json");
    fs::write(&real, "{}\n").expect("write");
    fs::create_dir_all(f.user_cfg()).expect("mkdir");
    std::os::unix::fs::symlink(&real, f.user_mcp()).expect("symlink");
    let before = snapshot(f._dir.path());

    let msg = run_install(&f, Scope::User, false)
        .expect_err("install refuses")
        .to_string();
    assert!(msg.contains("refusing to read through symlink"), "{msg}");
    let msg = run_uninstall(&f, Scope::User, &CapabilitiesFile::default())
        .expect_err("uninstall refuses")
        .to_string();
    assert!(msg.contains("refusing to read through symlink"), "{msg}");

    assert_eq!(snapshot(f._dir.path()), before);
}
