use super::*;

fn args(scope: ScopeArg, mode: ModeArg, dry_run: bool) -> InstallArgs {
    InstallArgs {
        client: CLIENT.to_owned(),
        scope,
        mode,
        probe: false,
        dry_run,
        trust_hooks: false,
    }
}

fn opts_at(agora_home: PathBuf, mode: Mode) -> Options {
    Options {
        scope: Scope::User,
        mode,
        dry_run: false,
        agora_home,
        user_home: PathBuf::from("/home/u"),
        project_dir: PathBuf::from("/work/proj"),
        exe: PathBuf::from("/usr/local/bin/agora"),
    }
}

fn opts(mode: Mode) -> Options {
    opts_at(PathBuf::from("/home/u/.agora"), mode)
}

fn action(path: &str, result: &str, detail: &str) -> Action {
    Action {
        path: path.to_owned(),
        result: result.to_owned(),
        detail: detail.to_owned(),
    }
}

fn sample_caps() -> ClaudeCapabilities {
    ClaudeCapabilities {
        claude_version: Some("2.1.293".to_owned()),
        mods: true,
        async_rewake: true,
        ..ClaudeCapabilities::default()
    }
}

#[test]
fn unknown_client_is_a_usage_error_for_install_and_uninstall() {
    for client in ["bogus", "Claude", ""] {
        let mut a = args(ScopeArg::User, ModeArg::Plugin, false);
        a.client = client.to_owned();
        let Err(err) = install(PathBuf::from("/nonexistent/agora"), a) else {
            panic!("install must reject client {client:?}");
        };
        assert!(
            matches!(err, CliError::Usage(ref m) if m.contains("valid:")),
            "{err}"
        );

        let mut a = args(ScopeArg::User, ModeArg::Plugin, false);
        a.client = client.to_owned();
        let Err(err) = uninstall(PathBuf::from("/nonexistent/agora"), a) else {
            panic!("uninstall must reject client {client:?}");
        };
        assert!(matches!(err, CliError::Usage(_)), "{err}");
    }
}

#[test]
fn scope_and_mode_map_to_options() {
    let o = options_from(
        PathBuf::from("/abs/home"),
        &args(ScopeArg::Project, ModeArg::Entries, true),
        Some(OsString::from("/h/u")),
        PathBuf::from("/proj"),
        PathBuf::from("/bin/agora"),
    )
    .expect("options");
    assert_eq!(o.scope, Scope::Project);
    assert_eq!(o.mode, Mode::Entries);
    assert!(o.dry_run);
    assert_eq!(o.agora_home, PathBuf::from("/abs/home"));
    assert_eq!(o.user_home, PathBuf::from("/h/u"));
    assert_eq!(o.project_dir, PathBuf::from("/proj"));
    assert_eq!(o.exe, PathBuf::from("/bin/agora"));

    let d = options_from(
        PathBuf::from("/abs/home"),
        &args(ScopeArg::User, ModeArg::Plugin, false),
        Some(OsString::from("/h/u")),
        PathBuf::from("/proj"),
        PathBuf::from("/bin/agora"),
    )
    .expect("options");
    assert_eq!(d.scope, Scope::User);
    assert_eq!(d.mode, Mode::Plugin);
    assert!(!d.dry_run);
}

#[test]
fn relative_home_is_made_absolute_against_the_current_dir() {
    let o = options_from(
        PathBuf::from("rel/.agora"),
        &args(ScopeArg::User, ModeArg::Plugin, false),
        Some(OsString::from("/h/u")),
        PathBuf::from("/proj"),
        PathBuf::from("/bin/agora"),
    )
    .expect("options");
    assert!(o.agora_home.is_absolute());
    assert_eq!(
        o.agora_home,
        std::env::current_dir().expect("cwd").join("rel/.agora")
    );
}

#[test]
fn missing_or_empty_home_env_is_a_usage_error() {
    for user_home in [None, Some(OsString::new())] {
        let Err(err) = options_from(
            PathBuf::from("/abs/home"),
            &args(ScopeArg::User, ModeArg::Plugin, false),
            user_home,
            PathBuf::from("/proj"),
            PathBuf::from("/bin/agora"),
        ) else {
            panic!("HOME must be required");
        };
        assert!(
            matches!(err, CliError::Usage(ref m) if m == "HOME is not set"),
            "{err}"
        );
    }
}

