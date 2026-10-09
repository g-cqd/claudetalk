use super::*;
use crate::install::claude::{Mode, Scope};
use crate::install::markers;
use agora_deliver::Harness;
use std::path::{Path, PathBuf};

/// A temp dir holding the user home, the project and (absent until an install) the agora home.
struct Fixture {
    dir: tempfile::TempDir,
    o: Options,
    paths: ClientPaths,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let user_home = dir.path().join("home");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&user_home).expect("home");
    std::fs::create_dir_all(&project).expect("project");
    let o = Options {
        scope: Scope::User,
        mode: Mode::Plugin,
        dry_run: false,
        agora_home: dir.path().join("agora"),
        user_home: user_home.clone(),
        project_dir: project.clone(),
        exe: PathBuf::from("/usr/local/bin/agora"),
    };
    let paths = ClientPaths::from_env(&user_home, &project, &|_| None).expect("paths");
    Fixture { dir, o, paths }
}

fn load(fx: &Fixture) -> CapabilitiesFile {
    capabilities::load(&Paths::new(&fx.o.agora_home)).expect("capabilities.json loads")
}

/// The output entry of `client`.
fn item<'a>(out: &'a Out, client: &str) -> &'a Value {
    out.json["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .find(|i| i["client"] == client)
        .expect("an entry for the client")
}

/// Every regular file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("list") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The Copilot hooks file both Copilot clients share (user scope, COPILOT_HOME unset).
fn shared_hooks(fx: &Fixture) -> PathBuf {
    fx.paths
        .copilot_cli(Scope::User)
        .hooks
        .expect("copilot-cli has a hooks file")
}

#[test]
fn codex_and_cursor_install_record_one_entry_each() {
    let fx = fixture();
    let out = install_with(
        &fx.o,
        &[Client::Codex, Client::Cursor],
        false,
        false,
        &fx.paths,
    )
    .expect("install");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    assert!(item(&out, "codex")["error"].is_null());
    assert!(item(&out, "cursor")["error"].is_null());
    let file = load(&fx);
    for h in [Harness::Codex, Harness::Cursor] {
        let caps = file.harness(h).expect("an entry for the client");
        assert_eq!(caps.scopes, vec!["user"]);
        assert!(!caps.files.is_empty());
    }
}

#[test]
fn a_failing_client_is_reported_and_the_others_still_install() {
    let fx = fixture();
    let mcp = fx.paths.cursor(Scope::User).mcp;
    std::fs::create_dir_all(mcp.parent().expect("parent")).expect("cursor dir");
    std::fs::write(&mcp, "{not json").expect("malformed mcp.json");

    let out = install_with(
        &fx.o,
        &[Client::Cursor, Client::Codex],
        false,
        false,
        &fx.paths,
    )
    .expect("install returns Ok with a failed client");
    assert_eq!(out.json["ok"], false);
    assert!(!item(&out, "cursor")["error"].is_null());
    assert!(item(&out, "codex")["error"].is_null());
    assert!(out.text.contains("error: "), "{}", out.text);

    let file = load(&fx);
    assert!(file.harness(Harness::Cursor).is_none());
    assert!(file.harness(Harness::Codex).is_some());
}

#[test]
fn uninstall_removes_the_entries_of_the_listed_clients() {
    let fx = fixture();
    install_with(
        &fx.o,
        &[Client::Codex, Client::Cursor],
        false,
        false,
        &fx.paths,
    )
    .expect("install");
    let out =
        uninstall_with(&fx.o, &[Client::Codex, Client::Cursor], &fx.paths).expect("uninstall");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    let file = load(&fx);
    assert!(file.harness(Harness::Codex).is_none());
    assert!(file.harness(Harness::Cursor).is_none());
}

#[test]
fn uninstalling_one_copilot_client_keeps_the_hooks_file_the_other_still_uses() {
    let fx = fixture();
    let hooks = shared_hooks(&fx);
    install_with(
        &fx.o,
        &[Client::CopilotCli, Client::CopilotVsCode],
        false,
        false,
        &fx.paths,
    )
    .expect("install both");
    assert!(hooks.is_file());

    uninstall_with(&fx.o, &[Client::CopilotCli], &fx.paths).expect("uninstall copilot-cli");
    assert!(hooks.is_file(), "copilot-vscode still uses the hooks file");
    let file = load(&fx);
    assert!(file.harness(Harness::CopilotCli).is_none());
    assert!(file.harness(Harness::CopilotVsCode).is_some());

    uninstall_with(&fx.o, &[Client::CopilotVsCode], &fx.paths).expect("uninstall copilot-vscode");
    assert!(!hooks.exists(), "the last user removes the shared file");
}

