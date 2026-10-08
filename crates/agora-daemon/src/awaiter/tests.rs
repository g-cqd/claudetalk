//! Unit tests of the awaiter's pure helpers: the cursor codec, the status rules, the role gates (filesystem waits and
//! log tails), the target cap, the notice floor, the task-log selection, the pattern size cap and the validation.

use super::cursor::{Cursor, decode_cursor, encode_cursor, seconds};
use super::rules::{
    Caller, MAX_TARGETS, card_states, compile_pattern, file_status, known_floor, may_read_tail,
    need_worker, numeric_status, refuse_job_paths, sole_path, tail_candidate, task_done,
    task_status, within_targets,
};
use crate::DaemonError;
use crate::watch::MAX_CARRY;
use agora_core::{Role, TransitionError};
use agora_proto::{b64, unb64};
use agora_sched::model::TaskRecord;
use agora_store::StoreError;
use std::path::Path;

fn task(id: &str, state: &str, status: Option<i32>, ended: Option<i64>) -> TaskRecord {
    serde_json::from_value(serde_json::json!({
        "seq": 1, "id": id, "title": "t", "kind": "task", "tier": 1,
        "author": "a", "author_name": "a", "state": state,
        "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0,
        "status": status, "ended_ms": ended,
    }))
    .expect("a task record")
}

/// A running task authored by `author` and claimed by `agent` (unclaimed when `None`).
fn owned(author: &str, agent: Option<&str>) -> TaskRecord {
    let mut record = task("t", "running", None, None);
    record.author = author.into();
    record.agent = agent.map(String::from);
    record
}

fn caller(key: &str, role: Role) -> Caller {
    Caller {
        key: key.into(),
        role,
    }
}

fn invalid_cursor(err: &DaemonError) -> bool {
    matches!(
        err,
        DaemonError::Store(StoreError::Invalid("invalid wait cursor"))
    )
}

#[test]
fn cursor_round_trips_every_field() {
    let cursor = Cursor {
        elapsed_ms: 1500,
        offset: 42,
        carry: "partial \"line\"".into(),
        ids: vec!["a".into(), "b".into()],
        notice_after: Some(9),
    };
    let text = encode_cursor(&cursor);
    assert_eq!(decode_cursor(Some(&text)).expect("decodes"), Some(cursor));
}

#[test]
fn cursor_is_base64_of_the_documented_json_without_empty_ids() {
    let text = encode_cursor(&Cursor {
        elapsed_ms: 7,
        offset: 9,
        carry: "x".into(),
        ids: Vec::new(),
        notice_after: None,
    });
    let json = unb64(&text, "cursor").expect("base64");
    assert_eq!(
        String::from_utf8(json).expect("utf-8"),
        r#"{"elapsed_ms":7,"offset":9,"carry":"x"}"#
    );
}

#[test]
fn cursor_for_the_largest_carry_stays_small() {
    let text = encode_cursor(&Cursor {
        carry: "a".repeat(MAX_CARRY),
        ..Cursor::default()
    });
    // 64 KiB of JSON is about 87 KiB of base64: far inside one frame.
    assert!(text.len() <= 88 * 1024, "{} bytes", text.len());
    assert!(decode_cursor(Some(&text)).expect("decodes").is_some());
}

#[test]
fn missing_cursor_is_none_and_garbage_is_refused() {
    assert!(decode_cursor(None).expect("no cursor").is_none());
    let not_json = b64(b"not json");
    for bad in ["zz", "abc", not_json.as_str()] {
        let err = decode_cursor(Some(bad)).expect_err("not a cursor");
        assert!(invalid_cursor(&err), "{bad}");
    }
}

#[test]
fn cursor_carry_over_the_cap_is_refused() {
    let at_cap = encode_cursor(&Cursor {
        carry: "a".repeat(MAX_CARRY),
        ..Cursor::default()
    });
    assert!(decode_cursor(Some(&at_cap)).expect("at the cap").is_some());
    let over = encode_cursor(&Cursor {
        carry: "a".repeat(MAX_CARRY + 1),
        ..Cursor::default()
    });
    let err = decode_cursor(Some(&over)).expect_err("over the cap");
    assert!(invalid_cursor(&err));
}

