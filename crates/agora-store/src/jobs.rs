//! System operations on running jobs: start, finish (with card sync), notes, samples, retention.
//!
//! The daemon signs these with its system identity (D6). Each state-changing operation authorizes through
//! `authorize_system`, which also consumes the nonce, and runs in one IMMEDIATE transaction, so a refusal leaves
//! no partial change behind.

use crate::cards_ext::card_job_sync_tx;
use crate::system::authorize_system;
use crate::work_query::{TASK_COLUMNS, task_from_row};
use crate::{SignedOp, Store, StoreError};
use agora_sched::model::{TaskRecord, TaskState};
use agora_sched::model_ops::{JobFinishArgs, JobNoteArgs, JobStartArgs};
use rusqlite::{OptionalExtension, Transaction, params};

/// Statuses the supervisor reports when the command never ran: 126 not executable, 127 not found (D8). Such a run
/// leaves no `job_history` row, since its duration says nothing about the work.
const NOT_RUN_STATUSES: [i32; 2] = [126, 127];

/// The columns a job operation reads before it changes the row.
struct Current {
    state: String,
    cancel_requested: bool,
    requested: Option<i64>,
    started: Option<i64>,
    weight: f64,
    sig_full: Option<String>,
    sig_head: Option<String>,
    sig_prog: Option<String>,
    sig_tool: Option<String>,
    scope: Option<String>,
}