#[test]
fn uninstalling_both_copilot_clients_in_one_call_removes_the_shared_hooks_file() {
    let fx = fixture();
    let hooks = shared_hooks(&fx);
    install_with(
        &fx.o,
        &[Client::CopilotCli, Client::CopilotVsCode],
        false,
        false,
        &fx.paths,
    )
    .expect("install both");

    let out = uninstall_with(
        &fx.o,
        &[Client::CopilotCli, Client::CopilotVsCode],
        &fx.paths,
    )
    .expect("uninstall both");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    assert!(!hooks.exists());
}

#[test]
fn a_dry_run_writes_no_file_and_no_capabilities() {
    let mut fx = fixture();
    fx.o.dry_run = true;
    let out = install_with(
        &fx.o,
        &[Client::Codex, Client::Cursor],
        false,
        false,
        &fx.paths,
    )
    .expect("dry run");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    assert_eq!(item(&out, "codex")["dry_run"], true);
    assert!(!fx.o.agora_home.exists());
    assert!(files_under(fx.dir.path()).is_empty());
}

#[test]
fn an_unparsable_capabilities_file_is_moved_aside_before_the_entry_is_recorded() {
    let fx = fixture();
    std::fs::create_dir_all(&fx.o.agora_home).expect("home");
    let target = capabilities::path(&Paths::new(&fx.o.agora_home));
    std::fs::write(&target, "not json").expect("seed");

    let out = install_with(&fx.o, &[Client::Cursor], false, false, &fx.paths).expect("install");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    let results: Vec<&str> = out.json["capabilities_actions"]
        .as_array()
        .expect("actions")
        .iter()
        .filter_map(|a| a["result"].as_str())
        .collect();
    assert_eq!(results, ["backed_up", "wrote"]);
    let kept_copy = files_under(&fx.o.agora_home)
        .iter()
        .any(|p| std::fs::read_to_string(p).ok().as_deref() == Some("not json"));
    assert!(kept_copy, "the unparsable file is kept as a backup");
    assert!(load(&fx).harness(Harness::Cursor).is_some());
}

#[test]
fn a_copilot_probe_sets_the_hook_flags_from_its_version() {
    let mut caps = HarnessCapabilities::default();
    let old = ClientProbe {
        bin: Some("/opt/copilot".to_owned()),
        version: Some("0.0.369".to_owned()),
        owned_session: true,
        notes: Vec::new(),
    };
    apply_probe(Client::CopilotCli, &mut caps, &old, 42);
    assert!(!caps.context_hook && !caps.stop_hook);
    assert_eq!(caps.bin.as_deref(), Some("/opt/copilot"));
    assert_eq!(caps.probed_at_ms, 42);

    let new = ClientProbe {
        version: Some("1.0.72".to_owned()),
        owned_session: false,
        ..old
    };
    apply_probe(Client::CopilotCli, &mut caps, &new, 43);
    assert!(caps.context_hook && caps.stop_hook);
}

#[test]
fn a_copilot_owned_session_is_the_probe_detection_whatever_the_version() {
    let mut caps = HarnessCapabilities::default();
    let acp_old = ClientProbe {
        bin: Some("/opt/copilot".to_owned()),
        version: Some("0.0.397".to_owned()),
        owned_session: true,
        notes: Vec::new(),
    };
    apply_probe(Client::CopilotCli, &mut caps, &acp_old, 1);
    assert!(
        caps.owned_session,
        "--acp detected: owned, though hooks are off"
    );
    assert!(!caps.context_hook && !caps.stop_hook);

    let no_acp_new = ClientProbe {
        version: Some("1.0.72".to_owned()),
        owned_session: false,
        ..acp_old
    };
    apply_probe(Client::CopilotCli, &mut caps, &no_acp_new, 2);
    assert!(
        !caps.owned_session,
        "no --acp in help: not owned, though hooks are on"
    );
    assert!(caps.context_hook && caps.stop_hook);
}

/// A finished Codex run that reports `hooks_trusted` as given, for `record_entry`.
fn codex_done(scope: &str, trusted: Option<bool>) -> Done {
    Done {
        client: Client::Codex,
        scope: scope.to_owned(),
        report: ClientReport {
            caps: HarnessCapabilities {
                hooks_trusted: trusted,
                ..HarnessCapabilities::default()
            },
            ..ClientReport::default()
        },
        probed: false,
    }
}

