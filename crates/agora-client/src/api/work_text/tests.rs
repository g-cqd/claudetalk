use agora_sched::model::{StatusReport, WaitResult};

use super::*;
use serde_json::{Value, json};

/// A task record from defaults plus the given fields.
fn task(extra: Value) -> TaskRecord {
    let mut value = json!({
        "seq": 1, "id": "task-1", "title": "", "kind": "other", "tier": 2,
        "author": "aa", "author_name": "ada", "state": "queued",
        "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) {
        for (key, field) in extra {
            base.insert(key, field);
        }
    }
    serde_json::from_value(value).expect("valid task record")
}

fn scope(id: &str, kind: &str, parent: Option<&str>, task: &str, counts: Value) -> ScopeRecord {
    serde_json::from_value(json!({
        "id": id, "kind": kind, "parent": parent, "task": task,
        "updated_ms": 0, "counts": counts
    }))
    .expect("valid scope record")
}

#[test]
fn show_frames_a_command_before_the_task_fields() {
    let t = task(json!({
        "id": "job-7", "title": "build", "author_name": "bob", "state": "claimed",
        "agent": "cc", "agent_name": "carol", "needs_acceptance": true,
        "command": ["make", "test"], "cwd": "/tmp/proj", "digest": "abc123", "claimed_ms": 1000
    }));
    let text = show_text(&t, 5000);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        &lines[..5],
        [
            "command of task job-7 (2 arguments, 9 bytes), queued by bob:",
            "  make test",
            "directory: /tmp/proj",
            "digest: abc123",
            "acceptance: needed from carol; if the command is right, run `agora work accept job-7 --digest abc123`",
        ]
    );
    assert_eq!(lines[5], "task job-7: build");
    assert_eq!(lines[6], "state: claimed by carol");
    assert_eq!(lines[7], "kind: other; size 1, value 1, weight 1");
    assert_eq!(
        lines[8],
        "scope: -; author: bob; queued 1970-01-01 00:00:00"
    );
}

#[test]
fn show_names_the_accepter_once_the_claimant_accepted() {
    let t = task(json!({
        "id": "job-7", "author_name": "bob", "state": "claimed", "agent": "cc",
        "agent_name": "carol", "accepted_by": "cc", "needs_acceptance": false,
        "command": ["ls"], "cwd": "/tmp", "digest": "d1"
    }));
    let text = show_text(&t, 0);
    assert!(text.contains("\nacceptance: accepted by carol\n"), "{text}");
}

#[test]
fn show_says_the_claimant_accepts_an_unclaimed_command() {
    let t = task(
        json!({"id": "job-7", "author_name": "bob", "command": ["ls"], "cwd": "/tmp", "digest": "d1"}),
    );
    let text = show_text(&t, 0);
    assert!(
        text.contains("\nacceptance: the agent that claims it accepts it after reading it\n"),
        "{text}"
    );
    assert!(
        text.starts_with("command of task job-7 (1 argument, 2 bytes), queued by bob:\n  ls\n")
    );
}

#[test]
fn show_escapes_control_and_invisible_characters_everywhere() {
    let t = task(json!({
        "id": "job-8", "title": "a\u{1b}[31mred", "author_name": "bob",
        "command": ["echo", "x\u{202e}y"], "cwd": "/tmp/p\nq", "digest": "ff",
        "note": "x\ty", "state": "queued"
    }));
    let text = show_text(&t, 0);
    assert!(text.contains("\n  echo 'x\\u202ey'\n"), "{text}");
    assert!(text.contains("directory: /tmp/p\\nq\n"), "{text}");
    assert!(text.contains("\ntask job-8: a\\x1b[31mred\n"), "{text}");
    assert!(text.ends_with("\nnote: x\\ty"), "{text}");
    assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}'));
}

#[test]
fn show_caps_the_files_list_at_ten_and_counts_all() {
    let files: Vec<String> = (0..12).map(|i| format!("/f/{i}")).collect();
    let t = task(json!({"id": "job-9", "files": files}));
    let expected = "files (the first 10 of 12; all of them with --json): /f/0, /f/1, /f/2, /f/3, /f/4, /f/5, \
                        /f/6, /f/7, /f/8, /f/9";
    assert!(show_text(&t, 0).lines().any(|line| line == expected));
}