/// Reads the row inside the caller's transaction; `NotFound("task <id>")` when absent.
fn current(tx: &Transaction, id: &str) -> Result<Current, StoreError> {
    tx.query_row(
        "SELECT state, cancel_requested, requested, started, weight, sig_full, sig_head, sig_prog, sig_tool, scope \
         FROM work_tasks WHERE id=?",
        [id],
        |r| {
            Ok(Current {
                state: r.get(0)?,
                cancel_requested: r.get(1)?,
                requested: r.get(2)?,
                started: r.get(3)?,
                weight: r.get(4)?,
                sig_full: r.get(5)?,
                sig_head: r.get(6)?,
                sig_prog: r.get(7)?,
                sig_tool: r.get(8)?,
                scope: r.get(9)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
}

/// The task as the work queue reports it, read inside the caller's transaction.
fn load_task(tx: &Transaction, id: &str) -> Result<TaskRecord, StoreError> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM work_tasks WHERE id=?");
    Ok(tx.query_row(&sql, [id], task_from_row)?)
}

/// A u64 from a supervisor report, stored as SQLite INTEGER. Out of range is refused, never truncated.
fn to_i64(v: u64, what: &'static str) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::Invalid(what))
}

impl Store {
    /// "job.start" (system): a claimed, requested, not-yet-started task begins running.
    pub fn job_start(&mut self, now: i64, op: &SignedOp) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        authorize_system(&tx, now, op, "job.start")?;
        let args: JobStartArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("job.start arg must be JSON"))?;
        let cur = current(&tx, &op.subject)?;
        let waiting = cur.state == TaskState::Claimed.as_str()
            && cur.requested.is_some()
            && cur.started.is_none();
        if !waiting {
            return Err(StoreError::Invalid("job is not waiting to start"));
        }
        let supervisor_start = args
            .supervisor_start_s
            .map(|s| to_i64(s, "supervisor start time out of range"))
            .transpose()?;
        let qos = (!args.qos.is_empty()).then_some(args.qos.as_str());
        let log = (!args.log.is_empty()).then_some(args.log.as_str());
        tx.execute(
            "UPDATE work_tasks SET state=?, started=?, pid=?, supervisor_pid=?, supervisor_start=?, qos=?, \
             exclusive=?, quiet_until=?, log=?, note='' WHERE id=?",
            params![
                TaskState::Running.as_str(),
                now,
                args.pid,
                args.supervisor_pid,
                supervisor_start,
                qos,
                args.exclusive,
                args.quiet_until_ms,
                log,
                op.subject,
            ],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "job.started",
            &op.subject,
            &serde_json::json!({
                "pid": args.pid,
                "qos": args.qos,
                "scope": cur.scope,
                "detail": format!("process {}", args.pid),
            }),
        )?;
        let task = load_task(&tx, &op.subject)?;
        tx.commit()?;
        Ok(task)
    }

    /// "job.finish" (system). State: cancel_requested -> cancelled; before_start -> failed; status Some(0) -> done;
    /// else failed. Inserts a job_history row when the job started. If `sync` is Some it is a "card.job_sync" op
    /// applied in the SAME transaction (D7).
    pub fn job_finish(
        &mut self,
        now: i64,
        op: &SignedOp,
        sync: Option<&SignedOp>,
    ) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        authorize_system(&tx, now, op, "job.finish")?;
        let args: JobFinishArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("job.finish arg must be JSON"))?;
        let cur = current(&tx, &op.subject)?;
        let running = cur.state == TaskState::Running.as_str();
        // A requested task that never started may be finished as failed, but only with before_start set.
        let before_start = cur.state == TaskState::Claimed.as_str()
            && cur.requested.is_some()
            && args.before_start;
        if !running && !before_start {
            return Err(StoreError::Invalid("job is not running"));
        }
        let new_state = if cur.cancel_requested {
            TaskState::Cancelled
        } else if args.before_start {
            TaskState::Failed
        } else if args.status == Some(0) {
            TaskState::Done
        } else {
            TaskState::Failed
        };
        let rss_peak = args
            .rss_peak_kib
            .map(|v| to_i64(v, "rss peak out of range"))
            .transpose()?;
        tx.execute(
            "UPDATE work_tasks SET state=?, ended=?, status=?, note=?, cpu_time=?, rss_peak=COALESCE(?, rss_peak) \
             WHERE id=?",
            params![
                new_state.as_str(),
                args.ended_ms,
                args.status,
                args.note,
                args.cpu_seconds,
                rss_peak,
                op.subject,
            ],
        )?;
        let ran = args.status.is_some_and(|s| !NOT_RUN_STATUSES.contains(&s));
        if let Some(started) = cur.started.filter(|_| ran) {
            tx.execute(
                "INSERT INTO job_history(task, sig_full, sig_head, sig_prog, sig_tool, weight, status, started, \
                 ended, cpu_time, rss_peak, requested) VALUES (?,?,?,?,?,?,?,?,?,?,COALESCE(?, (SELECT rss_peak FROM work_tasks WHERE id=?)),?)",
                params![
                    op.subject,
                    cur.sig_full,
                    cur.sig_head,
                    cur.sig_prog,
                    cur.sig_tool,
                    cur.weight,
                    args.status,
                    started,
                    args.ended_ms,
                    args.cpu_seconds,
                    rss_peak,
                    op.subject,
                    cur.requested,
                ],
            )?;
        }
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "job.finished",
            &op.subject,
            &serde_json::json!({
                "status": args.status,
                "state": new_state.as_str(),
                "scope": cur.scope,
                "detail": args.status.map_or_else(
                    || "no status".to_owned(),
                    |status| format!("status {status}"),
                ),
            }),
        )?;
        if let Some(sync_op) = sync {
            card_job_sync_tx(&tx, now, sync_op, &op.subject, new_state)?;
        }
        let task = load_task(&tx, &op.subject)?;
        tx.commit()?;
        Ok(task)
    }

    /// "job.note" (system): replaces the task's note; the state is unchanged.
    pub fn job_note(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        authorize_system(&tx, now, op, "job.note")?;
        let args: JobNoteArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("job.note arg must be JSON"))?;
        let cur = current(&tx, &op.subject)?;
        tx.execute(
            "UPDATE work_tasks SET note=? WHERE id=?",
            params![args.note, op.subject],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "job.noted",
            &op.subject,
            &serde_json::json!({"scope": cur.scope, "detail": args.note}),
        )?;
        Ok(tx.commit()?)
    }

    /// cpu/rss/rss_peak/sampled while running (not an op: sampling is not audited; no event). A task that is no
    /// longer running is left alone and the call succeeds: a sample read just after the job ended is not an error.
    pub fn job_sample(
        &mut self,
        id: &str,
        cpu: f64,
        rss_kib: u64,
        rss_peak_kib: u64,
        sampled_ms: i64,
    ) -> Result<(), StoreError> {
        let rss = to_i64(rss_kib, "rss out of range")?;
        let peak = to_i64(rss_peak_kib, "rss peak out of range")?;
        self.conn.execute(
            "UPDATE work_tasks SET cpu=?, rss=?, rss_peak=?, sampled=? WHERE id=? AND state=?",
            params![cpu, rss, peak, sampled_ms, id, TaskState::Running.as_str()],
        )?;
        Ok(())
    }

    /// Retention (D17, S-5): deletes events of kind work.%, job.%, lease.%, timer.%, quiet.%, pool.%, policy.% and
    /// reconcile.% older than `cutoff_ms`, leases released before the cutoff, reconcile marks and notices older than
    /// the cutoff (never `job_history`, which feeds the estimates, and never an active lease); returns the ids of
    /// ended tasks older than the cutoff whose log is not null (log set to NULL); the daemon deletes the files.
    pub fn work_prune(&mut self, cutoff_ms: i64) -> Result<Vec<String>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM events WHERE ts < ? AND (kind LIKE 'work.%' OR kind LIKE 'job.%' \
             OR kind LIKE 'lease.%' OR kind LIKE 'timer.%' OR kind LIKE 'quiet.%' OR kind LIKE 'pool.%' \
             OR kind LIKE 'policy.%' OR kind LIKE 'reconcile.%')",
            [cutoff_ms],
        )?;
        tx.execute(
            "DELETE FROM leases WHERE released IS NOT NULL AND released < ?",
            [cutoff_ms],
        )?;
        tx.execute("DELETE FROM reconcile_marks WHERE last < ?", [cutoff_ms])?;
        tx.execute("DELETE FROM notices WHERE ts < ?", [cutoff_ms])?;
        let ids: Vec<String> = {
            let mut st = tx.prepare(
                "SELECT id FROM work_tasks WHERE state IN ('done','failed','cancelled') \
                 AND ended < ? AND log IS NOT NULL ORDER BY seq",
            )?;
            st.query_map([cutoff_ms], |r| r.get(0))?
                .collect::<Result<Vec<String>, rusqlite::Error>>()?
        };
        for id in &ids {
            tx.execute("UPDATE work_tasks SET log=NULL WHERE id=?", [id])?;
        }
        tx.commit()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests;