#[test]
fn a_later_untrusted_install_never_downgrades_recorded_trust() {
    let mut file = CapabilitiesFile::default();
    record_entry(&mut file, &codex_done("user", Some(false)));
    assert_eq!(
        load_codex_trust(&file),
        Some(false),
        "untrusted is recorded"
    );

    record_entry(&mut file, &codex_done("user", Some(true)));
    assert_eq!(load_codex_trust(&file), Some(true));

    record_entry(&mut file, &codex_done("project:/p", Some(false)));
    assert_eq!(
        load_codex_trust(&file),
        Some(true),
        "Some(false) keeps Some(true)"
    );
    let entry = file.harness(Harness::Codex).expect("codex entry");
    assert_eq!(
        entry.scopes,
        vec!["project:/p", "user"],
        "both scopes are recorded"
    );
}

fn load_codex_trust(file: &CapabilitiesFile) -> Option<bool> {
    file.harness(Harness::Codex).and_then(|c| c.hooks_trusted)
}

#[test]
fn uninstalling_a_scope_drops_its_files_so_a_shared_block_goes_with_its_last_user() {
    let mut fx = fixture();
    let agents = fx.paths.project_dir.join("AGENTS.md");
    let agents_shown = agents.display().to_string();
    fx.o.scope = Scope::User;
    install_with(&fx.o, &[Client::OpenCode], false, false, &fx.paths).expect("opencode user");
    fx.o.scope = Scope::Project;
    install_with(&fx.o, &[Client::OpenCode], false, false, &fx.paths).expect("opencode project");
    install_with(&fx.o, &[Client::Codex], false, false, &fx.paths).expect("codex project");
    let before = load(&fx)
        .harness(Harness::OpenCode)
        .expect("opencode entry");
    assert!(before.files.contains(&agents_shown));

    let out = uninstall_with(&fx.o, &[Client::OpenCode], &fx.paths).expect("uninstall opencode");
    assert_eq!(out.json["ok"], true, "{}", out.text);
    let after = load(&fx)
        .harness(Harness::OpenCode)
        .expect("user scope keeps the entry");
    assert_eq!(after.scopes, vec!["user"]);
    assert!(
        !after.files.contains(&agents_shown),
        "the entry no longer lists the project file"
    );
    let text = std::fs::read_to_string(&agents).unwrap_or_default();
    assert!(markers::has_block(&text), "codex still uses the block");

    uninstall_with(&fx.o, &[Client::Codex], &fx.paths).expect("uninstall codex");
    let text = std::fs::read_to_string(&agents).unwrap_or_default();
    assert!(
        !markers::has_block(&text),
        "the last user removes the block"
    );
}

#[test]
fn a_project_install_notes_that_its_files_hold_this_machines_paths() {
    let mut fx = fixture();
    fx.o.scope = Scope::Project;
    let out = install_with(&fx.o, &[Client::Codex], false, false, &fx.paths).expect("install");
    let notes = item(&out, "codex")["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|n| n.as_str() == Some(PROJECT_NOTE)),
        "{notes:?}"
    );

    fx.o.scope = Scope::User;
    let out = install_with(&fx.o, &[Client::Cursor], false, false, &fx.paths).expect("install");
    let notes = item(&out, "cursor")["notes"].as_array().expect("notes");
    assert!(!notes.iter().any(|n| n.as_str() == Some(PROJECT_NOTE)));
}

#[test]
fn a_plain_path_is_passed_to_the_shell_unquoted() {
    assert_eq!(sh_arg("/usr/local/bin/agora"), "/usr/local/bin/agora");
    assert_eq!(
        sh_arg("/Users/me/.agora-home_1+x@y:z%=,"),
        "/Users/me/.agora-home_1+x@y:z%=,"
    );
}

#[test]
fn a_path_with_a_space_or_empty_is_quoted() {
    assert_eq!(sh_arg("/opt/agora bin/agora"), "'/opt/agora bin/agora'");
    assert_eq!(sh_arg(""), "''");
}

#[test]
fn a_single_quote_in_a_path_is_escaped_inside_the_quotes() {
    assert_eq!(sh_arg("/tmp/x'y"), r"'/tmp/x'\''y'");
}

#[test]
fn hook_command_quotes_only_the_path_that_needs_it() {
    let mut fx = fixture();
    fx.o.agora_home = PathBuf::from("/home/x/.agora");
    let c = Ctx {
        o: &fx.o,
        paths: &fx.paths,
        trust_hooks: false,
    };
    assert_eq!(
        c.hook_cmd("stop", "codex", None).expect("cmd"),
        "/usr/local/bin/agora --home /home/x/.agora hook stop --dialect codex"
    );
    fx.o.agora_home = PathBuf::from("/home/x/My Agora");
    let c = Ctx {
        o: &fx.o,
        paths: &fx.paths,
        trust_hooks: false,
    };
    assert_eq!(
        c.hook_cmd("context", "agy", Some("PreInvocation"))
            .expect("cmd"),
        "/usr/local/bin/agora --home '/home/x/My Agora' hook context --dialect agy --event PreInvocation"
    );
}
