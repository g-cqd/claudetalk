//! Persisted timers (`at`, `every`, `cron`) that enqueue a task template when they fire.
//!
//! A timer is a row in `timers`: its schedule, its task template, and its next and last fire times. `timer.fire` is a
//! system operation (see `fire`): it enqueues the template as a task authored by the timer's owner, then moves the
//! timer to its next fire after now, so missed fires coalesce into one. The template checks are in `template`.

mod fire;
mod template;
#[cfg(test)]
mod tests;

use crate::identity::{Pub, authorize, need, unhex32};
use crate::system::authorize_system;
use crate::work_query::{TASK_COLUMNS, task_from_row};
use crate::{SignedOp, Store, StoreError};
use agora_core::Role;
use agora_crypto::pseudonym;
use agora_sched::claims::validate_name;
use agora_sched::model::{TaskRecord, TimerRecord};
use agora_sched::model_ops::{AddArgs, TimerArgs, TimerFireArgs};
use agora_sched::settings::WorkSettings;
use agora_sched::timer::{self, TimerSpec};
use rusqlite::{Connection, OptionalExtension, Params, params};

const TIMER_COLUMNS: &str =
    "id, owner, spec, template, enabled, next_fire, last_fire, fired, created";

/// Longest timer id, in characters. Task ids are `<timer>-<n>` and must stay within the 128-byte name limit.
const TIMER_ID_LIMIT: usize = 100;

/// A timer row as stored, in `TIMER_COLUMNS` order.
type Raw = (
    String,
    String,
    String,
    String,
    bool,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
);

impl Store {
    /// "timer.create": the quota comes from the work settings.
    pub fn timer_create(&mut self, now: i64, op: &SignedOp) -> Result<TimerRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "timer.create")?;
        need(role, Role::Worker)?;
        let args: TimerArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("timer arg must be a timer JSON object"))?;
        timer::validate(&args.spec).map_err(StoreError::Refused)?;
        let owner = op.actor_hex();
        let id = match args.id {
            Some(id) => {
                // Checked before the name rule, so a long id gets the length reason.
                if id.chars().count() > TIMER_ID_LIMIT {
                    return Err(StoreError::Invalid(
                        "timer id is longer than 100 characters",
                    ));
                }
                validate_name("timer id", &id).map_err(StoreError::Refused)?;
                id
            }
            None => next_timer_id(&tx)?,
        };
        if exists(&tx, "SELECT EXISTS(SELECT 1 FROM timers WHERE id=?)", &id)? {
            return Err(StoreError::Invalid("timer already exists"));
        }
        let checked = template::check_template(&tx, &owner, role, &args.template)?;
        let quota = timer_quota(&tx)?;
        let enabled: i64 = tx.query_row(
            "SELECT COUNT(*) FROM timers WHERE owner=? AND enabled=1",
            [owner.as_str()],
            |r| r.get(0),
        )?;
        if enabled >= i64::from(quota) {
            return Err(StoreError::Refused(format!(
                "timer quota reached: {enabled} of {quota} enabled timers"
            )));
        }
        let next = timer::first_fire(&args.spec, now)
            .ok_or_else(|| StoreError::Refused("the schedule never fires".to_string()))?;
        tx.execute(
            "INSERT INTO timers(id,owner,spec,template,enabled,next_fire,last_fire,fired,created) \
             VALUES (?,?,?,?,1,?,NULL,0,?)",
            params![
                id,
                owner,
                stored(serde_json::to_string(&args.spec))?,
                stored(serde_json::to_string(&checked))?,
                next,
                now
            ],
        )?;
        Self::append(
            &tx,
            now,
            &owner,
            "timer.created",
            &id,
            &serde_json::json!({"spec": timer::describe(&args.spec), "next_fire": next}),
        )?;
        let created =
            load_timer(&tx, &id)?.ok_or_else(|| StoreError::NotFound(format!("timer {id}")))?;
        tx.commit()?;
        Ok(created)
    }

    /// "timer.delete": owner or Supervisor+.
    pub fn timer_delete(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "timer.delete")?;
        let id = op.subject.as_str();
        let owner: String = tx
            .query_row("SELECT owner FROM timers WHERE id=?", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("timer {id}")))?;
        let actor = op.actor_hex();
        if owner != actor {
            need(role, Role::Supervisor)?;
        }
        tx.execute("DELETE FROM timers WHERE id=?", [id])?;
        Self::append(
            &tx,
            now,
            &actor,
            "timer.deleted",
            id,
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    /// Every timer, or only those owned by `owner` (a hex public key), oldest first.
    pub fn timers(&self, owner: Option<&str>) -> Result<Vec<TimerRecord>, StoreError> {
        match owner {
            None => timer_records(
                &self.conn,
                &format!("SELECT {TIMER_COLUMNS} FROM timers ORDER BY created, id"),
                params![],
            ),
            Some(owner) => timer_records(
                &self.conn,
                &format!("SELECT {TIMER_COLUMNS} FROM timers WHERE owner=? ORDER BY created, id"),
                params![owner],
            ),
        }
    }

    /// The earliest next fire of an enabled timer.
    pub fn next_timer_due(&self) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MIN(next_fire) FROM timers WHERE enabled=1",
            [],
            |r| r.get(0),
        )?)
    }

    /// Enabled timers whose next fire is at or before `now`, earliest first.
    pub fn due_timers(&self, now: i64) -> Result<Vec<TimerRecord>, StoreError> {
        timer_records(
            &self.conn,
            &format!(
                "SELECT {TIMER_COLUMNS} FROM timers WHERE enabled=1 AND next_fire<=? ORDER BY next_fire, id"
            ),
            params![now],
        )
    }

    /// "timer.fire" (system): enqueue the template as a task authored by the owner (id "<timer>-<fired+1>",
    /// origin "timer:<id>"), update last_fire/fired/next_fire (coalesced), disable `at` timers. Owner revoked → disable, no task.
    /// Acts only when the timer is enabled and its next fire is at or before `now`; otherwise it returns `None` and
    /// changes nothing.
    pub fn timer_fire(
        &mut self,
        now: i64,
        op: &SignedOp,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        authorize_system(&tx, now, op, "timer.fire")?;
        let fire: TimerFireArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("timer.fire arg must be a fire time JSON object"))?;
        let system = op.actor_hex();
        let task = match load_timer(&tx, &op.subject)? {
            Some(timer) if is_due(&timer, now) => {
                fire::fire_timer(&tx, now, &system, timer, fire.fire_ms)?
            }
            // Unknown, disabled, or not yet due: nothing to fire.
            _ => None,
        };
        tx.commit()?;
        Ok(task)
    }
}