#[test]
fn plugin_text_lists_actions_details_and_hints() {
    let actions = vec![action(
        "/a/settings.json",
        "wrote",
        "enabledPlugins.agora@agora = true\nextraKnownMarketplaces.agora.path = /m",
    )];
    let text = install_text(&opts(Mode::Plugin), &actions, None);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "wrote /a/settings.json");
    assert_eq!(lines[1], "  enabledPlugins.agora@agora = true");
    assert_eq!(lines[2], "  extraKnownMarketplaces.agora.path = /m");
    assert!(text.contains(
            "If Claude Code does not list the plugin, run: claude plugin marketplace add /home/u/.agora/claude/marketplace && claude plugin install agora@agora"
        ));
    assert!(lines.contains(&CHANNEL_HINT));
    assert!(!text.contains("claude version"), "no probe, no probe lines");
}

#[test]
fn entries_text_has_no_plugin_hints() {
    let text = install_text(&opts(Mode::Entries), &[], None);
    assert!(!text.contains("marketplace add"));
    assert!(!text.contains("channel push"));
}

#[test]
fn probe_summary_notes_channels_hidden_from_help() {
    let p = Probe {
        claude_bin: Some("/opt/claude".to_owned()),
        version: Some("2.1.293 (Claude Code)".to_owned()),
        version_parsed: Some((2, 1, 293)),
        mods_supported: true,
        channels_flag: true,
        help_lists_channels: false,
        negotiation_env: Some("legacy".to_owned()),
    };
    let text = install_text(&opts(Mode::Entries), &[], Some(&p));
    assert!(text.contains("claude: /opt/claude"));
    assert!(text.contains("claude version: 2.1.293 (Claude Code)"));
    assert!(text.contains("mods supported: yes"));
    assert!(text.contains("channels flag: yes (hidden from --help)"));
    assert!(text.contains("MCP_PROTOCOL_NEGOTIATION: legacy"));
}

#[test]
fn install_json_has_the_documented_shape() {
    let o = opts(Mode::Plugin);
    let caps = sample_caps();
    let out = install_out(&o, &[action("/x", "dry_run", "")], None, &caps);
    assert_eq!(out.json["client"], "claude");
    assert_eq!(out.json["scope"], "user");
    assert_eq!(out.json["mode"], "plugin");
    assert_eq!(out.json["dry_run"], false);
    assert_eq!(out.json["actions"][0]["result"], "dry_run");
    assert!(out.json["probe"].is_null());
    assert_eq!(out.json["capabilities"]["claude_version"], "2.1.293");
}

#[test]
fn uninstall_json_has_null_capabilities_and_no_probe() {
    let out = uninstall_out(&opts(Mode::Entries), &[action("/x", "removed", "")]);
    assert_eq!(out.json["mode"], "entries");
    assert!(out.json["capabilities"].is_null());
    assert!(out.json.get("probe").is_none());
    assert_eq!(out.text, "removed /x");
}

const NOW: i64 = 1_700_000_000_000;

/// An existing agora home directory inside `dir`.
fn make_home(dir: &Path) -> PathBuf {
    let home = dir.join("agora");
    std::fs::create_dir(&home).expect("home");
    home
}

#[test]
fn record_writes_the_claude_entry_into_an_existing_home() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = make_home(dir.path());
    let actions = record_capabilities(&opts_at(home.clone(), Mode::Plugin), &sample_caps(), NOW)
        .expect("record");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].result, "wrote", "{}", actions[0].detail);
    let file = capabilities::load(&Paths::new(&home)).expect("capabilities.json loads");
    assert_eq!(file.claude, Some(sample_caps()));
}

#[test]
fn unparsable_capabilities_are_moved_aside_before_the_fresh_write() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = make_home(dir.path());
    std::fs::write(capabilities::path(&Paths::new(&home)), "not json").expect("seed");
    let actions = record_capabilities(&opts_at(home.clone(), Mode::Plugin), &sample_caps(), NOW)
        .expect("record");
    let results: Vec<&str> = actions.iter().map(|a| a.result.as_str()).collect();
    assert_eq!(results, ["backed_up", "wrote"]);
    let bak = home.join(format!("capabilities.json.agora-bak-{NOW}"));
    assert_eq!(std::fs::read_to_string(&bak).expect("backup"), "not json");
    assert_eq!(
        actions[0].detail,
        format!(
            "unparsable capabilities.json moved aside to {}",
            bak.display()
        )
    );
    let fresh = capabilities::load(&Paths::new(&home)).expect("fresh file loads");
    assert_eq!(fresh.claude, Some(sample_caps()));
}

