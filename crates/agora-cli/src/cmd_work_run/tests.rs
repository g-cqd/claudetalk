use super::output::{Announcer, json_document, tails_for, waiting_line};
use super::request::{MAX_TIMEOUT, env_scope, run_request, timeout};
use super::wait_spec::wait_spec;
use crate::{WorkRunArgs, WorkWaitArgs};
use agora_sched::model::{TaskRecord, WaitSpec};
use clap::Parser;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const BASE: &str = "/nonexistent-agora-base";

fn base() -> &'static Path {
    Path::new(BASE)
}

fn run_args(task: Option<&str>, command: &[&str]) -> WorkRunArgs {
    WorkRunArgs {
        task: task.map(str::to_owned),
        id: None,
        weight: None,
        files: Vec::new(),
        cwd: None,
        kind: None,
        title: None,
        scope: None,
        card: None,
        timeout: 570.0,
        tail: 5,
        interval: None,
        agent: None,
        command: command.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

fn wait_args(ids: &[&str]) -> WorkWaitArgs {
    WorkWaitArgs {
        ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        any: false,
        all: false,
        scope: None,
        pid: Vec::new(),
        file: Vec::new(),
        gone: Vec::new(),
        log: None,
        card: Vec::new(),
        state: Vec::new(),
        channel: None,
        after: None,
        notices: false,
        timeout: 570.0,
        tail: 5,
        interval: None,
    }
}

fn task(extra: Value) -> TaskRecord {
    let mut base = json!({
        "seq": 1, "id": "job-1", "title": "", "kind": "other", "tier": 1,
        "author": "pk", "author_name": "alice", "state": "running",
        "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0
    });
    if let (Some(object), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    serde_json::from_value(base).expect("a task record")
}

#[test]
fn run_needs_a_task_or_a_command() {
    let err = run_request(&run_args(None, &[]), None, base()).expect_err("refused");
    assert_eq!(err, "name a task you claimed, or give a command after --");
}

#[test]
fn run_refuses_a_task_and_a_command_together() {
    let err = run_request(&run_args(Some("job-1"), &["true"]), None, base()).expect_err("refused");
    assert_eq!(err, "give a task ID or a command after --, not both");
}

#[test]
fn run_with_a_task_refuses_new_job_flags_in_order() {
    let mut a = run_args(Some("job-1"), &[]);
    a.weight = Some(2.0);
    a.kind = Some("bug".to_owned());
    a.title = Some("t".to_owned());
    let err = run_request(&a, None, base()).expect_err("refused");
    assert_eq!(
        err,
        "--weight, --kind, --title describe a new job; give its command after --"
    );
}

#[test]
fn run_with_a_task_takes_scope_from_the_environment_as_the_default() {
    let mut a = run_args(Some("job-1"), &[]);
    a.scope = Some("proj".to_owned());
    assert!(run_request(&a, Some("proj"), base()).is_ok());
    let err = run_request(&a, Some("other"), base()).expect_err("refused");
    assert_eq!(err, "--scope describe a new job; give its command after --");
}

#[test]
fn run_with_a_task_allows_the_wait_flags() {
    let mut a = run_args(Some("job-1"), &[]);
    a.timeout = 5.0;
    a.tail = 9;
    a.interval = Some(0.5);
    a.agent = Some("ignored".to_owned());
    let request = run_request(&a, None, base()).expect("accepted");
    assert_eq!(request.task.as_deref(), Some("job-1"));
    assert!(request.add.is_none());
}

#[test]
fn run_inline_fills_the_defaults() {
    let request = run_request(&run_args(None, &["make", "test"]), None, base()).expect("accepted");
    assert!(request.task.is_none());
    let add = request.add.expect("an inline job");
    assert_eq!(add.kind, "other");
    assert_eq!(add.title, "");
    assert_eq!(add.weight, 1.0);
    assert_eq!(add.size, 1.0);
    assert_eq!(add.value, 1.0);
    assert_eq!(
        add.command,
        Some(vec!["make".to_owned(), "test".to_owned()])
    );
    assert_eq!(add.cwd.as_deref(), Some(BASE));
    assert!(add.scope.is_none());
}

#[test]
fn run_inline_resolves_a_relative_cwd_and_files_against_the_base() {
    let mut a = run_args(None, &["true"]);
    a.cwd = Some("sub".to_owned());
    a.files = vec!["src/app".to_owned()];
    let add = run_request(&a, None, base())
        .expect("accepted")
        .add
        .expect("inline");
    assert_eq!(add.cwd.as_deref(), Some("/nonexistent-agora-base/sub"));
    assert_eq!(
        add.files,
        vec!["/nonexistent-agora-base/sub/src/app".to_owned()]
    );
}

#[test]
fn run_inline_refuses_a_weight_that_is_not_a_positive_number() {
    for weight in [f64::NAN, 0.0, -1.0, f64::INFINITY] {
        let mut a = run_args(None, &["true"]);
        a.weight = Some(weight);
        let err = run_request(&a, None, base()).expect_err("refused");
        assert!(err.contains("is not a finite number above zero"), "{err}");
    }
}

#[test]
fn timeout_accepts_zero_and_refuses_negative_or_nan() {
    assert_eq!(timeout(0.0), Ok(Duration::ZERO));
    assert_eq!(timeout(0.5), Ok(Duration::from_millis(500)));
    assert!(timeout(-1.0).is_err());
    assert!(timeout(f64::NAN).is_err());
    assert!(timeout(f64::INFINITY).is_err());
    assert_eq!(timeout(1e300), Ok(MAX_TIMEOUT));
}

#[test]
fn wait_with_no_mode_names_the_choices() {
    let err = wait_spec(&wait_args(&[]), None).expect_err("refused");
    assert_eq!(
        err,
        "wait for one thing: task IDs, --all, --pid, --file, --gone, --log, --card, --channel or --notices"
    );
}

#[test]
fn wait_with_two_modes_names_both() {
    let mut a = wait_args(&["job-1"]);
    a.pid = vec![42];
    let err = wait_spec(&a, None).expect_err("refused");
    assert!(err.ends_with(", not task IDs and --pid"), "{err}");
}

#[test]
fn wait_any_needs_task_ids() {
    let mut a = wait_args(&[]);
    a.any = true;
    a.file = vec!["out.txt".to_owned()];
    let err = wait_spec(&a, None).expect_err("refused");
    assert_eq!(err, "--any goes with task IDs");
}

#[test]
fn wait_scope_needs_all_unless_it_is_the_environment_default() {
    let mut a = wait_args(&["job-1"]);
    a.scope = Some("proj".to_owned());
    assert_eq!(
        wait_spec(&a, None).expect_err("refused"),
        "--scope goes with --all"
    );
    assert!(wait_spec(&a, Some("proj")).is_ok());
}

#[test]
fn wait_ids_map_to_tasks_with_any() {
    let mut a = wait_args(&["a", "b"]);
    a.any = true;
    assert_eq!(
        wait_spec(&a, None),
        Ok(WaitSpec::Tasks {
            ids: vec!["a".to_owned(), "b".to_owned()],
            any: true,
        })
    );
}

#[test]
fn wait_all_maps_an_empty_scope_to_every_scope() {
    let mut a = wait_args(&[]);
    a.all = true;
    a.scope = Some(String::new());
    assert_eq!(wait_spec(&a, None), Ok(WaitSpec::All { scope: None }));
    a.scope = Some("proj".to_owned());
    assert_eq!(
        wait_spec(&a, None),
        Ok(WaitSpec::All {
            scope: Some("proj".to_owned())
        })
    );
}

#[test]
fn wait_pid_refuses_a_non_positive_process_id() {
    let mut a = wait_args(&[]);
    a.pid = vec![7, 0];
    assert_eq!(
        wait_spec(&a, None).expect_err("refused"),
        "--pid 0 is not a process id"
    );
}

/// The CLI maps relative paths unchanged; the client absolutises them before the request.
#[test]
fn wait_file_gone_and_log_map_their_targets() {
    let mut file = wait_args(&[]);
    file.file = vec!["done.txt".to_owned()];
    assert_eq!(
        wait_spec(&file, None),
        Ok(WaitSpec::File {
            paths: vec!["done.txt".to_owned()]
        })
    );

    let mut gone = wait_args(&[]);
    gone.gone = vec!["P1.lock".to_owned()];
    assert_eq!(
        wait_spec(&gone, None),
        Ok(WaitSpec::Gone {
            paths: vec!["P1.lock".to_owned()]
        })
    );

    let mut log = wait_args(&[]);
    log.log = Some(vec!["build.log".to_owned(), "Build complete".to_owned()]);
    assert_eq!(
        wait_spec(&log, None),
        Ok(WaitSpec::Log {
            path: "build.log".to_owned(),
            pattern: "Build complete".to_owned(),
        })
    );
}

#[test]
fn wait_card_channel_and_notices_map_their_targets() {
    let mut card = wait_args(&[]);
    card.card = vec!["c1".to_owned()];
    card.state = vec!["review".to_owned()];
    assert_eq!(
        wait_spec(&card, None),
        Ok(WaitSpec::Cards {
            ids: vec!["c1".to_owned()],
            states: vec!["review".to_owned()],
        })
    );

    let mut channel = wait_args(&[]);
    channel.channel = Some("general".to_owned());
    channel.after = Some(12);
    assert_eq!(
        wait_spec(&channel, None),
        Ok(WaitSpec::Messages {
            channel: "general".to_owned(),
            after: 12,
        })
    );

    let mut notices = wait_args(&[]);
    notices.notices = true;
    assert_eq!(
        wait_spec(&notices, None),
        Ok(WaitSpec::Notices { after: -1 })
    );
}

#[derive(Parser, Debug)]
struct WaitWrap {
    #[command(flatten)]
    a: WorkWaitArgs,
}

/// Parses the `wait` flags the way the binary does (`WORK_SCOPE` included) and maps them to a spec.
fn parsed_spec(argv: &[&str]) -> Result<WaitSpec, String> {
    let w =
        WaitWrap::try_parse_from(std::iter::once("x").chain(argv.iter().copied())).expect("parse");
    wait_spec(&w.a, env_scope().as_deref())
}

#[test]
fn wait_notices_takes_after_as_the_sequence_number() {
    assert_eq!(
        parsed_spec(&["--notices", "--after", "5"]),
        Ok(WaitSpec::Notices { after: 5 })
    );
}

#[test]
fn wait_after_without_channel_or_notices_is_refused() {
    assert_eq!(
        parsed_spec(&["--after", "5"]).expect_err("refused"),
        "--after goes with --channel or --notices"
    );
    assert_eq!(
        parsed_spec(&["--after", "5", "--pid", "7"]).expect_err("refused"),
        "--after goes with --channel or --notices"
    );
}

#[test]
fn announcer_reports_the_start_once_when_the_pid_appears() {
    let mut announcer = Announcer::default();
    let running = task(json!({
        "state": "running", "pid": 4242, "log": "/logs/job-1.log",
        "command": ["touch", "/tmp/x"]
    }));
    assert_eq!(
        announcer.observe(&running).as_deref(),
        Some("work: started job job-1 (touch /tmp/x) as process 4242; output in /logs/job-1.log")
    );
    assert_eq!(announcer.observe(&running), None);
}

#[test]
fn announcer_reports_the_capacity_wait_once_before_the_pid() {
    let mut announcer = Announcer::default();
    let waiting = task(json!({
        "state": "claimed", "requested_ms": 1, "note": "predicted 1 of 1 cores are taken",
        "command": ["true"]
    }));
    assert_eq!(
        announcer.observe(&waiting).as_deref(),
        Some("work: job job-1 waits for capacity: predicted 1 of 1 cores are taken")
    );
    assert_eq!(announcer.observe(&waiting), None);
    let running = task(json!({"state": "running", "pid": 9, "log": "/l"}));
    assert!(
        announcer
            .observe(&running)
            .is_some_and(|line| line.starts_with("work: started job"))
    );
}

#[test]
fn announcer_says_nothing_for_a_claim_without_a_request_or_note() {
    let mut announcer = Announcer::default();
    assert_eq!(announcer.observe(&task(json!({"state": "claimed"}))), None);
    assert_eq!(
        announcer.observe(&task(
            json!({"state": "claimed", "requested_ms": 1, "note": ""})
        )),
        None
    );
}

#[test]
fn waiting_line_lists_the_ids_and_the_any_suffix() {
    let ids = vec!["a".to_owned(), "b".to_owned()];
    assert_eq!(waiting_line(&ids, false), "work: waiting for a b");
    assert_eq!(
        waiting_line(&ids, true),
        "work: waiting for a b (the first to end)"
    );
    assert_eq!(
        waiting_line(&["solo".to_owned()], true),
        "work: waiting for solo"
    );
}

#[test]
fn json_document_has_the_status_flags_and_a_tail_per_task() {
    let tasks = vec![task(json!({"state": "failed", "status": 3, "id": "job-9"}))];
    let tails = vec![vec!["last line".to_owned()]];
    let doc = json_document(3, false, &tasks, &tails);
    assert_eq!(doc["status"], json!(3));
    assert_eq!(doc["timed_out"], json!(false));
    assert_eq!(doc["tasks"][0]["id"], json!("job-9"));
    assert_eq!(doc["tasks"][0]["status"], json!(3));
    assert_eq!(doc["tasks"][0]["tail"], json!(["last line"]));
}

#[test]
fn json_document_gives_an_empty_tail_when_none_was_read() {
    let tasks = vec![task(json!({"state": "running"}))];
    let doc = json_document(124, true, &tasks, &[]);
    assert_eq!(doc["timed_out"], json!(true));
    assert_eq!(doc["tasks"][0]["tail"], json!([]));
}

#[test]
fn tails_are_read_only_for_final_or_timed_out_tasks() {
    let running = task(json!({"state": "running", "log": "/nonexistent-agora-log"}));
    let done = task(json!({"state": "done", "log": "/nonexistent-agora-log"}));
    let none = task(json!({"state": "done"}));
    assert_eq!(
        tails_for(&[running.clone(), done, none], false, 5),
        vec![Vec::<String>::new(), Vec::new(), Vec::new()]
    );
    assert_eq!(tails_for(&[running], true, 5), vec![Vec::<String>::new()]);
}
