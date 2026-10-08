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
mod tests {
    use super::*;
    use agora_core::TransitionError;
    use agora_crypto::Identity;

    const NOW: i64 = 1_700_000_000_000;

    /// A store with the system identity registered, and a worker key that is not the system.
    fn setup() -> (Store, Identity, Identity) {
        let mut s = Store::open_in_memory().unwrap();
        let system = Identity::from_seed([9; 32]);
        let worker = Identity::from_seed([3; 32]);
        s.register_system_identity(NOW, &system.public()).unwrap();
        s.register_identity(NOW, &worker.public(), "worker")
            .unwrap();
        (s, system, worker)
    }

    /// Nonces are at most 128 bytes, so they name the op by kind, subject and time, not by its arg.
    fn signed(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
        let nonce = format!("{kind}/{subject}/{now}");
        SignedOp::new(kind, subject, arg, now, &nonce).sign(id)
    }

    /// A claimed, requested, not-yet-started task, as the work queue leaves it.
    fn insert_waiting(s: &Store, id: &str) {
        s.conn
            .execute(
                "INSERT INTO work_tasks(id, author, queued, state, claimed, requested) \
                 VALUES (?1, 'author', ?2, 'claimed', ?2, ?2)",
                params![id, NOW],
            )
            .unwrap();
    }

    fn start_arg() -> String {
        serde_json::to_string(&JobStartArgs {
            pid: 4242,
            supervisor_pid: 4241,
            supervisor_start_s: Some(1_700_000_000),
            qos: "utility".into(),
            exclusive: false,
            quiet_until_ms: None,
            log: "/logs/t.log".into(),
        })
        .unwrap()
    }

    fn finish_arg(status: Option<i32>, before_start: bool) -> String {
        serde_json::to_string(&JobFinishArgs {
            status,
            note: String::new(),
            cpu_seconds: Some(1.5),
            rss_peak_kib: Some(2048),
            ended_ms: NOW + 5_000,
            before_start,
        })
        .unwrap()
    }

    fn start(s: &mut Store, system: &Identity, id: &str) -> TaskRecord {
        let op = signed(system, "job.start", id, &start_arg(), NOW);
        s.job_start(NOW, &op).unwrap()
    }

    fn finish(
        s: &mut Store,
        system: &Identity,
        id: &str,
        status: Option<i32>,
        before_start: bool,
    ) -> Result<TaskRecord, StoreError> {
        let arg = finish_arg(status, before_start);
        let op = signed(system, "job.finish", id, &arg, NOW + 5_000);
        s.job_finish(NOW + 5_000, &op, None)
    }

    fn state_of(s: &Store, id: &str) -> String {
        s.conn
            .query_row("SELECT state FROM work_tasks WHERE id=?", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn history_statuses(s: &Store, id: &str) -> Vec<Option<i64>> {
        let mut st = s
            .conn
            .prepare("SELECT status FROM job_history WHERE task=? ORDER BY seq")
            .unwrap();
        st.query_map([id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn event_count(s: &Store, kind: &str, subject: &str) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind=? AND subject=?",
                params![kind, subject],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn start_marks_claimed_task_running() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");

        let task = start(&mut s, &system, "t1");

        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.started_ms, Some(NOW));
        assert_eq!(task.pid, Some(4242));
        assert_eq!(task.supervisor_pid, Some(4241));
        assert_eq!(task.qos.as_deref(), Some("utility"));
        assert_eq!(task.log.as_deref(), Some("/logs/t.log"));
        assert_eq!(task.note, "");
        assert_eq!(event_count(&s, "job.started", "t1"), 1);
    }

    #[test]
    fn start_refuses_a_task_that_is_not_waiting() {
        let (mut s, system, _) = setup();
        s.conn
            .execute(
                "INSERT INTO work_tasks(id, author, queued, state) VALUES ('queued', 'a', ?, 'queued')",
                [NOW],
            )
            .unwrap();
        let op = signed(&system, "job.start", "queued", &start_arg(), NOW);
        let err = s.job_start(NOW, &op).unwrap_err();
        assert!(matches!(
            err,
            StoreError::Invalid("job is not waiting to start")
        ));

        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        let again = signed(&system, "job.start", "t1", &start_arg(), NOW + 1);
        let err = s.job_start(NOW + 1, &again).unwrap_err();
        assert!(matches!(
            err,
            StoreError::Invalid("job is not waiting to start")
        ));
    }

    #[test]
    fn finish_status_zero_is_done_and_records_history() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");

        let task = finish(&mut s, &system, "t1", Some(0), false).unwrap();

        assert_eq!(task.state, TaskState::Done);
        assert_eq!(task.status, Some(0));
        assert_eq!(task.ended_ms, Some(NOW + 5_000));
        assert_eq!(history_statuses(&s, "t1"), [Some(0)]);
        assert_eq!(event_count(&s, "job.finished", "t1"), 1);
    }

    #[test]
    fn finish_nonzero_status_is_failed_and_records_history() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");

        let task = finish(&mut s, &system, "t1", Some(3), false).unwrap();

        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.status, Some(3));
        assert_eq!(history_statuses(&s, "t1"), [Some(3)]);
    }

