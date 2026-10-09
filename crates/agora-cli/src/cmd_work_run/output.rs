//! Output of `agora work run` and `agora work wait`: the task report and JSON document, the condition report, the
//! log tails, and the start and capacity-wait announcements.

use crate::CliError;
use agora_client::api::work_text;
use agora_sched::exit::{EXIT_TIMEOUT, wait_exit};
use agora_sched::model::{TaskRecord, TaskState, WaitResult};
use agora_sched::pyfmt::printable;
use agora_sched::textfmt::brief;
use serde_json::{Value, json};
use std::path::Path;

const BRIEF_LIMIT: usize = 200;

/// Prints the task report (text or JSON) and returns the exit status for the task rows.
pub(super) fn finish_tasks(
    result: &WaitResult,
    any: bool,
    lines: usize,
    json: bool,
) -> Result<i32, CliError> {
    if result.tasks.is_empty() {
        return Err(CliError::UnexpectedReply);
    }
    let timed_out = result.timed_out;
    let tails = tails_for(&result.tasks, timed_out, lines);
    let rows: Vec<(TaskState, Option<i32>, Option<i64>)> = result
        .tasks
        .iter()
        .map(|task| (task.state, task.status, task.ended_ms))
        .collect();
    let status = wait_exit(&rows, timed_out, any);
    if json {
        print_json(&json_document(status, timed_out, &result.tasks, &tails));
    } else {
        for line in work_text::report_text(
            &result.tasks,
            &tails,
            timed_out,
            agora_client::ops::now_ms(),
        ) {
            println!("{line}");
        }
    }
    Ok(status)
}

/// Prints a condition wait (text or the `WaitResult` JSON) and returns its exit status: 124 on timeout, else the
/// status the daemon gave (a file's number, or 0).
pub(super) fn finish_condition(result: &mut WaitResult, lines: usize, json: bool) -> i32 {
    if result.condition == "log"
        && let Some(path) = result.targets.first()
    {
        result.tail = agora_run::jobdir::tail(Path::new(path), lines);
    }
    result.status = if result.timed_out {
        EXIT_TIMEOUT
    } else {
        result.status
    };
    if json {
        match serde_json::to_value(&*result) {
            Ok(value) => print_json(&value),
            Err(_) => println!("{{\"status\": {}}}", result.status),
        }
    } else {
        for line in work_text::wait_condition_text(result) {
            println!("{line}");
        }
    }
    result.status
}

/// The last `lines` log lines of each task that is final or whose wait timed out; empty for the others.
pub(super) fn tails_for(tasks: &[TaskRecord], timed_out: bool, lines: usize) -> Vec<Vec<String>> {
    tasks
        .iter()
        .map(|task| {
            if !(timed_out || task.state.is_final()) {
                return Vec::new();
            }
            match task.log.as_deref() {
                Some(log) => agora_run::jobdir::tail(Path::new(log), lines),
                None => Vec::new(),
            }
        })
        .collect()
}

/// `{"status", "timed_out", "tasks": [task + "tail"]}` (ref-queue-cli section 4).
pub(super) fn json_document(
    status: i32,
    timed_out: bool,
    tasks: &[TaskRecord],
    tails: &[Vec<String>],
) -> Value {
    let rows: Vec<Value> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let mut row = serde_json::to_value(task).unwrap_or_default();
            if let Some(object) = row.as_object_mut() {
                object.insert(
                    "tail".to_owned(),
                    json!(tails.get(index).cloned().unwrap_or_default()),
                );
            }
            row
        })
        .collect();
    json!({"status": status, "timed_out": timed_out, "tasks": rows})
}

fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{value}"),
    }
}

/// `work: waiting for a b`, with ` (the first to end)` for `--any` over several ids.
pub(super) fn waiting_line(ids: &[String], any: bool) -> String {
    let names: Vec<String> = ids.iter().map(|id| printable(id)).collect();
    let suffix = if any && names.len() > 1 {
        " (the first to end)"
    } else {
        ""
    };
    format!("work: waiting for {}{suffix}", names.join(" "))
}

/// Prints each announcement of a run once: the process start, or the capacity wait before the process exists.
#[derive(Default)]
pub(super) struct Announcer {
    started: bool,
    waiting: bool,
}

impl Announcer {
    pub(super) fn observe(&mut self, task: &TaskRecord) -> Option<String> {
        match task.pid {
            Some(pid) if !self.started => {
                self.started = true;
                Some(started_line(task, pid))
            }
            Some(_) => None,
            None if !self.waiting && waits_for_capacity(task) => {
                self.waiting = true;
                Some(waits_line(task))
            }
            None => None,
        }
    }
}

fn waits_for_capacity(task: &TaskRecord) -> bool {
    task.state == TaskState::Claimed && task.requested_ms.is_some() && !task.note.is_empty()
}

fn started_line(task: &TaskRecord, pid: i64) -> String {
    let job = match task.command.as_deref() {
        Some(argv) => format!("{} ({})", printable(&task.id), brief(argv, BRIEF_LIMIT)),
        None => printable(&task.id),
    };
    let log = printable(task.log.as_deref().unwrap_or("-"));
    format!("work: started job {job} as process {pid}; output in {log}")
}

fn waits_line(task: &TaskRecord) -> String {
    format!(
        "work: job {} waits for capacity: {}",
        printable(&task.id),
        printable(&task.note)
    )
}
