//! What a wait reports beyond its status: the tail of a task's log (`WaitResult.tail`) and the newest notice seq a
//! Notices wait with a negative `after` starts after.

use super::rules::{Caller, may_read_tail, tail_candidate};
use crate::{DaemonError, Shared};
use agora_run::jobdir;
use agora_sched::model::TaskRecord;
use agora_store::{Store, StoreError};

/// Log lines a task wait reports.
const TAIL_LINES: usize = 20;
/// Notices read per page while the newest seq is searched for (the store caps a page at 500).
const NOTICE_PAGE: u32 = 500;

/// The last lines of the log of the task `tail_candidate` picks (see there), read on the blocking pool, and only when
/// `caller` may read that task's tail (`may_read_tail`); the log of any other task is not read at all. Empty when no
/// task qualifies, the caller may not read it, or its log is missing.
pub(super) async fn task_tail(
    sh: &Shared,
    tasks: Vec<TaskRecord>,
    timed_out: bool,
    caller: Caller,
) -> Result<Vec<String>, DaemonError> {
    let root = jobdir::work_root(&sh.config.home);
    tokio::task::spawn_blocking(move || {
        let has_log = |id: &str| jobdir::log_path(&root, id).exists();
        match tail_candidate(&tasks, timed_out, has_log) {
            Some(task) if may_read_tail(task, &caller) => {
                jobdir::tail(&jobdir::log_path(&root, &task.id), TAIL_LINES)
            }
            _ => Vec::new(),
        }
    })
    .await
    .map_err(|e| DaemonError::Io(std::io::Error::other(e)))
}

/// The seq of the newest notice for `key` (hex target), or 0 when it has none. Read a page at a time, oldest first.
pub(super) fn latest_notice_seq(s: &Store, key: &str) -> Result<i64, StoreError> {
    let mut latest = 0;
    loop {
        let rows = s.notices(key, latest, NOTICE_PAGE)?;
        let Some(last) = rows.last() else {
            return Ok(latest);
        };
        latest = last.seq;
        if rows.len() < NOTICE_PAGE as usize {
            return Ok(latest);
        }
    }
}