#[test]
fn show_run_block_reports_waits_status_usage_and_prediction() {
    let t = task(json!({
        "id": "job-10", "state": "done", "requested_ms": 1000, "started_ms": 4000,
        "ended_ms": 9000, "status": 0, "pid": 4242, "cpu_percent": 99.6, "rss_kib": 2048,
        "rss_peak_kib": 3072, "cpu_seconds": 2.5,
        "prediction": {"cores": 1.0, "rss_kib": 2048, "rss_p90_kib": null, "duration_s": 5.0,
                       "duration_p90_s": 7.0, "level": "full", "runs": 2}
    }));
    let text = show_text(&t, 10_000);
    assert!(
            text.contains(
                "\nrun: waited 3.0 s for capacity, ran 5.0 s, status 0, process 4242, cpu 100%, memory 2 MiB (peak 3 MiB), cpu time 2.5 s\n"
            ),
            "{text}"
        );
    assert!(
            text.ends_with("\npredicted: 1.0 cores, 2 MiB at peak, 5.0 s, p90 7.0 s, from 2 runs of the same command"),
            "{text}"
        );
}

#[test]
fn show_of_a_plain_task_has_no_command_block() {
    let t = task(json!({"id": "job-9", "title": "plan"}));
    let text = show_text(&t, 0);
    assert!(
        text.starts_with("task job-9: plan\nstate: queued\n"),
        "{text}"
    );
    assert!(!text.contains("command of task"));
}

#[test]
fn task_line_has_id_title_kind_state_and_command_author() {
    let t = task(json!({
        "id": "job-6", "title": "ship\u{7}", "kind": "build", "state": "queued",
        "command": ["make"], "author_name": "bob"
    }));
    assert_eq!(
        task_line(&t),
        "job-6 ship\\x07 (build; queued; command by bob)"
    );
    let bare = task(json!({"id": "task-2", "kind": "other"}));
    assert_eq!(task_line(&bare), "task-2 (other; queued)");
}

#[test]
fn report_prints_tails_then_the_finished_status_line() {
    let t = task(json!({
        "id": "t1", "state": "done", "status": 0, "requested_ms": 0,
        "started_ms": 2000, "ended_ms": 5000, "log": "/tmp/t1.log"
    }));
    let lines = report_text(&[t], &[vec!["ok line".to_owned()]], false, 9000);
    assert_eq!(
        lines,
        [
            "ok line",
            "work: job t1 finished with status 0 after 5.0 s (waited 2.0 s for capacity, ran 3.0 s)",
        ]
    );
}

#[test]
fn report_describes_failed_cancelled_and_done_without_status() {
    let failed = task(json!({"id": "t2", "state": "failed", "note": "boom"}));
    let cancelled = task(json!({"id": "t3", "state": "cancelled"}));
    let done = task(json!({"id": "t4", "state": "done", "note": "ok"}));
    let lines = report_text(&[failed, cancelled, done], &[], false, 0);
    assert_eq!(
        lines,
        [
            "work: task t2 failed: boom",
            "work: task t3 was cancelled",
            "work: task t4 is done: ok",
        ]
    );
}

#[test]
fn report_on_timeout_lists_only_open_tasks_in_the_wait_hint() {
    let running = task(json!({
        "id": "t5", "state": "running", "pid": 77, "requested_ms": 0, "started_ms": 1000
    }));
    let queued = task(json!({"id": "t6", "state": "queued"}));
    let done = task(json!({
        "id": "t7", "state": "done", "status": 0, "requested_ms": 0,
        "started_ms": 0, "ended_ms": 0
    }));
    let lines = report_text(&[running, queued, done], &[], true, 6000);
    assert_eq!(
        lines,
        [
            "work: job t5 still running after 5.0 s (waited 1.0 s for capacity)",
            "work: task t6 is still queued",
            "work: job t7 finished with status 0 after 0.0 s (waited 0.0 s for capacity, ran 0.0 s)",
            "work: call `agora work wait t5 t6` to keep waiting",
        ]
    );
}

#[test]
fn report_says_a_claimed_job_is_starting_or_waiting_with_its_note() {
    let starting = task(json!({"id": "t8", "state": "claimed", "requested_ms": 1000}));
    assert_eq!(
        report_text(&[starting], &[], false, 5000),
        ["work: job t8 is starting"]
    );
    let waiting =
        task(json!({"id": "t8", "state": "claimed", "requested_ms": 1000, "note": "cores busy"}));
    assert_eq!(
        report_text(&[waiting], &[], false, 5000),
        ["work: job t8 still waiting for capacity after 4.0 s: cores busy"]
    );
}