#[test]
fn an_unchanged_capabilities_file_is_not_rewritten() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = make_home(dir.path());
    let o = opts_at(home.clone(), Mode::Plugin);
    record_capabilities(&o, &sample_caps(), NOW).expect("first record");
    let file = capabilities::path(&Paths::new(&home));
    let before = std::fs::read(&file).expect("read");
    let actions = record_capabilities(&o, &sample_caps(), NOW + 1).expect("second record");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].result, "unchanged");
    assert_eq!(std::fs::read(&file).expect("read"), before);
    assert_eq!(
        std::fs::read_dir(&home).expect("list").count(),
        1,
        "no backup"
    );
}

#[test]
fn a_symlinked_capabilities_file_is_refused_and_its_target_untouched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = make_home(dir.path());
    let target = dir.path().join("elsewhere.json");
    std::fs::write(&target, "keep").expect("target");
    std::os::unix::fs::symlink(&target, capabilities::path(&Paths::new(&home))).expect("symlink");
    let Err(err) = record_capabilities(&opts_at(home, Mode::Plugin), &sample_caps(), NOW) else {
        panic!("a symlinked capabilities.json must be refused");
    };
    assert!(matches!(err, CliError::Usage(_)), "{err}");
    assert_eq!(std::fs::read_to_string(&target).expect("read"), "keep");
}

#[test]
fn a_failed_capabilities_write_is_an_error_not_an_action() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("not-a-dir");
    std::fs::write(&home, "x").expect("file");
    assert!(record_capabilities(&opts_at(home, Mode::Plugin), &sample_caps(), NOW).is_err());
}

#[test]
fn capabilities_plan_backs_up_only_an_unloadable_file_and_skips_equal_ones() {
    assert_eq!(capabilities_plan(false, false, false), "wrote");
    assert_eq!(capabilities_plan(true, false, false), "backed_up");
    assert_eq!(capabilities_plan(true, true, false), "wrote");
    assert_eq!(capabilities_plan(true, true, true), "unchanged");
}

#[test]
fn a_relative_exe_is_joined_onto_the_current_dir_and_an_absolute_one_is_kept() {
    assert_eq!(
        absolute_against(PathBuf::from("bin/agora"), Path::new("/work")),
        PathBuf::from("/work/bin/agora")
    );
    assert_eq!(
        absolute_against(PathBuf::from("/usr/bin/agora"), Path::new("/work")),
        PathBuf::from("/usr/bin/agora")
    );
}

#[test]
fn a_placeholder_in_the_home_or_the_exe_is_a_usage_error() {
    let a = args(ScopeArg::User, ModeArg::Plugin, false);
    for (home, exe) in [
        ("/abs/${AGORA}", "/bin/agora"),
        ("/abs/home", "/bin/${X}/agora"),
    ] {
        let Err(err) = options_from(
            PathBuf::from(home),
            &a,
            Some(OsString::from("/h/u")),
            PathBuf::from("/proj"),
            PathBuf::from(exe),
        ) else {
            panic!("{home} / {exe} must be refused");
        };
        assert!(
            matches!(err, CliError::Usage(ref m) if m.contains("${")),
            "{err}"
        );
    }
    let plain = options_from(
        PathBuf::from("/abs/$HOME/x"),
        &a,
        Some(OsString::from("/h/u")),
        PathBuf::from("/proj"),
        PathBuf::from("/bin/agora"),
    );
    assert!(plain.is_ok(), "a lone $ is not a placeholder");
}

#[test]
fn ensure_home_makes_a_private_home_and_keeps_an_existing_mode() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("a").join("agora");
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    ensure_home(&home).expect("create");
    assert_eq!(mode(&home), 0o700);
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    ensure_home(&home).expect("existing home");
    assert_eq!(mode(&home), 0o755, "an existing home keeps its mode");
}

#[test]
fn project_scope_text_warns_about_absolute_paths_only_when_written() {
    let mut o = opts(Mode::Plugin);
    o.scope = Scope::Project;
    assert!(install_text(&o, &[], None).contains(PROJECT_NOTE));
    o.dry_run = true;
    assert!(!install_text(&o, &[], None).contains(PROJECT_NOTE));
    o.dry_run = false;
    o.scope = Scope::User;
    assert!(!install_text(&o, &[], None).contains(PROJECT_NOTE));
}