#[test]
fn cursor_with_more_than_the_target_cap_is_refused() {
    let ids: Vec<String> = (0..=MAX_TARGETS).map(|i| i.to_string()).collect();
    let text = encode_cursor(&Cursor {
        ids,
        ..Cursor::default()
    });
    let err = decode_cursor(Some(&text)).expect_err("too many ids");
    assert!(
        matches!(&err, DaemonError::Store(StoreError::Invalid(m)) if *m == "name at most 256 targets"),
        "{err}"
    );
}

#[test]
fn target_lists_are_capped_at_256() {
    assert!(within_targets(MAX_TARGETS).is_ok());
    let err = within_targets(MAX_TARGETS + 1).expect_err("257 targets");
    assert!(
        matches!(&err, DaemonError::Store(StoreError::Invalid(m)) if *m == "name at most 256 targets"),
        "{err}"
    );
}

#[test]
fn numeric_status_takes_plain_digits() {
    assert_eq!(numeric_status(b"7"), Some(7));
    assert_eq!(numeric_status(b" 42\n"), Some(42));
    assert_eq!(numeric_status(b"0\n"), Some(0));
    assert_eq!(numeric_status(b"256"), Some(256));
    assert_eq!(numeric_status(b"2147483647"), Some(i32::MAX));
}

#[test]
fn numeric_status_ignores_any_other_content() {
    let cases: [&[u8]; 8] = [
        b"",
        b"  \n",
        b"\r\n",
        b"abc",
        b"1 2",
        b"12\n3",
        b"123456789012",
        b"-1",
    ];
    for text in cases {
        assert_eq!(numeric_status(text), None, "{text:?}");
    }
}

#[test]
fn numeric_status_wraps_only_values_that_do_not_fit_an_i32() {
    let wrapped = i32::try_from(99_999_999_999_i64 % 256).expect("fits");
    assert_eq!(numeric_status(b"99999999999"), Some(wrapped));
}

#[test]
fn file_status_reads_a_regular_file_and_refuses_a_fifo_without_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = dir.path().join("status");
    std::fs::write(&status, b"5\n").expect("write");
    assert_eq!(file_status(&status), 5);
    assert_eq!(file_status(&dir.path().join("absent")), 0);

    let fifo = dir.path().join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo");
    // Opening a FIFO with no writer blocks unless the open is non-blocking: the check must return at once.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(file_status(&fifo));
    });
    let status = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("file_status returned without blocking");
    assert_eq!(status, 0);
}

#[test]
fn sole_path_needs_exactly_one_distinct_path() {
    assert_eq!(sole_path(&["a".into(), "a".into()]), Some("a"));
    assert_eq!(sole_path(&["a".into(), "b".into()]), None);
    assert_eq!(sole_path(&[]), None);
}

#[test]
fn card_states_default_when_none_are_given() {
    assert_eq!(
        card_states(Vec::new()).expect("defaults"),
        ["review", "done", "blocked", "cancelled"]
    );
    assert_eq!(card_states(vec!["ready".into()]).expect("given"), ["ready"]);
    assert!(card_states(vec!["nope".into()]).is_err());
}

#[test]
fn task_done_and_status_follow_any_and_all() {
    let done = task("a", "done", Some(0), Some(5));
    let failed = task("b", "failed", Some(3), Some(9));
    let running = task("c", "running", None, None);
    assert!(task_done(&[done.clone(), running.clone()], true));
    assert!(!task_done(&[done.clone(), running.clone()], false));
    assert!(task_done(&[done.clone(), failed.clone()], false));
    // All: the first task, in the given order, that is not done decides.
    assert_eq!(task_status(&[done.clone(), failed.clone()], false), 3);
    // Any: the task that ended first decides (done at 5 before failed at 9).
    assert_eq!(task_status(&[failed.clone(), done.clone()], true), 0);
    assert_eq!(task_status(&[done], false), 0);
}

#[test]
fn tail_is_the_first_final_task_in_order_when_done() {
    let running = task("r", "running", None, None);
    let failed = task("f", "failed", Some(3), Some(9));
    let done = task("d", "done", Some(0), Some(5));
    let tasks = [running, failed, done];
    let picked = tail_candidate(&tasks, false, |_| true).map(|t| t.id.as_str());
    assert_eq!(picked, Some("f"), "the given order, not the end time");
    let only_running = [task("r", "running", None, None)];
    assert!(
        tail_candidate(&only_running, false, |_| true).is_none(),
        "nothing final, nothing to show"
    );
}