#[test]
fn status_prints_machine_cores_jobs_and_tally_in_order() {
    let mut r: StatusReport = serde_json::from_value(json!({
            "time_ms": 5000, "scope": null,
            "machine": {
                "cpus": 8.0, "idle": 42.4, "idle_min": 10.0, "memory": "normal",
                "memory_used_kib": 4_194_304, "memory_budget_kib": null, "memory_limit": "warning",
                "load": 1.5, "load_gate": true, "load_limit": 6.0, "cpu_budget": 8.0, "used": 1.0,
                "capacity": null, "declared": 2.0, "jobs": 1, "holds": null, "quiet": null
            },
            "scopes": [], "running": [], "waiting": [],
            "counts": {"queued": 2, "ready": 1, "claimed": 1, "running": 1, "done": 3, "failed": 0, "cancelled": 0},
            "swept": [{"id": "job-9", "state": "failed", "note": "gone"}]
        }))
        .expect("valid status report");
    r.running.push(task(json!({
        "id": "job-1", "state": "running", "pid": 4242, "started_ms": 0, "requested_ms": 0,
        "agent": "cc", "agent_name": "carol", "weight": 2.0, "command": ["sleep", "9"],
        "qos": "utility", "rss_kib": 2048, "rss_peak_kib": 3072, "cpu_percent": 99.6
    })));
    r.waiting.push(task(json!({
        "id": "job-2", "state": "claimed", "requested_ms": 1000, "agent": "cc",
        "agent_name": "carol", "note": "cores busy"
    })));
    let expected = [
            "machine: 8 effective CPUs; CPU 42% idle (jobs start at 10% or more); memory used 4.0 GiB (no budget); \
             memory pressure normal (jobs start below warning); load 1.50 (jobs start below 6.00)",
            "cores: predicted 1 of the CPU budget of 8 taken by 1 job(s); declared 2 (no cap)",
            "running:",
            "  job-1: process 4242, 5.0 s, cpu 100%, memory 2 MiB (peak 3 MiB), weight 2, holds 2 cores, its weight \
             (no prediction), utility QoS, carol, scope -: sleep 9",
            "waiting for capacity:",
            "  job-2: 4.0 s, weight 1, holds 1 cores, its weight (no prediction), carol, scope -: cores busy",
            "tasks: 2 queued (1 ready), 1 claimed, 1 running, 3 done, 0 failed, 0 cancelled",
            "swept: job-9 failed: gone",
        ]
        .join("\n");
    assert_eq!(status_text(&r, 5000), expected);
}

#[test]
fn scopes_indent_children_under_their_parent() {
    let scopes = [
        scope("root", "project", None, "", json!({"queued": 2})),
        scope("child", "agent", Some("root"), "fix", json!({})),
        scope(
            "grand",
            "conversation",
            Some("child"),
            "",
            json!({"queued": 2, "done": 1}),
        ),
        scope("orphan", "project", Some("gone"), "", json!({})),
    ];
    assert_eq!(
        scopes_text(&scopes),
        "root (project) [2 queued]\n  child (agent): fix\n    grand (conversation) [1 done, 2 queued]\norphan (project)"
    );
    assert_eq!(
        scopes_text(&[]),
        "no scope yet: `agora work scope set ID --kind project`"
    );
}

#[test]
fn next_names_the_claimed_task_and_its_next_step() {
    let with_command = NextOutcome {
        claimed: Some(task(json!({
            "id": "job-3", "author_name": "bob", "state": "claimed", "agent": "cc",
            "agent_name": "carol", "needs_acceptance": true, "command": ["ls"], "digest": "d"
        }))),
        reason: String::new(),
    };
    let text = next_text(&with_command);
    assert_eq!(text.lines().next(), Some("work: claimed task job-3"));
    assert!(text.ends_with(
            "work: job-3 runs a command that bob queued; read it with `agora work show job-3`, and if it is right, \
             accept it with the `agora work accept job-3 --digest ...` line that show prints, then `agora work run job-3`"
        ), "{text}");

    let no_command = NextOutcome {
        claimed: Some(task(
            json!({"id": "job-4", "title": "x", "state": "claimed"}),
        )),
        reason: String::new(),
    };
    assert_eq!(
        next_text(&no_command),
        "work: claimed task job-4: x\nwork: close it with `agora work done job-4` or `agora work fail job-4` when it \
             is finished"
    );
}

#[test]
fn next_reports_nothing_to_claim_with_the_reason() {
    let none = NextOutcome {
            claimed: None,
            reason: "of 3 queued task(s), 2 wait for their prerequisites and 1 change files that claimed or running tasks hold".to_owned(),
        };
    assert_eq!(
        next_text(&none),
        "work: nothing to claim: of 3 queued task(s), 2 wait for their prerequisites and 1 change files that claimed \
             or running tasks hold"
    );
}

#[test]
fn log_lines_have_clock_what_scope_task_actor_and_detail() {
    let events = [
        WorkEvent {
            seq: 1,
            ts_ms: 0,
            kind: "added".to_owned(),
            task: Some("t1".to_owned()),
            scope: Some("proj".to_owned()),
            actor: "aa".to_owned(),
            actor_name: "ada".to_owned(),
            detail: String::new(),
        },
        WorkEvent {
            seq: 2,
            ts_ms: 1_000_000,
            kind: "finished".to_owned(),
            task: None,
            scope: None,
            actor: String::new(),
            actor_name: String::new(),
            detail: "status 0\u{1b}".to_owned(),
        },
    ];
    assert_eq!(
        log_text(&events),
        "1970-01-01 00:00:00 added [proj] t1 by ada\n1970-01-01 00:16:40 finished : status 0\\x1b"
    );
}

