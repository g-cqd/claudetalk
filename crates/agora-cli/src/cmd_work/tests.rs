use super::*;
use crate::WorkPlanArgs;
use clap::Parser;

#[derive(Parser, Debug)]
struct PlanWrap {
    #[command(flatten)]
    a: WorkPlanArgs,
}

#[derive(Parser, Debug)]
struct AddWrap {
    #[command(flatten)]
    a: WorkAddArgs,
}

#[derive(Parser, Debug)]
struct ScheduleWrap {
    #[command(flatten)]
    a: ScheduleAddArgs,
}

#[derive(Parser, Debug)]
struct SettingsWrap {
    #[command(flatten)]
    a: WorkSettingsArgs,
}

#[derive(Parser, Debug)]
struct UpdateWrap {
    #[command(flatten)]
    a: WorkUpdateArgs,
}

fn add(argv: &[&str]) -> WorkAddArgs {
    AddWrap::try_parse_from(std::iter::once("x").chain(argv.iter().copied()))
        .expect("parse add")
        .a
}

#[test]
fn plan_agents_on_the_queue_path_is_bounded_to_256() {
    let ok = PlanWrap::try_parse_from(["x", "--agents", "256"]).expect("parse");
    assert!(check_agents(ok.a.agents).is_ok());
    let over = PlanWrap::try_parse_from(["x", "--agents", "300"]).expect("parse");
    let err = check_agents(over.a.agents).expect_err("refused");
    assert!(matches!(err, CliError::Usage(_)));
    assert_eq!(err.to_string(), "agents must be between 1 and 256");
    assert_eq!(exit_for_error(&err), EXIT_INPUT);
}

#[test]
fn add_args_applies_work_py_defaults_and_keeps_given_flags() {
    let args = add_args(add(&["--title", "build", "--cwd", "/p", "--size", "2"])).expect("add");
    assert_eq!(args.title, "build");
    assert_eq!(args.kind, "other");
    assert_eq!(args.size, 2.0);
    assert_eq!(args.value, 1.0);
    assert_eq!(args.weight, 1.0);
    assert_eq!(args.cwd.as_deref(), Some("/p"));
    assert_eq!(args.command, None);
    assert_eq!(args.id, None);
}

#[test]
fn add_args_takes_the_command_after_dashes_and_none_without_it() {
    let with = add_args(add(&["--cwd", "/p", "--", "touch", "/tmp/x"])).expect("add");
    assert_eq!(
        with.command,
        Some(vec!["touch".to_owned(), "/tmp/x".to_owned()])
    );
    let without = add_args(add(&["--cwd", "/p"])).expect("add");
    assert_eq!(without.command, None);
}

#[test]
fn add_args_defaults_the_cwd_to_the_current_directory() {
    let args = add_args(add(&["--title", "t"])).expect("add");
    assert_eq!(args.cwd, Some(current_dir_string().expect("cwd")));
}

#[test]
fn add_args_refuses_bad_names_and_kinds_as_input_errors() {
    for argv in [
        vec!["--id", "bad id", "--cwd", "/p"],
        vec!["--after", "a b", "--cwd", "/p"],
        vec!["--kind", "a b", "--cwd", "/p"],
        vec!["--scope=-x", "--cwd", "/p"],
    ] {
        let err = add_args(add(&argv)).expect_err("refused");
        assert!(matches!(err, CliError::Usage(_)), "{argv:?}: {err}");
        assert_eq!(exit_for_error(&err), EXIT_INPUT);
    }
}

#[test]
fn add_args_refuses_a_non_positive_size() {
    let zero = add_args(add(&["--size", "0", "--cwd", "/p"])).expect_err("zero");
    assert!(matches!(zero, CliError::Usage(_)));
    assert!(add_args(add(&["--value=-1", "--cwd", "/p"])).is_err());
    assert!(add_args(add(&["--weight", "2.5", "--cwd", "/p"])).is_ok());
}

#[test]
fn timer_args_names_the_timer_and_leaves_the_template_without_an_id() {
    let w = ScheduleWrap::try_parse_from([
        "x",
        "--at",
        "1700000000000",
        "--id",
        "nightly",
        "--title",
        "t",
        "--cwd",
        "/p",
    ])
    .expect("parse");
    let t = timer_args(w.a).expect("timer");
    assert_eq!(t.id.as_deref(), Some("nightly"));
    assert_eq!(t.template.id, None);
    assert_eq!(t.template.title, "t");
    assert!(matches!(
        t.spec,
        TimerSpec::At {
            at_ms: 1_700_000_000_000
        }
    ));
}

