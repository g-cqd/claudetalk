//! Claims: prerequisites, file holders, the claim check, and the `next` selection (ref-queue-cli sections 2 and 3).

use super::checks::{detail_text, event, load_task, refusal, stored_list};
use crate::StoreError;
use crate::work_query::{TASK_COLUMNS, task_from_row};
use agora_sched::claims::first_conflict;
use agora_sched::graph::Graph;
use agora_sched::model::{NextOutcome, TaskRecord};
use agora_sched::order::{OrderPolicy, order};
use agora_sched::task::{Queued, Task, tier_of};
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::HashSet;

/// Prerequisites that are not done yet, as (id, state). A missing prerequisite reads "unknown".
pub(super) fn waiting_in(
    tx: &Transaction,
    after: &[String],
) -> Result<Vec<(String, String)>, StoreError> {
    let mut waiting = Vec::new();
    for item in after {
        let state: Option<String> = tx
            .query_row("SELECT state FROM work_tasks WHERE id=?", [item], |r| {
                r.get(0)
            })
            .optional()?;
        match state {
            Some(state) if state == "done" => {}
            Some(state) => waiting.push((item.clone(), state)),
            None => waiting.push((item.clone(), "unknown".to_string())),
        }
    }
    Ok(waiting)
}

/// (holder, files) for every claimed or running task with files, then every active path lease as
/// ("lease:<id>", [path]). A lease held by `exclude` does not count; tasks always count.
pub(super) fn holders_in(
    tx: &Transaction,
    now: i64,
    exclude: Option<&str>,
) -> Result<Vec<(String, Vec<String>)>, StoreError> {
    let mut holders = Vec::new();
    let mut tasks = tx.prepare(
        "SELECT id, files FROM work_tasks WHERE state IN ('claimed','running') AND files <> '[]' ORDER BY seq",
    )?;
    for row in tasks.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (id, files) = row?;
        holders.push((id, stored_list(&files)?));
    }
    let mut leases = tx.prepare(
        "SELECT id, resource FROM leases WHERE kind='path' AND released IS NULL \
         AND (expires IS NULL OR expires > ?1) AND (?2 IS NULL OR holder <> ?2) ORDER BY created, id",
    )?;
    for row in leases.query_map(params![now, exclude], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (id, resource) = row?;
        holders.push((format!("lease:{id}"), vec![resource]));
    }
    Ok(holders)
}

/// Refuses when `files` overlap a holder's files; `subject` names what would change them ("task T", "job J").
pub(super) fn check_files(
    tx: &Transaction,
    now: i64,
    subject: &str,
    files: &[String],
    exclude: &str,
) -> Result<(), StoreError> {
    let holders = holders_in(tx, now, Some(exclude))?;
    match first_conflict(files, &holders) {
        None => Ok(()),
        Some((holder, _)) if holder.starts_with("lease:") => Err(refusal(format!(
            "{subject} changes files that {holder} holds"
        ))),
        Some((holder, _)) => {
            let held = load_task(tx, holder)?;
            Err(refusal(format!(
                "{subject} changes files that task {holder} holds ({})",
                held.describe_state()
            )))
        }
    }
}

/// The checks of `claim`: no unfinished prerequisite, no file held by someone else.
pub(super) fn claim_block(
    tx: &Transaction,
    now: i64,
    task: &TaskRecord,
    actor: &str,
) -> Result<(), StoreError> {
    let waiting = waiting_in(tx, &task.after)?;
    if !waiting.is_empty() {
        let list: Vec<String> = waiting
            .iter()
            .map(|(id, state)| format!("{id} ({state})"))
            .collect();
        return Err(refusal(format!(
            "task {} waits for {}",
            task.id,
            list.join(", ")
        )));
    }
    check_files(tx, now, &format!("task {}", task.id), &task.files, actor)
}