#[test]
fn tail_on_a_timeout_is_the_first_task_with_a_log() {
    let tasks = [
        task("a", "running", None, None),
        task("b", "running", None, None),
    ];
    let picked = tail_candidate(&tasks, true, |id| id == "b").map(|t| t.id.as_str());
    assert_eq!(picked, Some("b"));
    assert!(tail_candidate(&tasks, true, |_| false).is_none());
}

#[test]
fn seconds_round_to_three_decimals_and_never_go_negative() {
    assert_eq!(seconds(1234), 1.234);
    assert_eq!(seconds(1), 0.001);
    assert_eq!(seconds(2_000), 2.0);
    assert_eq!(seconds(-5), 0.0);
}

#[test]
fn notice_floor_takes_a_given_seq_and_resumes_a_negative_one() {
    assert_eq!(known_floor(7, Some(3)), Some(7), "a given seq wins");
    assert_eq!(known_floor(0, None), Some(0));
    assert_eq!(
        known_floor(-1, Some(3)),
        Some(3),
        "-1 keeps the resolved seq"
    );
    assert_eq!(
        known_floor(-1, None),
        None,
        "-1 with no stored seq reads the latest"
    );
}

#[test]
fn only_worker_or_more_may_run_the_filesystem_and_process_waits() {
    let err = need_worker(Role::Observer).expect_err("an observer");
    assert!(
        matches!(
            &err,
            DaemonError::Store(StoreError::Transition(TransitionError::Forbidden(
                Role::Worker
            )))
        ),
        "{err}"
    );
    for role in [Role::Worker, Role::Supervisor, Role::Owner] {
        assert!(need_worker(role).is_ok(), "{role:?}");
    }
}

#[test]
fn the_author_reads_the_tail_of_its_task_at_any_role() {
    let record = owned("ka", Some("kc"));
    assert!(may_read_tail(&record, &caller("ka", Role::Worker)));
    assert!(may_read_tail(&record, &caller("ka", Role::Observer)));
}

#[test]
fn the_claimant_reads_the_tail_of_its_task() {
    let record = owned("ka", Some("kc"));
    assert!(may_read_tail(&record, &caller("kc", Role::Worker)));
}

#[test]
fn a_supervisor_or_more_reads_any_tail() {
    let record = owned("ka", Some("kc"));
    for role in [Role::Supervisor, Role::Controller, Role::Owner] {
        assert!(may_read_tail(&record, &caller("kx", role)), "{role:?}");
    }
}

#[test]
fn another_worker_gets_no_tail_of_someone_elses_task() {
    let record = owned("ka", Some("kc"));
    assert!(!may_read_tail(&record, &caller("kx", Role::Worker)));
}

#[test]
fn an_observer_who_is_neither_author_nor_claimant_gets_no_tail() {
    let record = owned("ka", Some("kc"));
    assert!(!may_read_tail(&record, &caller("kx", Role::Observer)));
}

#[test]
fn an_unclaimed_task_matches_no_claimant_below_supervisor() {
    // A missing claimant is not the empty key: a worker never gains the tail by matching nothing.
    let record = owned("ka", None);
    assert!(!may_read_tail(&record, &caller("kx", Role::Worker)));
    assert!(may_read_tail(&record, &caller("ka", Role::Worker)));
}

#[test]
fn a_pattern_over_the_one_mib_cap_is_refused_though_the_default_accepts_it() {
    // The regex crate's default limit is 10 MiB; this program is between the two, so only the 1 MiB cap refuses it.
    assert!(regex::Regex::new("(a{300}){300}").is_ok());
    let err = compile_pattern("(a{300}){300}").expect_err("over 1 MiB");
    assert!(
        matches!(
            &err,
            DaemonError::Store(StoreError::Refused(m))
                if m.contains("exceeds size limit of 1048576 bytes")
        ),
        "{err}"
    );
}

#[test]
fn a_pattern_that_blows_past_the_cap_is_refused() {
    let err = compile_pattern("(a{1000}){1000}").expect_err("far over 1 MiB");
    assert!(
        matches!(
            &err,
            DaemonError::Store(StoreError::Refused(m))
                if m.starts_with("'(a{1000}){1000}' is not a valid regular expression: ")
                    && m.contains("size limit")
        ),
        "{err}"
    );
}

