use super::*;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Parses `agora ARGS...` exactly as `main` does.
fn parse(args: &[String]) -> Cli {
    Cli::try_parse_from(std::iter::once("agora".to_string()).chain(args.iter().cloned()))
        .expect("parses")
}

#[test]
fn token_file_writes_to_its_file_before_or_after_the_token_subcommand() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = dir.path().join("home");
    let file = dir.path().join("tokens.json");
    let f = file.display().to_string();
    let orders = [
        strings(&["mcp", "--token-file", &f, "token", "add", "--label", "l"]),
        strings(&["mcp", "token", "add", "--label", "l", "--token-file", &f]),
    ];
    for args in orders {
        let cli = parse(&args);
        let Cmd::Mcp(McpArgs {
            cmd: Some(McpCmd::Token(c)),
            token_file,
            http: None,
            ..
        }) = cli.cmd
        else {
            panic!("not a token command: {args:?}");
        };
        assert_eq!(token_file.as_deref(), Some(file.as_path()), "{args:?}");
        cmd_mcp::token(home.clone(), false, token_file, c).expect("token add");
        assert!(file.exists(), "{args:?}");
        std::fs::remove_file(&file).expect("reset");
    }
    assert!(!agora_mcp::tokens::default_path(&Paths::new(&home)).exists());
}

#[test]
fn token_file_without_http_is_refused_by_the_stdio_server() {
    let dir = tempfile::tempdir().expect("temp dir");
    let f = dir.path().join("tokens.json").display().to_string();
    let cli = parse(&strings(&["mcp", "--token-file", f.as_str()]));
    let Cmd::Mcp(args) = cli.cmd else {
        panic!("not mcp");
    };
    let err = cmd_mcp::run(dir.path().to_path_buf(), args).expect_err("refused");
    assert_eq!(err, "--token-file needs --http");
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(|a| OsString::from(*a)).collect()
}

#[test]
fn work_order_with_tasks_is_routed_before_parsing() {
    let some = |a: &[&str]| given_tasks_args(&os(a));
    assert_eq!(
        some(&["agora", "work", "order", "--task", "id=a"]),
        Some(strings(&["order", "--task", "id=a"]))
    );
    assert_eq!(
        some(&[
            "agora", "--home", "/h", "--json", "work", "plan", "--agents", "2", "--tasks", "-"
        ]),
        Some(strings(&[
            "plan", "--agents", "2", "--tasks", "-", "--json"
        ]))
    );
    assert_eq!(some(&["agora", "work", "order", "--policy", "wsjf"]), None);
}

#[test]
fn only_work_order_and_plan_are_candidates_for_early_routing() {
    let some = |a: &[&str]| given_tasks_args(&os(a));
    assert_eq!(some(&["agora", "work", "add", "--task", "x"]), None);
    assert_eq!(
        some(&["agora", "chat", "work", "order", "--task", "a"]),
        None
    );
    assert_eq!(some(&["agora", "--home"]), None);
    assert_eq!(some(&["agora", "work"]), None);
    assert_eq!(some(&["agora"]), None);
}

