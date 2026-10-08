//! Request validation and the status rules of the conditions: task outcomes, card states, log patterns, the numeric
//! status a file may name, the role gates of the filesystem and process waits and of the task-log tails, and the seq a
//! notices wait starts after.

use crate::DaemonError;
use agora_core::{CardState, Role, TransitionError};
use agora_sched::exit::wait_exit;
use agora_sched::model::{TaskRecord, TaskState};
use agora_store::StoreError;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

/// Bytes of a status file read by the numeric rule.
const STATUS_FILE_MAX: u64 = 4096;
/// Card states that end a `cards` wait when the request names none.
const DEFAULT_CARD_STATES: [&str; 4] = ["review", "done", "blocked", "cancelled"];
/// The most ids, pids, paths or card states one wait names, and the most ids a cursor carries.
pub(super) const MAX_TARGETS: usize = 256;
/// Bytes the compiled program and the lazy DFA of a log pattern may each take (the regex crate's default is 10 MiB).
const PATTERN_LIMIT: usize = 1 << 20;

/// Whether the tasks satisfy the wait: any one final when `any`, else every one final.
pub(super) fn task_done(tasks: &[TaskRecord], any: bool) -> bool {
    if any {
        tasks.iter().any(|t| t.state.is_final())
    } else {
        tasks.iter().all(|t| t.state.is_final())
    }
}

/// The exit status of a finished task wait (agora_sched::exit rules).
pub(super) fn task_status(tasks: &[TaskRecord], any: bool) -> i32 {
    let rows: Vec<(TaskState, Option<i32>, Option<i64>)> = tasks
        .iter()
        .map(|t| (t.state, t.status, t.ended_ms))
        .collect();
    wait_exit(&rows, false, any)
}

/// The ids of the tasks that are not final yet.
pub(super) fn non_final(tasks: &[TaskRecord]) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| !t.state.is_final())
        .map(|t| t.id.clone())
        .collect()
}

/// The task whose log a task wait reports: when the wait is done, the first final task in the given order; on a
/// timeout, the first task whose log exists (`has_log`). `None` when there is no such task.
pub(super) fn tail_candidate(
    tasks: &[TaskRecord],
    timed_out: bool,
    has_log: impl Fn(&str) -> bool,
) -> Option<&TaskRecord> {
    if timed_out {
        tasks.iter().find(|t| has_log(&t.id))
    } else {
        tasks.iter().find(|t| t.state.is_final())
    }
}

/// Card states to wait for: the request's list, or the default terminal-or-review set when it is empty. Unknown
/// names are refused.
pub(super) fn card_states(states: Vec<String>) -> Result<Vec<String>, DaemonError> {
    if states.is_empty() {
        return Ok(DEFAULT_CARD_STATES.iter().map(|s| s.to_string()).collect());
    }
    if let Some(bad) = states.iter().find(|s| CardState::parse(s).is_none()) {
        return Err(DaemonError::Store(StoreError::Refused(format!(
            "unknown card state {bad:?}"
        ))));
    }
    Ok(states)
}

/// A missing card is named as a card, not as a bare id.
pub(super) fn card_not_found(e: StoreError, id: &str) -> StoreError {
    match e {
        StoreError::NotFound(_) => StoreError::NotFound(format!("card {id}")),
        other => other,
    }
}

/// The log pattern as a regular expression; an invalid one, or one whose compiled program or lazy DFA would exceed
/// `PATTERN_LIMIT` bytes, is refused with the regex error.
pub(super) fn compile_pattern(pattern: &str) -> Result<regex::Regex, DaemonError> {
    regex::RegexBuilder::new(pattern)
        .size_limit(PATTERN_LIMIT)
        .dfa_size_limit(PATTERN_LIMIT)
        .build()
        .map_err(|e| {
            DaemonError::Store(StoreError::Refused(format!(
                "'{pattern}' is not a valid regular expression: {e}"
            )))
        })
}

/// The single distinct path of the list, if there is exactly one.
pub(super) fn sole_path(paths: &[String]) -> Option<&str> {
    let distinct: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    if distinct.len() == 1 {
        distinct.into_iter().next()
    } else {
        None
    }
}

