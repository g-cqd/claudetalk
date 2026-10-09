//! Checks shared by the work-queue operations: the role rule of D4, lookups, stored-row helpers, and the refusals
//! that repeat across operations. Dynamic work.py texts are `StoreError::Refused`; fixed texts are `Invalid`.

use crate::work_query::{TASK_COLUMNS, task_from_row};
use crate::{SignedOp, Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_sched::claims;
use agora_sched::model::{TaskRecord, TaskState};
use agora_sched::settings::WorkSettings;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Longest free text (note, directory, file path, scope task) in characters (work.py MAX_TEXT).
pub(super) const TEXT_MAX: usize = 4000;
/// Longest title in characters (work.py `--title` allows 1000).
pub(super) const TITLE_MAX: usize = 1000;
/// Files of one task, prerequisites of one task (work.py MAX_ITEMS).
pub(super) const MAX_ITEMS: usize = 1000;

pub(super) fn malformed() -> StoreError {
    StoreError::Invalid("malformed arguments")
}

/// A stored JSON column that does not parse: a database fault, reported as an internal error.
pub(super) fn corrupt() -> StoreError {
    StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "corrupt work queue row",
    ))
}

pub(super) fn stored_list(raw: &str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(raw).map_err(|_| corrupt())
}

/// A validator's message from agora_sched is already the user-facing text.
pub(super) fn refuse<T>(result: Result<T, String>) -> Result<T, StoreError> {
    result.map_err(StoreError::Refused)
}

pub(super) fn check_text_field(what: &str, value: &str, limit: usize) -> Result<(), StoreError> {
    refuse(claims::check_text(what, value, limit))
}

pub(super) fn forbidden(role: Role) -> StoreError {
    StoreError::Transition(TransitionError::Forbidden(role))
}

pub(super) fn refusal(message: String) -> StoreError {
    StoreError::Refused(message)
}

/// One task as the shared reader maps it. Works on a transaction (derefs to its connection) or a plain connection.
pub(super) fn load_task(conn: &Connection, id: &str) -> Result<TaskRecord, StoreError> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM work_tasks WHERE id=?");
    conn.query_row(&sql, [id], task_from_row)
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
}

pub(super) fn task_exists(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_tasks WHERE id=?)",
        [id],
        |r| r.get(0),
    )?)
}

/// `unknown scope` refusal unless the scope exists.
pub(super) fn scope_exists(conn: &Connection, scope: &str) -> Result<(), StoreError> {
    let found: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_scopes WHERE id=?)",
        [scope],
        |r| r.get(0),
    )?;
    if found {
        Ok(())
    } else {
        Err(refusal(format!(
            "unknown scope {scope}; create it with `agora work scope set {scope} --kind project`"
        )))
    }
}

/// A card a task is linked to must exist, and the actor must own it or be a supervisor (D7).
pub(super) fn check_card(
    conn: &Connection,
    card: &str,
    actor: &str,
    role: Role,
) -> Result<(), StoreError> {
    let owner: Option<Option<String>> = conn
        .query_row("SELECT owner FROM cards WHERE id=?", [card], |r| r.get(0))
        .optional()?;
    match owner {
        None => Err(StoreError::NotFound(format!("card {card}"))),
        Some(owner) if role >= Role::Supervisor || owner.as_deref() == Some(actor) => Ok(()),
        Some(_) => Err(forbidden(role)),
    }
}

/// D4 for acting on one task: a supervisor (or above) may act on any task; a worker only on its own, which is the
/// author while the task is queued and the claimant once it is claimed. Otherwise `Forbidden(role)`.
pub(super) fn may_act(role: Role, actor: &str, task: &TaskRecord) -> Result<(), StoreError> {
    if role >= Role::Supervisor {
        return Ok(());
    }
    let own = match task.state {
        TaskState::Queued => task.author == actor,
        _ => task.agent.as_deref() == Some(actor),
    };
    if own { Ok(()) } else { Err(forbidden(role)) }
}

/// The lowest role that may run or accept a command (settings `run_role`). A name that is not a role fails closed to
/// owner, so a damaged setting never widens who may run.
fn run_role_of(settings: &WorkSettings) -> Role {
    Role::parse(&settings.run_role).unwrap_or(Role::Owner)
}