/// Moves a queued task to claimed by `actor`, with its event. A claim is a new agent, so any acceptance left on the
/// task is cleared: the claimant accepts the command again.
pub(super) fn take(tx: &Transaction, now: i64, id: &str, actor: &str) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE work_tasks SET state='claimed', agent=?1, claimed=?2, accepted=NULL WHERE id=?3 AND state='queued'",
        params![actor, now, id],
    )?;
    if changed != 1 {
        return Err(StoreError::Refused(format!(
            "task {id} is no longer queued"
        )));
    }
    let (scope, title): (Option<String>, String) = tx.query_row(
        "SELECT scope, title FROM work_tasks WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    event(
        tx,
        now,
        actor,
        "work.claimed",
        id,
        serde_json::json!({"scope": scope, "detail": detail_text(&title, id)}),
    )
}

/// Queued tasks of a scope (with its descendants), by arrival. A task without a scope is invisible under a filter.
fn queued_in_scope(tx: &Transaction, scope: Option<&str>) -> Result<Vec<TaskRecord>, StoreError> {
    let sql = format!(
        "SELECT {TASK_COLUMNS} FROM work_tasks WHERE state='queued' AND (?1 IS NULL OR scope IN (\
         WITH RECURSIVE under(id) AS (SELECT ?1 UNION SELECT s.id FROM work_scopes s JOIN under ON s.parent = under.id) \
         SELECT id FROM under)) ORDER BY seq"
    );
    let mut stmt = tx.prepare(&sql)?;
    let rows = stmt.query_map([scope], task_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// work.py `next`: order the queued tasks of the scope, skip the blocked and the file-conflicting ones, claim the
/// first that is left. Nothing claimable is `Ok` with `claimed: None` and the reason.
pub(super) fn next_in(
    tx: &Transaction,
    now: i64,
    actor: &str,
    scope: Option<&str>,
    policy: OrderPolicy,
) -> Result<NextOutcome, StoreError> {
    let queued = queued_in_scope(tx, scope)?;
    let where_ = scope.map(|s| format!(" in scope {s}")).unwrap_or_default();
    if queued.is_empty() {
        return Ok(NextOutcome {
            claimed: None,
            reason: format!("nothing is queued{where_}"),
        });
    }
    let ids: HashSet<&str> = queued.iter().map(|t| t.id.as_str()).collect();
    let mut tasks = Vec::with_capacity(queued.len());
    let mut blocked_flags = Vec::with_capacity(queued.len());
    for (index, task) in queued.iter().enumerate() {
        blocked_flags.push(!waiting_in(tx, &task.after)?.is_empty());
        tasks.push(Task {
            index,
            id: task.id.clone(),
            title: task.title.clone(),
            kind: task.kind.clone(),
            tier: tier_of(&task.kind),
            queued: Some(Queued::Int(task.queued_ms)),
            after: task
                .after
                .iter()
                .filter(|item| ids.contains(item.as_str()))
                .cloned()
                .collect(),
            size: task.size,
            value: task.value,
            files: task.files.clone(),
        });
    }
    let graph = Graph::new(tasks)
        .map_err(|e| StoreError::Refused(format!("the queue cannot be ordered: {}", e.0)))?;
    let holders = holders_in(tx, now, Some(actor))?;
    let (mut blocked, mut held_files) = (0usize, 0usize);
    let mut chosen = None;
    for index in order(&graph, policy).order {
        let Some(task) = graph.tasks.get(index) else {
            continue;
        };
        if blocked_flags.get(index).copied().unwrap_or(true) {
            blocked += 1;
        } else if first_conflict(&task.files, &holders).is_some() {
            held_files += 1;
        } else {
            chosen = Some(task.id.clone());
            break;
        }
    }
    let Some(id) = chosen else {
        return Ok(NextOutcome {
            claimed: None,
            reason: format!(
                "of {} queued task(s){where_}, {blocked} wait for their prerequisites and {held_files} change files \
                 that claimed or running tasks hold",
                queued.len()
            ),
        });
    };
    take(tx, now, &id, actor)?;
    let claimed = load_task(tx, &id)?;
    Ok(NextOutcome {
        claimed: Some(claimed),
        reason: String::new(),
    })
}