#[test]
fn timer_spec_maps_every_and_cron() {
    let every = ScheduleWrap::try_parse_from(["x", "--every", "5m", "--", "true"]).expect("parse");
    assert!(matches!(
        timer_spec(&every.a.when).expect("every"),
        TimerSpec::Every { every_s: 300, .. }
    ));
    let cron =
        ScheduleWrap::try_parse_from(["x", "--cron", "*/5 * * * *", "--", "true"]).expect("parse");
    assert!(matches!(
        timer_spec(&cron.a.when).expect("cron"),
        TimerSpec::Cron { .. }
    ));
}

#[test]
fn timer_spec_refuses_a_short_interval_and_a_bad_cron() {
    let short = ScheduleWrap::try_parse_from(["x", "--every", "30"]).expect("parse");
    assert!(matches!(timer_spec(&short.a.when), Err(CliError::Usage(_))));
    let bad = ScheduleWrap::try_parse_from(["x", "--cron", "61 * * * *"]).expect("parse");
    assert!(matches!(timer_spec(&bad.a.when), Err(CliError::Usage(_))));
    let none = ScheduleWhen {
        at: None,
        every: None,
        cron: None,
    };
    assert!(matches!(timer_spec(&none), Err(CliError::Usage(_))));
}

#[test]
fn exit_for_maps_input_errors_and_refusals() {
    assert_eq!(exit_for("usage", "x"), EXIT_INPUT);
    assert_eq!(exit_for("not_found", "task task-9"), EXIT_INPUT);
    assert_eq!(exit_for("not_found", "scope team"), EXIT_REFUSED);
    assert_eq!(
        exit_for("invalid", "a command must be accepted"),
        EXIT_REFUSED
    );
    assert_eq!(exit_for("forbidden", "x"), EXIT_REFUSED);
    assert_eq!(exit_for("not_member", "x"), EXIT_REFUSED);
    assert_eq!(exit_for("daemon", "unavailable"), EXIT_REFUSED);
    assert_eq!(exit_for_error(&CliError::Usage("x".into())), EXIT_INPUT);
}

#[test]
fn exit_byte_masks_the_work_status_to_a_byte() {
    assert_eq!(exit_byte(0), 0);
    assert_eq!(exit_byte(2), 2);
    assert_eq!(exit_byte(124), 124);
    assert_eq!(exit_byte(256), 0);
    assert_eq!(exit_byte(300), 44);
    assert_eq!(exit_byte(-1), 255);
}

#[test]
fn settings_request_reads_when_no_value_flag_is_given() {
    let none = SettingsWrap::try_parse_from(["x"]).expect("parse");
    assert_eq!(settings_request(&none.a), None);
    let set = SettingsWrap::try_parse_from(["x", "--cpu-budget", "4"]).expect("parse");
    let patch = settings_request(&set.a).expect("patch");
    assert_eq!(patch.cpu_budget.as_deref(), Some("4"));
}

#[test]
fn settings_request_maps_run_role_and_max_waiting() {
    let set = SettingsWrap::try_parse_from(["x", "--run-role", "supervisor", "--max-waiting", "8"])
        .expect("parse");
    let patch = settings_request(&set.a).expect("patch");
    assert_eq!(patch.run_role.as_deref(), Some("supervisor"));
    assert_eq!(patch.max_waiting.as_deref(), Some("8"));
    assert_eq!(patch.cpu_budget, None, "other fields keep inheriting");
}

#[test]
fn settings_request_sends_one_new_flag_as_a_change() {
    let only = SettingsWrap::try_parse_from(["x", "--run-role", "owner"]).expect("parse");
    let patch = settings_request(&only.a).expect("a change, not a read");
    assert_eq!(patch.run_role.as_deref(), Some("owner"));
    assert_eq!(patch.max_waiting, None);
}

#[test]
fn status_report_unwraps_the_report_object_only() {
    let inner =
        serde_json::json!({"machine": {"cpus": 8}, "running": [], "waiting": [], "counts": {}});
    let wrapped = serde_json::json!({"report": inner.clone()});
    assert_eq!(status_report(wrapped), inner);
    let bare = serde_json::json!({"machine": {"cpus": 8}, "running": []});
    assert_eq!(
        status_report(bare.clone()),
        bare,
        "no report key: unchanged"
    );
    let extra = serde_json::json!({"report": {"counts": {}}, "other": 1});
    assert_eq!(
        status_report(extra.clone()),
        extra,
        "not only a report: unchanged"
    );
    assert_eq!(
        status_report(serde_json::Value::Null),
        serde_json::Value::Null
    );
}