#[test]
fn work_commands_declare_their_flags() {
    for args in [
        &["work", "run", "--timeout", "1", "--", "sleep", "5"][..],
        &["work", "wait", "a", "b", "--any", "--tail", "2"],
        &["work", "wait", "--log", "/tmp/x", "done"],
        &["work", "wait", "--card", "c1", "--state", "review", "done"],
        &[
            "work", "add", "--title", "t", "--after", "a", "b", "--files", "/x", "--", "true",
        ],
        &[
            "work",
            "settings",
            "--cpu-budget",
            "4",
            "--self-accept",
            "true",
        ],
        &["work", "schedule", "add", "--every", "5m", "--", "true"],
        &["work", "update", "t1", "--assign", "ab"],
        &["pool", "lease", "--path", "/x"],
        &["quiet", "open", "--for-secs", "60"],
        &[
            "policy",
            "set",
            "core",
            "main",
            "--wip-limit",
            "3",
            "--reclaim",
        ],
        &["card", "link", "c1", "blocks", "c2"],
        &[
            "card",
            "create",
            "c1",
            "t",
            "--priority",
            "5",
            "--after",
            "c0",
        ],
        &["metrics", "--team", "core"],
        &["_supervise", "--job-dir", "/tmp/j"],
    ] {
        Cli::try_parse_from(std::iter::once("agora").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
}

#[test]
fn schedule_add_needs_exactly_one_time() {
    assert!(Cli::try_parse_from(["agora", "work", "schedule", "add", "--", "true"]).is_err());
    assert!(
        Cli::try_parse_from([
            "agora", "work", "schedule", "add", "--at", "1", "--every", "60", "--", "true"
        ])
        .is_err()
    );
}

#[test]
fn hook_invocation_skips_global_options() {
    for args in [
        vec!["agora", "hook", "context"],
        vec!["agora", "--json", "hook", "stop"],
        vec!["agora", "--home", "/x", "hook", "stop"],
        vec!["agora", "--home=/x", "hook"],
    ] {
        assert!(is_hook_invocation(&os(&args)), "{args:?}");
    }
}

#[test]
fn other_invocations_are_not_hooks() {
    for args in [
        vec!["agora", "inbox"],
        vec!["agora", "chat", "hook"],
        vec!["agora"],
    ] {
        assert!(!is_hook_invocation(&os(&args)), "{args:?}");
    }
}

#[test]
fn hook_parse_errors_are_silent_except_help_and_version() {
    let missing = Cli::try_parse_from(["agora", "hook"]).expect_err("no subcommand");
    assert!(hook_parse_error_is_silent(
        missing.kind(),
        &os(&["agora", "hook"])
    ));
    let help = Cli::try_parse_from(["agora", "hook", "--help"]).expect_err("help");
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(!hook_parse_error_is_silent(
        help.kind(),
        &os(&["agora", "hook", "--help"])
    ));
    let bad = Cli::try_parse_from(["agora", "inbox", "--bogus"]).expect_err("bad flag");
    assert!(!hook_parse_error_is_silent(
        bad.kind(),
        &os(&["agora", "inbox", "--bogus"])
    ));
}

#[test]
fn hook_rewake_quick_parses() {
    let cli = parse(&strings(&["hook", "rewake", "--quick"]));
    let Cmd::Hook(HookArgs {
        cmd: HookCmd::Rewake {
            quick,
            max_wait_secs,
        },
        ..
    }) = cli.cmd
    else {
        panic!("not a rewake hook");
    };
    assert!(quick);
    assert_eq!(max_wait_secs, None);
}

#[test]
fn hook_poll_parses_session_and_folder() {
    let cli = parse(&strings(&[
        "hook",
        "poll",
        "--session",
        "s",
        "--folder",
        "/tmp",
    ]));
    let Cmd::Hook(HookArgs {
        cmd: HookCmd::Poll { session, folder },
        ..
    }) = cli.cmd
    else {
        panic!("not a poll hook");
    };
    assert_eq!(session, "s");
    assert_eq!(folder, PathBuf::from("/tmp"));
}

#[test]
fn install_parses_scope_mode_and_flags() {
    let cli = parse(&strings(&[
        "install",
        "--client",
        "claude",
        "--scope",
        "project",
        "--mode",
        "entries",
        "--probe",
        "--dry-run",
    ]));
    let Cmd::Install(a) = cli.cmd else {
        panic!("not install");
    };
    assert_eq!(a.client, "claude");
    assert_eq!(a.scope, ScopeArg::Project);
    assert_eq!(a.mode, ModeArg::Entries);
    assert!(a.probe);
    assert!(a.dry_run);
}

#[test]
fn uninstall_defaults_to_user_scope_and_plugin_mode() {
    let cli = parse(&strings(&["uninstall", "--client", "claude"]));
    let Cmd::Uninstall(a) = cli.cmd else {
        panic!("not uninstall");
    };
    assert_eq!(a.scope, ScopeArg::User);
    assert_eq!(a.mode, ModeArg::Plugin);
    assert!(!a.probe);
    assert!(!a.dry_run);
}

#[test]
fn unexpanded_placeholder_home_is_treated_as_unset() {
    let cli = parse(&strings(&["--home", "${AGORA_HOME:-}", "whoami"]));
    assert_eq!(home_of(&cli), DaemonConfig::default_home());
    let cli = parse(&strings(&["--home", "/x/home", "whoami"]));
    assert_eq!(home_of(&cli), PathBuf::from("/x/home"));
}
