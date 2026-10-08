//! `work.update`: reprioritise, edit, assign or unassign a task (ref-queue-cli sections 1 and 2; D4).

use super::checks::{
    MAX_ITEMS, TITLE_MAX, check_text_field, event, forbidden, load_task, may_act, refusal, refuse,
    stored_list,
};
use super::queue::claim_block;
use crate::StoreError;
use crate::identity::unhex32;
use agora_core::Role;
use agora_sched::claims;
use agora_sched::graph::Graph;
use agora_sched::model::{TaskRecord, TaskState};
use agora_sched::model_ops::UpdateArgs;
use agora_sched::task::Task;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;

fn finite_positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// Applies `args` to task `id`. Edits need `may_act`; assignment needs a supervisor. Returns the changed fields.
pub(super) fn update_in(
    tx: &Transaction,
    now: i64,
    actor: &str,
    role: Role,
    id: &str,
    args: &UpdateArgs,
) -> Result<Vec<&'static str>, StoreError> {
    let task = load_task(tx, id)?;
    if task.state.is_final() {
        return Err(refusal(format!(
            "task {id} is {} already",
            task.describe_state()
        )));
    }
    if matches!(task.state, TaskState::Running) {
        return Err(refusal(format!(
            "task {id} is {}; a running task cannot be changed",
            task.describe_state()
        )));
    }
    may_act(role, actor, &task)?;
    let mut changed: Vec<&'static str> = Vec::new();
    let mut title = task.title.clone();
    let mut kind = task.kind.clone();
    let mut size = task.size;
    let mut value = task.value;
    let mut after = task.after.clone();
    let mut state = task.state;
    let mut agent = task.agent.clone();
    let mut claimed = task.claimed_ms;
    let mut accepted = task.accepted_by.clone();
    if let Some(text) = &args.title {
        check_text_field("title", text, TITLE_MAX)?;
        title = text.clone();
        changed.push("title");
    }
    if let Some(name) = &args.kind {
        refuse(claims::validate_kind(name))?;
        kind = name.clone();
        changed.push("kind");
    }
    if let Some(v) = args.size {
        if !finite_positive(v) {
            return Err(StoreError::Invalid("size must be a number above zero"));
        }
        size = v;
        changed.push("size");
    }
    if let Some(v) = args.value {
        if !finite_positive(v) {
            return Err(StoreError::Invalid("value must be a number above zero"));
        }
        value = v;
        changed.push("value");
    }
    if let Some(list) = &args.after {
        after = validate_after(tx, id, list)?;
        check_cycles(tx, id, &after)?;
        changed.push("after");
    }
    match &args.agent {
        None => {}
        Some(None) => {
            // Unassign: the claimant (or a supervisor, by may_act) returns a claimed task that is not requested.
            if !matches!(state, TaskState::Claimed) || task.requested_ms.is_some() {
                return Err(refusal(format!(
                    "task {id} is {}; only a claimed task can be unassigned",
                    task.describe_state()
                )));
            }
            state = TaskState::Queued;
            agent = None;
            claimed = None;
            changed.push("agent");
        }
        Some(Some(pk)) => {
            if role < Role::Supervisor {
                return Err(forbidden(role));
            }
            let mut next = task.clone();
            next.after = after.clone();
            (state, agent, claimed) = assign(tx, now, &next, pk)?;
            changed.push("agent");
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    // An acceptance is of the command for one agent: once the agent changes, the next one accepts again.
    if agent != task.agent {
        accepted = None;
    }
    let after_json = serde_json::to_string(&after)
        .map_err(|_| StoreError::Invalid("prerequisites could not be stored"))?;
    tx.execute(
        "UPDATE work_tasks SET title=?1, kind=?2, size=?3, value=?4, after=?5, state=?6, agent=?7, claimed=?8, \
         accepted=?9 WHERE id=?10",
        params![
            title,
            kind,
            size,
            value,
            after_json,
            state.as_str(),
            agent,
            claimed,
            accepted,
            id
        ],
    )?;
    event(
        tx,
        now,
        actor,
        "work.updated",
        id,
        json!({
            "changed": changed,
            "scope": task.scope,
            "detail": format!("changed {}", changed.join(", ")),
        }),
    )?;
    Ok(changed)
}

/// Assigns the task to the identity `pk` (hex): a queued task is claimed for it under the claim checks, a claimed one
/// moves to it. A task already waiting for capacity or running cannot move. Returns (state, agent, claimed).
fn assign(
    tx: &Transaction,
    now: i64,
    task: &TaskRecord,
    pk: &str,
) -> Result<(TaskState, Option<String>, Option<i64>), StoreError> {
    let Some(key) = unhex32(pk) else {
        return Err(StoreError::Invalid(
            "agent must be a 64-character hex public key",
        ));
    };
    // The target must be a registered, non-revoked, non-system identity whose role is at least a worker.
    let row: Option<(String, bool, bool)> = tx
        .query_row(
            "SELECT role, revoked, system FROM identities WHERE pubkey=?",
            [key.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let is_worker = match row {
        Some((role, false, false)) => Role::parse(&role).is_some_and(|r| r >= Role::Worker),
        _ => false,
    };
    if !is_worker {
        return Err(StoreError::Invalid(
            "agent is not an active worker identity",
        ));
    }
    if task.requested_ms.is_some() {
        return Err(refusal(format!(
            "task {} is {}; cancel it instead",
            task.id,
            task.describe_state()
        )));
    }
    if matches!(task.state, TaskState::Queued) {
        claim_block(tx, now, task, pk)?;
    }
    Ok((TaskState::Claimed, Some(pk.to_string()), Some(now)))
}

/// Every prerequisite must exist, be listed once, and be named validly.
fn validate_after(tx: &Transaction, id: &str, list: &[String]) -> Result<Vec<String>, StoreError> {
    let mut after: Vec<String> = Vec::with_capacity(list.len());
    for item in list {
        if !after.contains(item) {
            after.push(item.clone());
        }
    }
    if after.len() > MAX_ITEMS {
        return Err(StoreError::Invalid("more than 1000 prerequisites"));
    }
    for item in &after {
        refuse(claims::validate_name("--after", item))?;
        let found: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_tasks WHERE id=?)",
            [item],
            |r| r.get(0),
        )?;
        if !found {
            return Err(refusal(format!("task {id} is after unknown task {item}")));
        }
    }
    Ok(after)
}

/// Builds the dependency graph of every task with `id`'s new prerequisites; a cycle or a self-edge is refused with
/// the graph's own message.
fn check_cycles(tx: &Transaction, id: &str, after: &[String]) -> Result<(), StoreError> {
    let rows: Vec<(String, String)> = {
        let mut stmt = tx.prepare("SELECT id, after FROM work_tasks ORDER BY seq")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    let mut tasks = Vec::with_capacity(rows.len());
    for (index, (task_id, raw)) in rows.into_iter().enumerate() {
        let list = if task_id == id {
            after.to_vec()
        } else {
            stored_list(&raw)?
        };
        tasks.push(Task {
            index,
            id: task_id,
            title: String::new(),
            kind: String::new(),
            tier: 0,
            queued: None,
            after: list,
            size: 1.0,
            value: 1.0,
            files: Vec::new(),
        });
    }
    Graph::new(tasks)
        .map(|_| ())
        .map_err(|e| StoreError::Refused(e.0))
}
