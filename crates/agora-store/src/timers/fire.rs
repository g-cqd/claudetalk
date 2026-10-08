//! The effect of `timer.fire`: enqueue a timer's template as a task authored by its owner, then move the timer to its
//! next fire. Runs inside the caller's transaction.

use super::template::card_owner;
use super::{exists, load_task, stored};
use crate::identity::unhex32;
use crate::{Store, StoreError};
use agora_core::Role;
use agora_sched::model::{TaskRecord, TimerRecord};
use agora_sched::timer;
use rusqlite::{OptionalExtension, Transaction, params};

/// Fires one enabled timer. Returns `None`, with the timer disabled and nothing enqueued, when its owner is revoked,
/// unknown, or no longer a Worker. `system` is the system identity's hex key, the actor of the events written here.
pub(super) fn fire_timer(
    tx: &Transaction,
    now: i64,
    system: &str,
    timer: TimerRecord,
    fire_ms: i64,
) -> Result<Option<TaskRecord>, StoreError> {
    let pk = unhex32(&timer.owner).ok_or(StoreError::Invalid("corrupt timer owner in database"))?;
    let owner_role: Option<String> = tx
        .query_row(
            "SELECT role FROM identities WHERE pubkey=? AND revoked=0",
            [pk.as_slice()],
            |r| r.get(0),
        )
        .optional()?;
    let owner_role = owner_role
        .and_then(|role| Role::parse(&role))
        .filter(|role| *role >= Role::Worker);
    let Some(owner_role) = owner_role else {
        tx.execute(
            "UPDATE timers SET enabled=0, next_fire=NULL WHERE id=?",
            [timer.id.as_str()],
        )?;
        Store::append(
            tx,
            now,
            system,
            "timer.disabled",
            &timer.id,
            &serde_json::json!({"reason": "owner is revoked, unknown or no longer a worker"}),
        )?;
        return Ok(None);
    };

    let task_id = next_task_id(tx, &timer.id, u64::from(timer.fired) + 1)?;
    let origin = format!("timer:{}", timer.id);
    // A reference the template names that no longer resolves is dropped, and the task's note says why.
    let mut notes: Vec<String> = Vec::new();
    let template = timer.template;
    let scope = match template.scope {
        Some(scope) => {
            if exists(
                tx,
                "SELECT EXISTS(SELECT 1 FROM work_scopes WHERE id=?)",
                &scope,
            )? {
                Some(scope)
            } else {
                notes.push(format!("scope {scope} no longer exists"));
                None
            }
        }
        None => None,
    };
    let mut after = Vec::new();
    for prerequisite in template.after {
        if exists(
            tx,
            "SELECT EXISTS(SELECT 1 FROM work_tasks WHERE id=?)",
            &prerequisite,
        )? {
            after.push(prerequisite);
        } else {
            notes.push(format!("prerequisite {prerequisite} no longer exists"));
        }
    }
    let card = match template.card {
        Some(card) => match card_owner(tx, &card)? {
            None => {
                notes.push(format!("card {card} no longer exists"));
                None
            }
            Some(owned_by) if owned_by.as_deref() == Some(timer.owner.as_str()) => Some(card),
            Some(_) if owner_role >= Role::Supervisor => Some(card),
            Some(_) => {
                notes.push(format!("card {card} is no longer owned by the timer owner"));
                None
            }
        },
        None => None,
    };
    let command = template
        .command
        .as_ref()
        .map(|argv| stored(serde_json::to_string(argv)))
        .transpose()?;
    let cwd = template.cwd.map(canonical_cwd);
    tx.execute(
        "INSERT INTO work_tasks(id,title,kind,scope,author,queued,after,size,value,files,command,cwd,weight,\
         state,note,origin,card,exclusive) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,'queued',?,?,?,0)",
        params![
            task_id,
            template.title,
            template.kind,
            scope,
            timer.owner,
            now,
            stored(serde_json::to_string(&after))?,
            template.size,
            template.value,
            stored(serde_json::to_string(&template.files))?,
            command,
            cwd,
            template.weight,
            notes.join("; "),
            origin,
            card,
        ],
    )?;
    Store::append(
        tx,
        now,
        system,
        "work.added",
        &task_id,
        &serde_json::json!({"origin": origin, "scope": scope, "detail": format!("timer {}", timer.id)}),
    )?;
    // Coalescing: the next fire is the first grid point after `now`, so missed fires never replay.
    let next = timer::next_fire(&timer.spec, fire_ms, now);
    tx.execute(
        "UPDATE timers SET enabled=?, next_fire=?, last_fire=?, fired=fired+1 WHERE id=?",
        params![next.is_some(), next, fire_ms, timer.id],
    )?;
    Store::append(
        tx,
        now,
        system,
        "timer.fired",
        &timer.id,
        &serde_json::json!({
            "task": task_id,
            "fire_ms": fire_ms,
            "scope": scope,
            "detail": format!("timer {}", timer.id),
        }),
    )?;
    load_task(tx, &task_id).map(Some)
}

/// The template's cwd with symlinks resolved, so the supervisor's canonical-cwd check accepts the fired job. Best
/// effort: a path that does not resolve (missing, or not UTF-8 once resolved) is kept as the template gave it.
fn canonical_cwd(cwd: String) -> String {
    std::fs::canonicalize(&cwd)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or(cwd)
}

/// `<timer>-<n>`, or `<timer>-<n>-<k>` for the first free k >= 2 when that id is taken.
fn next_task_id(conn: &rusqlite::Connection, timer: &str, n: u64) -> Result<String, StoreError> {
    let base = format!("{timer}-{n}");
    let mut candidate = base.clone();
    let mut k: u64 = 1;
    while exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM work_tasks WHERE id=?)",
        &candidate,
    )? {
        k = k.saturating_add(1);
        candidate = format!("{base}-{k}");
    }
    Ok(candidate)
}