/// The status a file names, read from its first 4096 bytes; 0 when it names none or cannot be read.
///
/// The file is opened `O_NONBLOCK` and must be a regular file once opened: opening a FIFO with no writer would
/// otherwise block the daemon's blocking pool on it, and reading a device could never end.
pub(super) fn file_status(path: &Path) -> i32 {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return 0;
    };
    let Ok(meta) = file.metadata() else {
        return 0;
    };
    if !meta.is_file() {
        return 0;
    }
    let mut buf = Vec::new();
    match Read::take(file, STATUS_FILE_MAX).read_to_end(&mut buf) {
        Ok(_) => numeric_status(&buf).unwrap_or(0),
        Err(_) => 0,
    }
}

/// The numeric status rule: the content, minus leading and trailing spaces and newlines, is a number of ASCII
/// digits (fewer than 12 of them). Values that fit an i32 are kept; larger ones are wrapped to their low byte,
/// as the OS wraps an exit status.
pub(super) fn numeric_status(content: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(content)
        .ok()?
        .trim_matches(|c| c == ' ' || c == '\n');
    if text.is_empty() || text.len() >= 12 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = text.parse().ok()?;
    Some(i32::try_from(n).unwrap_or_else(|_| i32::try_from(n % 256).unwrap_or(0)))
}

/// The seq a Notices wait starts after, when it is known without reading the store. A non-negative request names it.
/// A negative one (`-1`: "from now") keeps the seq its first call resolved, which the cursor stores; `None` means the
/// latest seq has to be read now.
pub(super) fn known_floor(after: i64, stored: Option<i64>) -> Option<i64> {
    if after >= 0 { Some(after) } else { stored }
}

/// The role gate of the waits that touch the daemon's filesystem or processes (pid, file, gone, log): Worker or more.
pub(super) fn need_worker(role: Role) -> Result<(), DaemonError> {
    if role < Role::Worker {
        return Err(DaemonError::Store(StoreError::Transition(
            TransitionError::Forbidden(Role::Worker),
        )));
    }
    Ok(())
}

/// The role gate of the filesystem waits (file, gone, log) on the work root: a path under `work_root` (job logs and
/// job records) is refused for a caller below Supervisor. A worker reads a job's log through its task wait, which
/// applies the author-or-claimant rule of `may_read_tail` (S-2), not through a path.
pub(super) fn refuse_job_paths(
    paths: &[String],
    role: Role,
    work_root: &Path,
) -> Result<(), DaemonError> {
    if role >= Role::Supervisor {
        return Ok(());
    }
    let root = resolve(work_root);
    if paths
        .iter()
        .any(|p| resolve(Path::new(p)).starts_with(&root))
    {
        return Err(DaemonError::Store(StoreError::Transition(
            TransitionError::Forbidden(Role::Supervisor),
        )));
    }
    Ok(())
}

/// `path` made absolute and lexically normalised (`.` dropped, each `..` removing the component before it), then with
/// its longest existing ancestor canonicalized, so a symlink in the part that exists counts as its target. A path that
/// does not exist yet resolves through its existing ancestor, so a file about to be created is judged where it lands.
fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |dir| dir.join(path))
    };
    let mut lexical = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    let mut existing = lexical.as_path();
    // The components below the existing ancestor, deepest first.
    let mut rest: Vec<&OsStr> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |mut acc, part| {
                acc.push(part);
                acc
            });
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return lexical,
        }
    }
}

/// Who a wait answers for: the caller's pubkey as hex (the form the store keeps authors and claimants in) and its role.
#[derive(Clone)]
pub(super) struct Caller {
    pub(super) key: String,
    pub(super) role: Role,
}

/// Whether the caller may read the log tail of `task` in a task wait: the task's author and its claimant may, at any
/// role, and so does a Supervisor or more. Anyone else gets no tail; the rest of the result is unchanged.
pub(super) fn may_read_tail(task: &TaskRecord, caller: &Caller) -> bool {
    caller.role >= Role::Supervisor
        || task.author == caller.key
        || task.agent.as_deref() == Some(caller.key.as_str())
}

/// Request lists longer than `MAX_TARGETS` are refused.
pub(super) fn within_targets(len: usize) -> Result<(), DaemonError> {
    if len > MAX_TARGETS {
        return Err(invalid("name at most 256 targets"));
    }
    Ok(())
}

/// Non-empty request lists: an empty list would be done at once, which is never what was asked.
pub(super) fn non_empty<T>(items: &[T], message: &'static str) -> Result<(), DaemonError> {
    if items.is_empty() {
        Err(invalid(message))
    } else {
        Ok(())
    }
}

/// A usage error with a fixed message.
pub(super) fn invalid(message: &'static str) -> DaemonError {
    DaemonError::Store(StoreError::Invalid(message))
}