#[test]
fn uninstall_keeps_the_claude_entry_while_the_other_scope_uses_agora() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = make_home(dir.path());
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join(".claude")).expect("project");
    let settings = project.join(".claude").join("settings.json");
    std::fs::write(&settings, r#"{"enabledPlugins":{"agora@agora":true}}"#).expect("settings");
    let mut o = opts_at(home.clone(), Mode::Plugin);
    o.project_dir = project;
    record_capabilities(&o, &sample_caps(), NOW).expect("record");

    assert_eq!(clear_capabilities(&o).result, "kept");
    let kept = capabilities::load(&Paths::new(&home)).expect("loads");
    assert_eq!(kept.claude, Some(sample_caps()));

    std::fs::remove_file(&settings).expect("remove");
    assert_eq!(clear_capabilities(&o).result, "wrote");
    let cleared = capabilities::load(&Paths::new(&home)).expect("loads");
    assert!(cleared.claude.is_none());
}

#[test]
fn record_and_clear_keep_unknown_top_level_keys() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("agora");
    std::fs::create_dir(&home).expect("home");
    let file = capabilities::path(&Paths::new(&home));
    std::fs::write(&file, r#"{"schema":1,"other_tool":{"x":1}}"#).expect("seed file");
    let o = opts_at(home.clone(), Mode::Plugin);

    assert_eq!(
        record_capabilities(&o, &sample_caps(), NOW).expect("record")[0].result,
        "wrote"
    );
    let kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
    assert_eq!(kept["other_tool"]["x"], 1);
    assert!(kept["claude"].is_object());

    assert_eq!(clear_capabilities(&o).result, "wrote");
    let cleared: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
    assert_eq!(cleared["other_tool"]["x"], 1);
    assert!(cleared.get("claude").is_none());
    assert_eq!(clear_capabilities(&o).result, "unchanged");
}

#[test]
fn clear_without_a_capabilities_file_is_absent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let o = opts_at(dir.path().join("missing"), Mode::Plugin);
    assert_eq!(clear_capabilities(&o).result, "absent");
}

/// Dry-run options for `dir`: every path is under `dir`, so a dry run that wrote anything would
/// leave a file there.
fn dry_options(dir: &Path) -> Options {
    let mut a = args(ScopeArg::User, ModeArg::Plugin, true);
    a.client = "all".to_owned();
    options_from(
        dir.join("agora-home"),
        &a,
        Some(dir.join("user").into_os_string()),
        dir.join("project"),
        dir.join("bin").join("agora"),
    )
    .expect("options")
}

#[test]
fn a_codex_list_runs_the_multi_path_and_returns_a_clients_array() {
    let dir = tempfile::tempdir().expect("temp dir");
    let o = dry_options(dir.path());
    let out = dispatch_install(&o, &[Client::Codex], false, false).expect("codex dry run");
    assert_eq!(out.json["clients"][0]["client"], "codex");
}

#[test]
fn all_clients_dry_run_lists_seven_items_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let o = dry_options(dir.path());
    let out = dispatch_install(&o, &Client::ALL, false, false).expect("dry run of all clients");
    let items = out.json["clients"].as_array().expect("clients array");
    assert_eq!(items.len(), 7);
    let names: Vec<&str> = items.iter().filter_map(|i| i["client"].as_str()).collect();
    assert_eq!(names, Client::ALL.map(Client::as_str));
    assert_eq!(fs::read_dir(dir.path()).expect("list temp dir").count(), 0);
}

#[test]
fn entries_mode_without_claude_is_a_usage_error_for_install_and_uninstall() {
    for client in ["codex", "codex,agy"] {
        let mut a = args(ScopeArg::User, ModeArg::Entries, true);
        a.client = client.to_owned();
        let Err(err) = install(PathBuf::from("/nonexistent/agora"), a) else {
            panic!("install must reject --mode entries with client {client:?}");
        };
        assert!(
            matches!(err, CliError::Usage(ref m) if m.contains("--mode applies to --client claude only")),
            "{err}"
        );

        let mut a = args(ScopeArg::User, ModeArg::Entries, true);
        a.client = client.to_owned();
        let Err(err) = uninstall(PathBuf::from("/nonexistent/agora"), a) else {
            panic!("uninstall must reject --mode entries with client {client:?}");
        };
        assert!(matches!(err, CliError::Usage(_)), "{err}");
    }
}
