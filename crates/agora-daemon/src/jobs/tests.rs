//! Unit tests of the job helpers that need no process and no daemon: the exact texts, the outcome and note rules, the
//! environment fallback, the start arguments, and the job table. End-to-end process tests live in the daemon's
//! integration tests.

use super::*;
use agora_run::jobdir::{JobStatus, SupervisorInfo};
use agora_sched::model::JobStartArgs;
use std::path::Path;
use std::time::Duration;

fn info() -> SupervisorInfo {
    SupervisorInfo {
        supervisor_pid: 10,
        supervisor_start_s: Some(100),
        pid: Some(20),
        pid_start_s: Some(200),
        started_ms: 0,
        qos: "utility".into(),
    }
}

#[test]
fn context_line_names_the_load_the_idle_cpu_and_the_other_jobs() {
    let others = ["t1".to_owned(), "t2".to_owned()];

    let line = context_line(Some(1.5), Some(42.4), &others);

    assert_eq!(
        line,
        "work: context at start: load 1.50, CPU 42% idle; running: t1, t2"
    );
}

#[test]
fn context_line_writes_unknown_signals_and_no_other_job_as_such() {
    let line = context_line(None, None, &[]);

    assert_eq!(
        line,
        "work: context at start: load unknown, CPU idle unknown; running: no other job"
    );
}

#[test]
fn outcome_follows_the_state_the_store_records() {
    // A cancel wins over everything, as it does in the store.
    assert_eq!(outcome(true, false, Some(0)), JobOutcome::Cancelled);
    assert_eq!(outcome(true, true, None), JobOutcome::Cancelled);
    // Status 0 after a real start is done.
    assert_eq!(outcome(false, false, Some(0)), JobOutcome::Done);
    // A job that never started fails even with status 0, and so does any other status or none.
    assert_eq!(outcome(false, true, Some(0)), JobOutcome::Failed);
    assert_eq!(outcome(false, false, Some(3)), JobOutcome::Failed);
    assert_eq!(outcome(false, false, None), JobOutcome::Failed);
}

#[test]
fn effective_note_keeps_the_task_note_when_the_supervisor_has_none() {
    assert_eq!(
        effective_note("", "cancelled by Ada: enough"),
        "cancelled by Ada: enough"
    );
    assert_eq!(
        effective_note("killed by the log cap", "old"),
        "killed by the log cap"
    );
    assert_eq!(effective_note("", ""), "");
}

#[test]
fn minimal_env_keeps_only_path_home_and_lang() {
    let host = [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/a"),
        ("LANG", "C.UTF-8"),
        ("SECRET_TOKEN", "x"),
        ("USER", "a"),
    ];
    let env = minimal_env(|name| {
        host.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    });

    let names: Vec<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(names, ["HOME", "LANG", "PATH"]);
    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));

    let missing_lang = minimal_env(|name| (name != "LANG").then(|| "v".to_owned()));
    assert!(!missing_lang.contains_key("LANG"));
    assert_eq!(missing_lang.len(), 2);
}

#[test]
fn a_job_whose_command_never_started_ends_before_its_start() {
    let status = JobStatus {
        status: None,
        note: "its directory /gone is gone".into(),
        cpu_seconds: None,
        rss_peak_kib: None,
        started_ms: None,
        ended_ms: 77,
        started: false,
        cancelled: false,
        over_limit: false,
    };

    let fin = Finish::from_status(&status);

    assert!(fin.before_start);
    assert_eq!(fin.status, None);
    assert_eq!(fin.note, "its directory /gone is gone");
    assert_eq!(fin.ended_ms, 77);

    let started = JobStatus {
        started: true,
        status: Some(2),
        ..status
    };
    assert!(!Finish::from_status(&started).before_start);
}

#[test]
fn open_jobs_are_running_or_claimed_and_requested() {
    assert!(is_open(TaskState::Running, None));
    assert!(is_open(TaskState::Claimed, Some(1)));
    assert!(!is_open(TaskState::Claimed, None));
    assert!(!is_open(TaskState::Queued, Some(1)));
    assert!(!is_open(TaskState::Done, Some(1)));
    assert!(!is_open(TaskState::Cancelled, None));
}

#[test]
fn ended_notes_use_the_d8_texts() {
    assert_eq!(
        gone_note(10, 20),
        "its supervisor, process 10, is gone; the command runs on as process 20, and its status will not be recorded"
    );
    assert_eq!(
        ended_without_status_note(10),
        "the job ended without a recorded status: its supervisor, process 10, is gone"
    );
}