#[test]
fn wait_condition_reports_a_pid_that_exited_and_a_timeout_with_the_rerun_hint() {
    let done = WaitResult {
        status: 0,
        timed_out: false,
        condition: "pid".to_owned(),
        targets: vec!["77".to_owned()],
        pending: vec![],
        seconds: 3.0,
        tail: vec![],
        tasks: vec![],
        cursor: None,
    };
    assert_eq!(
        wait_condition_text(&done),
        ["work: every process exited after 3.0 s"]
    );
    let timeout = WaitResult {
        status: 124,
        timed_out: true,
        condition: "gone".to_owned(),
        targets: vec!["/tmp/a b".to_owned()],
        pending: vec!["/tmp/a b".to_owned()],
        seconds: 1.0,
        tail: vec![],
        tasks: vec![],
        cursor: None,
    };
    assert_eq!(
        wait_condition_text(&timeout),
        [
            "work: still present after 1.0 s: /tmp/a b; call `agora work wait --gone '/tmp/a b'` to keep waiting"
        ]
    );
}

#[test]
fn wait_condition_reports_a_file_number_and_log_tail_then_match() {
    let number = WaitResult {
        status: 7,
        timed_out: false,
        condition: "file".to_owned(),
        targets: vec!["/tmp/r.status".to_owned()],
        pending: vec![],
        seconds: 1.5,
        tail: vec![],
        tasks: vec![],
        cursor: None,
    };
    assert_eq!(
        wait_condition_text(&number),
        ["work: finished with status 7 after 1.5 s, from /tmp/r.status"]
    );
    let matched = WaitResult {
        status: 0,
        timed_out: false,
        condition: "log".to_owned(),
        targets: vec!["/tmp/x.log".to_owned()],
        pending: vec![],
        seconds: 0.5,
        tail: vec!["line one".to_owned(), "line\u{7}two".to_owned()],
        tasks: vec![],
        cursor: None,
    };
    assert_eq!(
        wait_condition_text(&matched),
        [
            "line one",
            "line\\x07two",
            "work: the log matched after 0.5 s"
        ]
    );
    let unmatched = WaitResult {
        status: 124,
        timed_out: true,
        condition: "log".to_owned(),
        targets: vec!["/tmp/x.log".to_owned()],
        pending: vec!["/tmp/x.log".to_owned()],
        seconds: 3.0,
        tail: vec![],
        tasks: vec![],
        cursor: None,
    };
    assert_eq!(
        wait_condition_text(&unmatched),
        [
            "work: no match after 3.0 s: /tmp/x.log; call `agora work wait --log /tmp/x.log PATTERN` to keep waiting"
        ]
    );
}

/// A timed-out wait result for `condition` with the given targets and pending items.
fn timed_out_wait(condition: &str, targets: &[&str], pending: &[&str]) -> WaitResult {
    WaitResult {
        status: 124,
        timed_out: true,
        condition: condition.to_owned(),
        targets: targets.iter().map(|s| (*s).to_owned()).collect(),
        pending: pending.iter().map(|s| (*s).to_owned()).collect(),
        seconds: 2.0,
        tail: vec![],
        tasks: vec![],
        cursor: None,
    }
}

#[test]
fn timed_out_pid_and_file_waits_name_their_resume_command() {
    assert_eq!(
        wait_condition_text(&timed_out_wait("pid", &["77"], &["77"])),
        ["work: still running after 2.0 s: 77; call `agora work wait --pid 77` to keep waiting"]
    );
    assert_eq!(
        wait_condition_text(&timed_out_wait(
            "file",
            &["/tmp/r.status"],
            &["/tmp/r.status"]
        )),
        [
            "work: not ready after 2.0 s: /tmp/r.status; call `agora work wait --file /tmp/r.status` to keep waiting"
        ]
    );
}

#[test]
fn timed_out_task_all_card_message_and_notice_waits_say_to_run_the_same_wait_again() {
    for condition in ["tasks", "all", "cards", "messages", "notices"] {
        let lines = wait_condition_text(&timed_out_wait(condition, &["task-1"], &["task-1"]));
        assert_eq!(
            lines,
            ["work: still waiting after 2.0 s: task-1; run the same wait again to keep waiting"],
            "condition {condition}"
        );
        assert!(
            !lines[0].contains("agora work wait"),
            "condition {condition}"
        );
    }
}