    #[test]
    fn finish_with_cancel_requested_is_cancelled() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        s.conn
            .execute("UPDATE work_tasks SET cancel_requested=1 WHERE id='t1'", [])
            .unwrap();

        let task = finish(&mut s, &system, "t1", Some(143), false).unwrap();

        assert_eq!(task.state, TaskState::Cancelled);
        assert_eq!(task.status, Some(143));
        assert_eq!(history_statuses(&s, "t1"), [Some(143)]);
    }

    #[test]
    fn finish_before_start_is_failed_without_history() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");

        let task = finish(&mut s, &system, "t1", None, true).unwrap();

        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.started_ms, None);
        assert_eq!(task.ended_ms, Some(NOW + 5_000));
        assert!(history_statuses(&s, "t1").is_empty());
    }

    #[test]
    fn finish_with_status_126_or_127_leaves_no_history() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        insert_waiting(&s, "t2");
        start(&mut s, &system, "t1");
        start(&mut s, &system, "t2");

        let missing = finish(&mut s, &system, "t1", Some(127), false).unwrap();
        let not_exec = finish(&mut s, &system, "t2", Some(126), false).unwrap();

        assert_eq!(missing.state, TaskState::Failed);
        assert_eq!(not_exec.state, TaskState::Failed);
        assert!(history_statuses(&s, "t1").is_empty());
        assert!(history_statuses(&s, "t2").is_empty());
    }

    #[test]
    fn finish_refuses_a_task_that_is_not_running() {
        let (mut s, system, _) = setup();
        s.conn
            .execute(
                "INSERT INTO work_tasks(id, author, queued, state) VALUES ('queued', 'a', ?, 'queued')",
                [NOW],
            )
            .unwrap();
        insert_waiting(&s, "t1");

        let err = finish(&mut s, &system, "queued", Some(0), true).unwrap_err();
        assert!(matches!(err, StoreError::Invalid("job is not running")));
        // Claimed and requested, but before_start is not set: the job never ran, so this is not a finish.
        let err = finish(&mut s, &system, "t1", Some(0), false).unwrap_err();
        assert!(matches!(err, StoreError::Invalid("job is not running")));
        assert_eq!(state_of(&s, "t1"), "claimed");
    }

    #[test]
    fn worker_signed_job_finish_is_forbidden() {
        let (mut s, system, worker) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        let op = signed(
            &worker,
            "job.finish",
            "t1",
            &finish_arg(Some(0), false),
            NOW + 5_000,
        );

        let err = s.job_finish(NOW + 5_000, &op, None).unwrap_err();

        assert!(matches!(
            err,
            StoreError::Transition(TransitionError::Forbidden(_))
        ));
        assert_eq!(state_of(&s, "t1"), "running");
    }

    #[test]
    fn failed_card_sync_rolls_back_the_finish() {
        let (mut s, system, worker) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        // A refused sync in the same transaction must take the job finish down with it.
        let sync = signed(&worker, "card.job_sync", "c1", "{}", NOW + 5_000);
        let arg = finish_arg(Some(0), false);
        let op = signed(&system, "job.finish", "t1", &arg, NOW + 5_000);

        assert!(s.job_finish(NOW + 5_000, &op, Some(&sync)).is_err());

        assert_eq!(state_of(&s, "t1"), "running");
        assert!(history_statuses(&s, "t1").is_empty());
        assert_eq!(event_count(&s, "job.finished", "t1"), 0);
    }

    #[test]
    fn finish_without_a_peak_keeps_the_sampled_one() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        s.job_sample("t1", 1.0, 100, 4096, NOW + 1_000).unwrap();
        let arg = serde_json::to_string(&JobFinishArgs {
            status: Some(0),
            note: String::new(),
            cpu_seconds: None,
            rss_peak_kib: None,
            ended_ms: NOW + 5_000,
            before_start: false,
        })
        .unwrap();

        s.job_finish(
            NOW + 5_000,
            &signed(&system, "job.finish", "t1", &arg, NOW + 5_000),
            None,
        )
        .unwrap();

        let peak: Option<i64> = s
            .conn
            .query_row("SELECT rss_peak FROM work_tasks WHERE id='t1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(peak, Some(4096));
    }

    #[test]
    fn finish_without_a_peak_records_the_sampled_one_in_history() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        s.job_sample("t1", 1.0, 100, 4096, NOW + 1_000).unwrap();
        let arg = serde_json::to_string(&JobFinishArgs {
            status: Some(0),
            note: String::new(),
            cpu_seconds: None,
            rss_peak_kib: None,
            ended_ms: NOW + 5_000,
            before_start: false,
        })
        .unwrap();

        s.job_finish(
            NOW + 5_000,
            &signed(&system, "job.finish", "t1", &arg, NOW + 5_000),
            None,
        )
        .unwrap();

        let peak: Option<i64> = s
            .conn
            .query_row(
                "SELECT rss_peak FROM job_history WHERE task='t1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(peak, Some(4096));
    }

    #[test]
    fn job_note_replaces_note_and_keeps_state() {
        let (mut s, system, _) = setup();
        insert_waiting(&s, "t1");
        start(&mut s, &system, "t1");
        let arg = serde_json::json!({"note": "its supervisor is gone"}).to_string();

        s.job_note(NOW + 1, &signed(&system, "job.note", "t1", &arg, NOW + 1))
            .unwrap();

        let note: String = s
            .conn
            .query_row("SELECT note FROM work_tasks WHERE id='t1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(note, "its supervisor is gone");
        assert_eq!(state_of(&s, "t1"), "running");
        assert_eq!(event_count(&s, "job.noted", "t1"), 1);
    }

    #[test]
    fn job_events_carry_the_task_scope_and_a_detail() {
        let (mut s, system, _) = setup();
        s.conn
            .execute(
                "INSERT INTO work_scopes(id,kind,task,created,updated) VALUES ('proj','project','',?,?)",
                params![NOW, NOW],
            )
            .unwrap();
        insert_waiting(&s, "t1");
        s.conn
            .execute("UPDATE work_tasks SET scope='proj' WHERE id='t1'", [])
            .unwrap();
        start(&mut s, &system, "t1");
        let arg = serde_json::json!({"note": "slow disk"}).to_string();
        s.job_note(NOW + 1, &signed(&system, "job.note", "t1", &arg, NOW + 1))
            .unwrap();
        finish(&mut s, &system, "t1", Some(3), false).unwrap();

        let data = |kind: &str| -> serde_json::Value {
            let text: String = s
                .conn
                .query_row(
                    "SELECT data FROM events WHERE kind=? AND subject='t1'",
                    [kind],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        for kind in ["job.started", "job.noted", "job.finished"] {
            assert_eq!(data(kind)["scope"], "proj", "{kind} carries the scope");
        }
        assert_eq!(data("job.started")["detail"], "process 4242");
        assert_eq!(data("job.noted")["detail"], "slow disk");
        assert_eq!(data("job.finished")["detail"], "status 3");
    }

    #[test]
    fn job_sample_updates_only_running_tasks() {
        let (mut s, _, _) = setup();
        insert_waiting(&s, "t1");
        s.conn
            .execute("UPDATE work_tasks SET state='running' WHERE id='t1'", [])
            .unwrap();
        insert_waiting(&s, "t2");

        s.job_sample("t1", 12.5, 1000, 2000, NOW + 2_000).unwrap();
        s.job_sample("t2", 9.0, 1, 1, NOW + 2_000).unwrap();

        let sampled: (f64, i64, i64, i64) = s
            .conn
            .query_row(
                "SELECT cpu, rss, rss_peak, sampled FROM work_tasks WHERE id='t1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(sampled, (12.5, 1000, 2000, NOW + 2_000));
        let untouched: Option<f64> = s
            .conn
            .query_row("SELECT cpu FROM work_tasks WHERE id='t2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(untouched, None);
    }

    #[test]
    fn work_prune_drops_old_events_and_returns_ended_logs() {
        let (mut s, _, _) = setup();
        let old = NOW - 10_000;
        s.conn
            .execute_batch(&format!(
                "INSERT INTO events(ts, actor, kind, subject) VALUES
                    ({old}, 'a', 'work.added', 't-old'),
                    ({old}, 'a', 'job.started', 't-old'),
                    ({old}, 'a', 'card.created', 'c-old'),
                    ({NOW}, 'a', 'work.added', 't-new');
                 INSERT INTO reconcile_marks(key, last) VALUES ('mark-old', {old}), ('mark-new', {NOW});
                 INSERT INTO notices(ts, target, kind, subject, text) VALUES
                    ({old}, 'a', 'job_sync', 'c-old', 'old'),
                    ({NOW}, 'a', 'job_sync', 'c-new', 'new');
                 INSERT INTO job_history(task, weight, status, started, ended) VALUES ('old-done', 1, 0, {old}, {old});
                 INSERT INTO work_tasks(id, author, queued, state, ended, log) VALUES
                    ('old-done', 'a', 0, 'done', {old}, '/logs/old-done.log'),
                    ('old-failed', 'a', 0, 'failed', {old}, '/logs/old-failed.log'),
                    ('old-no-log', 'a', 0, 'done', {old}, NULL),
                    ('old-running', 'a', 0, 'running', NULL, '/logs/old-running.log'),
                    ('new-done', 'a', 0, 'done', {NOW}, '/logs/new-done.log');"
            ))
            .unwrap();

        let ids = s.work_prune(NOW - 1_000).unwrap();

        assert_eq!(ids, ["old-done", "old-failed"]);
        let log_of = |id: &str| -> Option<String> {
            s.conn
                .query_row("SELECT log FROM work_tasks WHERE id=?", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(log_of("old-done"), None);
        assert_eq!(log_of("old-failed"), None);
        assert_eq!(log_of("new-done").as_deref(), Some("/logs/new-done.log"));
        assert_eq!(
            log_of("old-running").as_deref(),
            Some("/logs/old-running.log")
        );
        // Old work.* and job.* events go; the old card event and the recent work event stay.
        assert_eq!(event_count(&s, "work.added", "t-old"), 0);
        assert_eq!(event_count(&s, "job.started", "t-old"), 0);
        assert_eq!(event_count(&s, "card.created", "c-old"), 1);
        assert_eq!(event_count(&s, "work.added", "t-new"), 1);
        // Reconcile marks and notices older than the cutoff go; the recent ones stay.
        let count = |sql: &str| -> i64 { s.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM reconcile_marks WHERE key='mark-old'"),
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM reconcile_marks WHERE key='mark-new'"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM notices WHERE subject='c-old'"),
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM notices WHERE subject='c-new'"),
            1
        );
        // Job history is never pruned by retention.
        assert_eq!(
            count("SELECT COUNT(*) FROM job_history WHERE task='old-done'"),
            1
        );
    }

    #[test]
    fn work_prune_drops_old_coordination_events_and_released_leases_only() {
        let (mut s, _, _) = setup();
        let old = NOW - 10_000;
        s.conn
            .execute_batch(&format!(
                "INSERT INTO events(ts, actor, kind, subject) VALUES
                    ({old}, 'a', 'lease.acquired', 'old-lease'),
                    ({old}, 'a', 'timer.created', 'old-timer'),
                    ({old}, 'a', 'quiet.opened', 'old-quiet'),
                    ({old}, 'a', 'pool.defined', 'old-pool'),
                    ({old}, 'a', 'policy.set', 'old-policy'),
                    ({old}, 'a', 'reconcile.blocked', 'old-card'),
                    ({old}, 'a', 'card.created', 'c-keep'),
                    ({NOW}, 'a', 'lease.renewed', 'new-lease');
                 INSERT INTO leases(id, kind, resource, holder, created, expires, heartbeat, released) VALUES
                    ('released-old', 'path', '/a/b', 'h', {old}, {NOW}, {old}, {old}),
                    ('active-old', 'path', '/a/c', 'h', {old}, {NOW} + 1000000, {old}, NULL),
                    ('released-new', 'path', '/a/d', 'h', {old}, {NOW} + 1000000, {old}, {NOW});
                 INSERT INTO job_history(task, weight, status, started, ended) VALUES ('keep', 1, 0, {old}, {old});"
            ))
            .unwrap();

        s.work_prune(NOW - 1_000).unwrap();

        for (kind, subject) in [
            ("lease.acquired", "old-lease"),
            ("timer.created", "old-timer"),
            ("quiet.opened", "old-quiet"),
            ("pool.defined", "old-pool"),
            ("policy.set", "old-policy"),
            ("reconcile.blocked", "old-card"),
        ] {
            assert_eq!(event_count(&s, kind, subject), 0, "{kind} {subject}");
        }
        assert_eq!(event_count(&s, "card.created", "c-keep"), 1);
        assert_eq!(event_count(&s, "lease.renewed", "new-lease"), 1);
        let lease_exists = |id: &str| -> i64 {
            s.conn
                .query_row("SELECT COUNT(*) FROM leases WHERE id=?", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(lease_exists("released-old"), 0);
        assert_eq!(lease_exists("active-old"), 1);
        assert_eq!(lease_exists("released-new"), 1);
        let history: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM job_history WHERE task='keep'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(history, 1);
    }
}