#[test]
fn a_pattern_under_the_cap_still_compiles() {
    assert!(compile_pattern("(a{100}){100}").is_ok());
}

#[test]
fn invalid_pattern_is_refused_with_the_regex_error() {
    let err = compile_pattern("(").expect_err("unbalanced group");
    assert!(
        matches!(
            &err,
            DaemonError::Store(StoreError::Refused(m))
                if m.starts_with("'(' is not a valid regular expression: ")
        ),
        "{err}"
    );
    assert!(compile_pattern(r"^\d+$").is_ok());
}

/// The work root the job-path tests check against (absent on disk, so the lexical rule decides).
const WORK: &str = "/home/agora/work";

fn job_paths(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

fn is_supervisor_forbidden(err: &DaemonError) -> bool {
    matches!(
        err,
        DaemonError::Store(StoreError::Transition(TransitionError::Forbidden(
            Role::Supervisor
        )))
    )
}

#[test]
fn a_path_under_the_work_root_is_refused_below_supervisor() {
    let paths = job_paths(&["/home/agora/work/logs/job-1.log"]);
    for role in [Role::Observer, Role::Worker] {
        let err = refuse_job_paths(&paths, role, Path::new(WORK)).expect_err("refused");
        assert!(is_supervisor_forbidden(&err), "{role:?}: {err}");
    }
}

#[test]
fn a_supervisor_or_more_reads_a_path_under_the_work_root() {
    let paths = job_paths(&["/home/agora/work/logs/job-1.log"]);
    for role in [Role::Supervisor, Role::Controller, Role::Owner] {
        assert!(
            refuse_job_paths(&paths, role, Path::new(WORK)).is_ok(),
            "{role:?}"
        );
    }
}

#[test]
fn a_path_outside_the_work_root_is_allowed_for_a_worker() {
    // A sibling that shares the root's text prefix is outside it: the match is by path component.
    let paths = job_paths(&[
        "/home/agora/notes/build.log",
        "/home/agora/workshop/build.log",
        "/tmp/build.log",
    ]);
    assert!(refuse_job_paths(&paths, Role::Worker, Path::new(WORK)).is_ok());
    assert!(refuse_job_paths(&[], Role::Worker, Path::new(WORK)).is_ok());
}

#[test]
fn one_path_under_the_work_root_refuses_the_whole_list() {
    let paths = job_paths(&[
        "/home/agora/notes/build.log",
        "/home/agora/work/jobs/t/status.json",
    ]);
    assert!(refuse_job_paths(&paths, Role::Worker, Path::new(WORK)).is_err());
}

#[test]
fn dot_and_dot_dot_segments_are_resolved_before_the_check() {
    let root = Path::new(WORK);
    for inside in [
        "/home/agora/work/../work/logs/x.log",
        "/home/agora/work/./logs/x.log",
        "/home/agora/notes/../work/logs/x.log",
    ] {
        let paths = job_paths(&[inside]);
        assert!(
            refuse_job_paths(&paths, Role::Worker, root).is_err(),
            "{inside}"
        );
    }
    let outside = job_paths(&["/home/agora/work/../notes/x.log"]);
    assert!(refuse_job_paths(&outside, Role::Worker, root).is_ok());
}

#[test]
fn a_symlink_into_the_work_root_is_refused_even_for_a_file_not_yet_created() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("logs")).expect("create the logs dir");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&work, &alias).expect("symlink the dir");

    // The log does not exist yet: its existing ancestor, the alias, resolves into the work root.
    let future = job_paths(&[&alias.join("logs").join("new.log").display().to_string()]);
    assert!(refuse_job_paths(&future, Role::Worker, &work).is_err());

    // A file link whose target is a job log is refused by the target's location.
    let log = work.join("logs").join("job.log");
    std::fs::write(&log, b"x\n").expect("write the log");
    let peek = dir.path().join("peek.log");
    std::os::unix::fs::symlink(&log, &peek).expect("symlink the file");
    let via_file = job_paths(&[&peek.display().to_string()]);
    assert!(refuse_job_paths(&via_file, Role::Worker, &work).is_err());

    // The root given through the alias is the same root.
    let via_alias_root = job_paths(&[&log.display().to_string()]);
    assert!(refuse_job_paths(&via_alias_root, Role::Worker, &alias).is_err());
}