#[test]
fn supervisor_and_command_prefer_the_report_then_the_recorded_pid() {
    let report = info();
    assert_eq!(
        supervisor_of(Some(&report), Some(99)),
        Some((10, Some(100)))
    );
    assert_eq!(command_of(Some(&report), Some(99)), Some((20, Some(200))));

    // Without a report, the recorded pid stands with no start time.
    assert_eq!(supervisor_of(None, Some(42)), Some((42, None)));
    assert_eq!(command_of(None, Some(42)), Some((42, None)));

    // A negative or out-of-range recorded pid is no pid at all.
    assert_eq!(command_of(None, Some(-1)), None);
    assert_eq!(supervisor_of(None, Some(i64::from(u32::MAX) + 1)), None);
    assert_eq!(supervisor_of(None, None), None);
}

#[test]
fn start_args_need_the_command_pid_from_the_report() {
    let mut without_pid = info();
    without_pid.pid = None;
    assert_eq!(start_args(&without_pid, false, None, "/l.log"), None);

    let args = start_args(&info(), true, Some(5), "/l.log");

    assert_eq!(
        args,
        Some(JobStartArgs {
            pid: 20,
            supervisor_pid: 10,
            supervisor_start_s: Some(100),
            qos: "utility".into(),
            exclusive: true,
            quiet_until_ms: Some(5),
            log: "/l.log".into(),
        })
    );
}

#[test]
fn job_dir_stays_under_the_jobs_directory_of_the_work_root() {
    let root = Path::new("/home/a/work");

    let dir = dir_in(root, "t1").expect("a plain task id");
    assert_eq!(dir.dir.as_path(), Path::new("/home/a/work/jobs/t1"));

    assert!(dir_in(root, "../escape").is_err());
    assert!(dir_in(root, "").is_err());
}

#[test]
fn job_table_keeps_an_environment_until_it_is_taken_or_forgotten() {
    let table = JobTable::default();
    let env = BTreeMap::from([("PATH".to_owned(), "/bin".to_owned())]);
    table.put_env("t1", env.clone());
    table.put_env("t2", env.clone());

    assert_eq!(table.take_env("t1"), Some(env));
    assert_eq!(table.take_env("t1"), None);

    table.forget("t2");
    assert_eq!(table.take_env("t2"), None);
}

#[tokio::test]
async fn shutdown_aborts_tracked_tasks_that_never_finish() {
    let table = JobTable::default();
    table.spawn(std::future::pending());

    tokio::time::timeout(Duration::from_secs(5), table.shutdown())
        .await
        .expect("shutdown returns once the pending task is aborted");
}

#[test]
fn a_card_sync_failure_is_retried_without_the_sync_only_for_a_store_error() {
    let store = DaemonError::Store(agora_store::StoreError::Invalid("card.job_sync"));

    assert!(retry_without_sync(true, &store));
    // An attempt that carried no sync has nothing to drop, and a dead writer is not worth a second try.
    assert!(!retry_without_sync(false, &store));
    assert!(!retry_without_sync(true, &DaemonError::ActorGone));
    assert!(!retry_without_sync(true, &DaemonError::Config("x".into())));
}

#[test]
fn a_pid_without_its_start_time_is_never_trusted_to_be_the_job_process() {
    // This test process is alive, but a pid recorded without its start time may have been reused: no verification.
    assert!(!verified_alive(std::process::id(), None));
    assert!(!verified_alive(0, Some(1)));
    assert!(!verified_alive(0, None));
}

#[test]
fn a_task_awaiting_its_start_is_claimed_requested_and_not_started() {
    assert!(awaiting_start(TaskState::Claimed, Some(1), None));
    // Cancelled, started, never requested, or queued: each is no longer waiting for a supervisor.
    assert!(!awaiting_start(TaskState::Claimed, None, None));
    assert!(!awaiting_start(TaskState::Claimed, Some(1), Some(2)));
    assert!(!awaiting_start(TaskState::Running, Some(1), Some(2)));
    assert!(!awaiting_start(TaskState::Cancelled, Some(1), None));
    assert!(!awaiting_start(TaskState::Queued, Some(1), None));
}

#[test]
fn the_start_gate_is_shared_by_every_clone_of_the_table() {
    let table = JobTable::default();
    let clone = table.clone();

    let held = table.gate().try_lock();
    assert!(held.is_ok());
    assert!(clone.gate().try_lock().is_err());
    drop(held);
    assert!(clone.gate().try_lock().is_ok());
}