#[test]
fn status_out_unwraps_the_json_and_keeps_the_text() {
    let out = status_out(Out {
        json: serde_json::json!({"report": {"counts": {"queued": 2}}}),
        text: "status text".into(),
    });
    assert_eq!(out.json, serde_json::json!({"counts": {"queued": 2}}));
    assert_eq!(out.text, "status text");
}

#[test]
fn update_args_maps_assign_and_unassign() {
    let unassign = UpdateWrap::try_parse_from(["x", "t1", "--unassign"]).expect("parse");
    assert_eq!(update_args(&unassign.a).expect("u").agent, Some(None));
    let assign = UpdateWrap::try_parse_from(["x", "t1", "--assign", "pk1"]).expect("parse");
    assert_eq!(
        update_args(&assign.a).expect("a").agent,
        Some(Some("pk1".to_owned()))
    );
    let keep = UpdateWrap::try_parse_from(["x", "t1"]).expect("parse");
    assert_eq!(update_args(&keep.a).expect("k").agent, None);
    let bad_kind = UpdateWrap::try_parse_from(["x", "t1", "--kind", "a b"]).expect("parse");
    assert!(matches!(update_args(&bad_kind.a), Err(CliError::Usage(_))));
}

#[test]
fn claimed_none_detects_a_null_claim() {
    let none = Out {
        json: serde_json::json!({"claimed": null, "reason": "nothing is queued"}),
        text: String::new(),
    };
    assert!(claimed_none(&none));
    let got = Out {
        json: serde_json::json!({"claimed": {"id": "t1"}, "reason": ""}),
        text: String::new(),
    };
    assert!(!claimed_none(&got));
    let missing = Out {
        json: serde_json::json!({}),
        text: String::new(),
    };
    assert!(!claimed_none(&missing));
}

#[test]
fn normalize_digest_trims_and_lowercases() {
    assert_eq!(normalize_digest("  ABC123 \n"), "abc123");
}

#[test]
fn scope_kind_parses_the_three_kinds_and_refuses_others() {
    assert!(matches!(
        scope_kind(Some("project")),
        Ok(Some(ScopeKind::Project))
    ));
    assert!(matches!(scope_kind(None), Ok(None)));
    assert!(matches!(scope_kind(Some("team")), Err(CliError::Usage(_))));
}

#[test]
fn read_source_reads_a_file_and_stdin_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("tasks.json");
    std::fs::write(&file, "[]").expect("write");
    let mut used = false;
    let mut unused: &[u8] = b"ignored";
    let text = read_source(&file.display().to_string(), &mut used, &mut unused).expect("file");
    assert_eq!(text, "[]");
    assert!(!used, "a file does not use stdin");

    let mut first: &[u8] = b"[1]";
    assert_eq!(
        read_source("-", &mut used, &mut first).expect("first"),
        "[1]"
    );
    assert!(used);
    let mut second: &[u8] = b"[2]";
    assert_eq!(
        read_source("-", &mut used, &mut second).expect("second"),
        ""
    );
    assert_eq!(second, b"[2]", "stdin is not read twice");
}

#[test]
fn read_source_reports_a_missing_file() {
    let mut used = false;
    let mut empty: &[u8] = b"";
    let err = read_source(
        "/nonexistent/agora-m5-cli-work/tasks.json",
        &mut used,
        &mut empty,
    )
    .expect_err("missing");
    assert!(err.starts_with("cannot read /nonexistent/agora-m5-cli-work/tasks.json: "));
}

#[test]
fn given_tasks_refuses_an_unreadable_file_with_exit_2() {
    let args: Vec<String> = ["order", "--tasks", "/nonexistent/agora-m5-cli-work/t.json"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut empty: &[u8] = b"";
    let out = given_tasks_with(&args, &mut empty);
    assert_eq!(out.exit_code, EXIT_INPUT);
    assert!(out.stdout.is_empty());
    assert!(
        out.stderr
            .starts_with("work: cannot read /nonexistent/agora-m5-cli-work/t.json"),
        "{}",
        out.stderr
    );
}