/// Whether `timer.fire` may act on the timer: it is enabled and its next fire is at or before `now`.
fn is_due(timer: &TimerRecord, now: i64) -> bool {
    timer.enabled && timer.next_fire_ms.is_some_and(|next| next <= now)
}

/// `timer-<n>` for the first n above the number of timers that is free.
fn next_timer_id(conn: &Connection) -> Result<String, StoreError> {
    let mut n: i64 = conn.query_row("SELECT COUNT(*) FROM timers", [], |r| r.get(0))?;
    loop {
        n = n.saturating_add(1);
        let id = format!("timer-{n}");
        if !exists(conn, "SELECT EXISTS(SELECT 1 FROM timers WHERE id=?)", &id)? {
            return Ok(id);
        }
    }
}

/// The per-identity timer quota: the stored work settings, or the default when no row exists.
fn timer_quota(conn: &Connection) -> Result<u32, StoreError> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM work_settings WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    match json {
        None => Ok(WorkSettings::default().timer_quota),
        Some(json) => serde_json::from_str::<WorkSettings>(&json)
            .map(|settings| settings.timer_quota)
            .map_err(|_| StoreError::Invalid("corrupt work settings in database")),
    }
}

/// A value encoded for a JSON column. The encoding of these plain data types does not fail in practice; a failure
/// is still reported rather than stored as an empty column.
fn stored(json: serde_json::Result<String>) -> Result<String, StoreError> {
    json.map_err(|_| StoreError::Invalid("value cannot be stored as JSON"))
}

fn exists(conn: &Connection, sql: &str, key: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(sql, [key], |r| r.get(0))?)
}

fn raw_timer(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn timer_records<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<TimerRecord>, StoreError> {
    let mut statement = conn.prepare_cached(sql)?;
    let rows = statement.query_map(params, raw_timer)?;
    rows.map(|raw| timer_record(raw?)).collect()
}

fn load_timer(conn: &Connection, id: &str) -> Result<Option<TimerRecord>, StoreError> {
    Ok(timer_records(
        conn,
        &format!("SELECT {TIMER_COLUMNS} FROM timers WHERE id=?"),
        params![id],
    )?
    .pop())
}

/// Decodes a stored row. The owner's pseudonym is derived from its key here.
fn timer_record(raw: Raw) -> Result<TimerRecord, StoreError> {
    let (id, owner, spec, template, enabled, next_fire, last_fire, fired, created) = raw;
    let pk: Pub = unhex32(&owner).ok_or(StoreError::Invalid("corrupt timer owner in database"))?;
    Ok(TimerRecord {
        id,
        owner_name: pseudonym(&pk),
        owner,
        spec: serde_json::from_str::<TimerSpec>(&spec)
            .map_err(|_| StoreError::Invalid("corrupt timer spec in database"))?,
        template: serde_json::from_str::<AddArgs>(&template)
            .map_err(|_| StoreError::Invalid("corrupt timer template in database"))?,
        enabled,
        next_fire_ms: next_fire,
        last_fire_ms: last_fire,
        fired: u32::try_from(fired)
            .map_err(|_| StoreError::Invalid("corrupt fire count in database"))?,
        created_ms: created,
    })
}

/// The task with `id`, read back with the work queue's own mapping.
fn load_task(conn: &Connection, id: &str) -> Result<TaskRecord, StoreError> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM work_tasks WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], task_from_row)?)
}