/// S-1: the actor's role must reach `run_role` to run or accept a command, else `Forbidden(role)`.
pub(super) fn may_run(role: Role, settings: &WorkSettings) -> Result<(), StoreError> {
    if role >= run_role_of(settings) {
        Ok(())
    } else {
        Err(forbidden(role))
    }
}

/// The checks of a run request on `task` that the role rule does not cover. `named` is true for a task the caller
/// names, false for an inline job the caller just created as its author (which is accepted by design).
/// - S-1: a named task whose author may no longer accept it (`self_accept` was switched off after the accept).
/// - S-5: the actor already has `max_waiting` jobs claimed, requested and not yet started (not counting `task`).
pub(super) fn check_request(
    conn: &Connection,
    op: &SignedOp,
    actor: &str,
    task: &TaskRecord,
    named: bool,
    settings: &WorkSettings,
) -> Result<(), StoreError> {
    if named && task.author == actor && !settings.self_accept {
        return Err(refusal(
            "a command must be accepted by an identity other than its author".to_string(),
        ));
    }
    let waiting: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_tasks WHERE agent=?1 AND state='claimed' AND requested IS NOT NULL \
         AND started IS NULL AND id<>?2",
        params![actor, task.id],
        |r| r.get(0),
    )?;
    if waiting >= i64::from(settings.max_waiting) {
        return Err(refusal(format!(
            "{} already has {waiting} jobs waiting to start",
            agora_crypto::pseudonym(&op.actor)
        )));
    }
    Ok(())
}

/// S-8: the number of path components of a canonical absolute path ("/" has none, "/tmp" one, "/tmp/x" two).
pub(crate) fn path_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

/// The subject of a task operation is the task id, which must be NAME-valid.
pub(super) fn subject_task(subject: &str) -> Result<&str, StoreError> {
    refuse(claims::validate_name("task", subject))?;
    Ok(subject)
}

/// D5 digest of a task's command, or None for a task without one.
pub(super) fn digest_of(task: &TaskRecord) -> Option<String> {
    let argv = task.command.as_ref()?;
    Some(claims::digest(
        argv,
        task.cwd.as_deref().unwrap_or(""),
        &task.files,
    ))
}

pub(super) fn agent_label(task: &TaskRecord) -> String {
    task.agent_name.clone().unwrap_or_default()
}

/// The `detail` datum of an event: `text` when it has some, else `fallback` (the task's title or id), so every
/// work event reads as a line of human text in `work log`.
pub(super) fn detail_text(text: &str, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_string()
    } else {
        text.to_string()
    }
}

/// work.py `check_runnable`: whether `actor` may run the task's command now (exit-1 refusals, same order).
pub(super) fn check_runnable(task: &TaskRecord, actor: &str) -> Result<(), StoreError> {
    let id = task.id.as_str();
    let desc = task.describe_state();
    if task.command.is_none() {
        return Err(refusal(format!(
            "task {id} has no command; close it with `agora work done {id}` when it is finished"
        )));
    }
    if matches!(task.state, TaskState::Queued) {
        return Err(refusal(format!(
            "claim task {id} first: `agora work claim {id}`"
        )));
    }
    let waiting = matches!(task.state, TaskState::Claimed) && task.requested_ms.is_some();
    if matches!(task.state, TaskState::Running) || waiting {
        return Err(refusal(format!(
            "task {id} is already {desc}; wait for it with `agora work wait {id}`"
        )));
    }
    if !matches!(task.state, TaskState::Claimed) {
        return Err(refusal(format!("task {id} is {desc}")));
    }
    if task.agent.as_deref() != Some(actor) {
        return Err(refusal(format!(
            "task {id} is claimed by {}; only that agent runs it",
            agent_label(task)
        )));
    }
    if task.accepted_by.as_deref() != Some(actor) {
        return Err(refusal(format!(
            "task {id} runs a command that {} queued, and you have not accepted it: read it with `agora work show {id}`, \
             then accept it with the `agora work accept {id} --digest ...` line that show prints",
            task.author_name
        )));
    }
    Ok(())
}

/// Appends one event in the caller's transaction (kind past tense, small JSON data).
pub(super) fn event(
    tx: &Transaction,
    now: i64,
    actor: &str,
    kind: &str,
    subject: &str,
    data: serde_json::Value,
) -> Result<(), StoreError> {
    Store::append(tx, now, actor, kind, subject, &data)?;
    Ok(())
}
