//! Agent operations on the work queue: scopes, tasks, claims, acceptance, runs, finish, cancel, update, reclaim and
//! settings. Rules: SPEC section 3.2 and D3 to D9, ref-queue-cli sections 1 to 4.
//!
//! Each operation runs in one IMMEDIATE transaction. `authorize` derives the role from the database and consumes the
//! nonce; the change and its event are written together, and a refusal rolls everything back. The helpers live in
//! `work_ops/`.

mod add;
pub(crate) mod checks;
mod edit;
mod queue;
mod request;
mod scope;
#[cfg(test)]
mod tests;

use self::add::Placement;
use self::checks::{
    TEXT_MAX, agent_label, check_request, check_runnable, check_text_field, detail_text, digest_of,
    event, load_task, malformed, may_act, may_run, refusal, refuse, scope_exists, settings_in,
    subject_task,
};
use crate::identity::{authorize, need};
use crate::{SignedOp, Store, StoreError};
use agora_core::Role;
use agora_sched::claims;
use agora_sched::model::{
    CancelOutcome, NextOutcome, ScopeRecord, StopTarget, TaskRecord, TaskState,
};
use agora_sched::model_ops::{
    AcceptArgs, AddArgs, CancelArgs, FinishArgs, NextArgs, ReclaimArgs, RunArgs, ScopeArgs,
    SettingsArgs, UpdateArgs,
};
use agora_sched::settings::WorkSettings;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;

impl Store {
    /// "work.scope": create or update a scope. A new scope is `agent` when it has a parent, else `project`.
    pub fn work_scope_set(&mut self, now: i64, op: &SignedOp) -> Result<ScopeRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.scope")?;
        need(role, Role::Worker)?;
        let args: ScopeArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        scope::set_in(&tx, now, &op.actor_hex(), &args)?;
        tx.commit()?;
        self.work_scopes(Some(&args.id))?
            .into_iter()
            .find(|s| s.id == args.id)
            .ok_or_else(|| StoreError::NotFound(format!("scope {}", args.id)))
    }

    /// "work.add": queue a task (optionally with a command, a card link and file claims).
    pub fn work_add(&mut self, now: i64, op: &SignedOp) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.add")?;
        need(role, Role::Worker)?;
        let args: AddArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let id = add::insert_task(&tx, now, &op.actor_hex(), role, &args, Placement::Queued)?;
        tx.commit()?;
        load_task(&self.conn, &id)
    }

    /// "work.next": claim the first claimable task of a scope in policy order. Nothing claimable is `Ok` with
    /// `claimed: None` and the reason.
    pub fn work_next(&mut self, now: i64, op: &SignedOp) -> Result<NextOutcome, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.next")?;
        need(role, Role::Worker)?;
        let args: NextArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let scope = args.scope.as_deref().filter(|s| !s.is_empty());
        if let Some(name) = scope {
            refuse(claims::validate_name("scope", name))?;
            scope_exists(&tx, name)?;
        }
        let outcome = queue::next_in(&tx, now, &op.actor_hex(), scope, args.policy)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// "work.claim": claim one named queued task. The subject is the task id.
    pub fn work_claim(&mut self, now: i64, op: &SignedOp) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.claim")?;
        need(role, Role::Worker)?;
        let actor = op.actor_hex();
        let id = subject_task(&op.subject)?;
        let task = load_task(&tx, id)?;
        if !matches!(task.state, TaskState::Queued) {
            return Err(refusal(format!("task {id} is {}", task.describe_state())));
        }
        queue::claim_block(&tx, now, &task, &actor)?;
        queue::take(&tx, now, id, &actor)?;
        tx.commit()?;
        load_task(&self.conn, id)
    }

    /// "work.accept": the claimant accepts the command's digest. Ok((task, first_time)); a repeat is `false`.
    pub fn work_accept(
        &mut self,
        now: i64,
        op: &SignedOp,
    ) -> Result<(TaskRecord, bool), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.accept")?;
        need(role, Role::Worker)?;
        let args: AcceptArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let actor = op.actor_hex();
        let settings = settings_in(&tx)?;
        may_run(role, &settings)?;
        let id = subject_task(&op.subject)?;
        let task = load_task(&tx, id)?;
        let Some(sealed) = digest_of(&task) else {
            return Err(refusal(format!("task {id} has no command to accept")));
        };
        if !matches!(task.state, TaskState::Claimed) || task.requested_ms.is_some() {
            return Err(refusal(format!(
                "task {id} is {}; accept a command after you claim its task",
                task.describe_state()
            )));
        }
        if task.agent.as_deref() != Some(actor.as_str()) {
            return Err(refusal(format!(
                "task {id} is claimed by {}; only that agent accepts its command",
                agent_label(&task)
            )));
        }
        if task.author == actor && !settings.self_accept {
            return Err(StoreError::Invalid(
                "a command must be accepted by an identity other than its author",
            ));
        }
        if args.digest.trim().to_lowercase() != sealed {
            return Err(refusal(format!(
                "the digest does not match the command of task {id} in its directory; read it with `agora work show {id}`, \
                 and accept it with the digest that show prints"
            )));
        }
        let first = task.accepted_by.as_deref() != Some(actor.as_str());
        if first {
            tx.execute(
                "UPDATE work_tasks SET accepted=?1 WHERE id=?2",
                params![actor, id],
            )?;
            event(
                &tx,
                now,
                &actor,
                "work.accepted",
                id,
                json!({
                    "digest": sealed,
                    "scope": task.scope,
                    "detail": format!("digest {sealed}"),
                }),
            )?;
        }
        tx.commit()?;
        Ok((load_task(&self.conn, id)?, first))
    }

    /// "work.run": a claimed, accepted task (`task`) is requested, or an inline command (`add`) creates a claimed,
    /// accepted job first. Computes signatures (real filesystem) and the prediction (with `cpus`); the log is
    /// `<log_dir>/<id>.log`.
    pub fn work_request(
        &mut self,
        now: i64,
        op: &SignedOp,
        cpus: f64,
        log_dir: &str,
    ) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.run")?;
        need(role, Role::Worker)?;
        let args: RunArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let actor = op.actor_hex();
        let settings = settings_in(&tx)?;
        may_run(role, &settings)?;
        let named = args.task.is_some();
        let id = match (args.task, args.add) {
            (Some(_), Some(_)) => {
                return Err(StoreError::Invalid("give a task ID or a command, not both"));
            }
            (None, None) => {
                return Err(StoreError::Invalid(
                    "name a task you claimed, or give a command to run",
                ));
            }
            (Some(task), None) => {
                refuse(claims::validate_name("task", &task))?;
                task
            }
            (None, Some(add)) => {
                add::insert_task(&tx, now, &actor, role, &add, Placement::Inline { cpus })?
            }
        };
        let task = load_task(&tx, &id)?;
        check_runnable(&task, &actor)?;
        check_request(&tx, op, &actor, &task, named, &settings)?;
        request::reserve(&tx, now, &task, &actor, cpus, log_dir)?;
        tx.commit()?;
        load_task(&self.conn, &id)
    }

    /// "work.finish": close a task without a command as done or failed, by its author (queued) or claimant.
    pub fn work_finish(&mut self, now: i64, op: &SignedOp) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.finish")?;
        need(role, Role::Worker)?;
        let args: FinishArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let status = match args.state {
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            _ => return Err(StoreError::Invalid("finish takes done or failed")),
        };
        let actor = op.actor_hex();
        let id = subject_task(&op.subject)?;
        check_text_field("note", &args.note, TEXT_MAX)?;
        let task = load_task(&tx, id)?;
        if task.state.is_final() {
            return Err(refusal(format!(
                "task {id} is {} already",
                task.describe_state()
            )));
        }
        if matches!(task.state, TaskState::Running) {
            return Err(refusal(format!(
                "task {id} is {}; its supervisor records how it ends, so stop it with `agora work cancel {id}`",
                task.describe_state()
            )));
        }
        match task.state {
            TaskState::Queued if task.author != actor => {
                return Err(refusal(format!(
                    "task {id} was queued by {}; until an agent claims it, only {} closes it",
                    task.author_name, task.author_name
                )));
            }
            TaskState::Claimed if task.agent.as_deref() != Some(actor.as_str()) => {
                return Err(refusal(format!(
                    "task {id} is claimed by {}; only that agent closes it",
                    agent_label(&task)
                )));
            }
            _ => {}
        }
        if task.command.is_some() {
            return Err(refusal(format!(
                "task {id} has a command: run it with `agora work run {id}`, or cancel it"
            )));
        }
        tx.execute(
            "UPDATE work_tasks SET state=?1, ended=?2, note=?3 WHERE id=?4",
            params![status, now, args.note, id],
        )?;
        event(
            &tx,
            now,
            &actor,
            &format!("work.{status}"),
            id,
            json!({
                "note": args.note,
                "scope": task.scope,
                "detail": detail_text(&args.note, &task.title),
            }),
        )?;
        tx.commit()?;
        load_task(&self.conn, id)
    }

    /// "work.cancel": a queued, claimed or waiting task ends as cancelled. A running task is only flagged
    /// (`cancel_requested`) and returns a `StopTarget` for the daemon, which finalises it as cancelled.
    pub fn work_cancel(&mut self, now: i64, op: &SignedOp) -> Result<CancelOutcome, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.cancel")?;
        need(role, Role::Worker)?;
        let args: CancelArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let actor = op.actor_hex();
        let id = subject_task(&op.subject)?;
        check_text_field("note", &args.note, TEXT_MAX)?;
        let task = load_task(&tx, id)?;
        if task.state.is_final() {
            return Err(refusal(format!(
                "task {id} is {} already",
                task.describe_state()
            )));
        }
        may_act(role, &actor, &task)?;
        if matches!(task.state, TaskState::Running) {
            let supervisor_pid = task.supervisor_pid.ok_or(StoreError::Invalid(
                "the running task has no recorded supervisor",
            ))?;
            let start: Option<i64> = tx.query_row(
                "SELECT supervisor_start FROM work_tasks WHERE id=?",
                [id],
                |r| r.get(0),
            )?;
            // The note says who asked for the stop and why; the supervisor keeps it when it finalises the job.
            let by = agora_crypto::pseudonym(&op.actor);
            let note = if args.note.is_empty() {
                format!("cancelled by {by}")
            } else {
                format!("cancelled by {by}: {}", args.note)
            };
            tx.execute(
                "UPDATE work_tasks SET cancel_requested=1, note=?1 WHERE id=?2",
                params![note, id],
            )?;
            event(
                &tx,
                now,
                &actor,
                "work.cancel_requested",
                id,
                json!({
                    "note": args.note,
                    "scope": task.scope,
                    "detail": detail_text(&args.note, &task.title),
                }),
            )?;
            tx.commit()?;
            let stop = StopTarget {
                supervisor_pid,
                supervisor_start_s: start.and_then(|s| u64::try_from(s).ok()),
                job_dir: String::new(),
            };
            return Ok(CancelOutcome {
                task: load_task(&self.conn, id)?,
                stop: Some(stop),
            });
        }
        tx.execute(
            "UPDATE work_tasks SET state='cancelled', ended=?1, note=?2 WHERE id=?3",
            params![now, args.note, id],
        )?;
        event(
            &tx,
            now,
            &actor,
            "work.cancelled",
            id,
            json!({
                "note": args.note,
                "scope": task.scope,
                "detail": detail_text(&args.note, &task.title),
            }),
        )?;
        tx.commit()?;
        Ok(CancelOutcome {
            task: load_task(&self.conn, id)?,
            stop: None,
        })
    }

    /// "work.update": reprioritise, edit, assign or unassign. The subject is the task id.
    pub fn work_update(&mut self, now: i64, op: &SignedOp) -> Result<TaskRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.update")?;
        need(role, Role::Worker)?;
        let args: UpdateArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let id = subject_task(&op.subject)?;
        edit::update_in(&tx, now, &op.actor_hex(), role, id, &args)?;
        tx.commit()?;
        load_task(&self.conn, id)
    }

    /// "work.reclaim" (supervisor): subject `task:<id>` returns a claimed, not requested task to the queue;
    /// `lease:<id>` releases an active path lease.
    pub fn work_reclaim(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.reclaim")?;
        need(role, Role::Supervisor)?;
        let args: ReclaimArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        check_text_field("note", &args.note, TEXT_MAX)?;
        let actor = op.actor_hex();
        if let Some(id) = op.subject.strip_prefix("task:") {
            refuse(claims::validate_name("task", id))?;
            let task = load_task(&tx, id)?;
            if !matches!(task.state, TaskState::Claimed) || task.requested_ms.is_some() {
                return Err(refusal(format!(
                    "task {id} is {}; only a claimed task that has not been requested can be reclaimed",
                    task.describe_state()
                )));
            }
            tx.execute(
                "UPDATE work_tasks SET state='queued', agent=NULL, claimed=NULL, accepted=NULL, note=?1 WHERE id=?2",
                params![args.note, id],
            )?;
            event(
                &tx,
                now,
                &actor,
                "work.reclaimed",
                id,
                json!({
                    "note": args.note,
                    "scope": task.scope,
                    "detail": detail_text(&args.note, &task.title),
                }),
            )?;
        } else if let Some(id) = op.subject.strip_prefix("lease:") {
            // None: no such lease. A quiet window closes with quiet.close, never by reclaim, so the kind is judged
            // first. Some(released): the lease exists, with its release time when released.
            let lease: Option<(String, Option<i64>)> = tx
                .query_row("SELECT kind, released FROM leases WHERE id=?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            match lease {
                None => return Err(StoreError::NotFound(format!("lease {id}"))),
                Some((kind, _)) if kind == "quiet" => {
                    return Err(StoreError::Invalid("use quiet.close"));
                }
                Some((_, Some(_))) => {
                    return Err(refusal(format!("lease {id} is released already")));
                }
                Some((_, None)) => {}
            }
            tx.execute(
                "UPDATE leases SET released=?1, note=?2 WHERE id=?3",
                params![now, args.note, id],
            )?;
            event(
                &tx,
                now,
                &actor,
                "lease.reclaimed",
                id,
                json!({"note": args.note}),
            )?;
        } else {
            return Err(StoreError::Invalid("reclaim takes task:<id> or lease:<id>"));
        }
        tx.commit()?;
        Ok(())
    }

    /// "work.settings" (controller and above): apply a patch of the settings. Returns the settings after it.
    pub fn work_settings_set(
        &mut self,
        now: i64,
        op: &SignedOp,
    ) -> Result<WorkSettings, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "work.settings")?;
        need(role, Role::Controller)?;
        let args: SettingsArgs = serde_json::from_str(&op.arg).map_err(|_| malformed())?;
        let mut settings = settings_in(&tx)?;
        let changes = settings.apply(&args.patch).map_err(StoreError::Refused)?;
        if !changes.is_empty() {
            let stored = serde_json::to_string(&settings)
                .map_err(|_| StoreError::Invalid("settings could not be stored"))?;
            tx.execute(
                "INSERT INTO work_settings(id, json) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
                [stored],
            )?;
            event(
                &tx,
                now,
                &op.actor_hex(),
                "work.settings",
                "",
                json!({
                    "changes": changes,
                    "scope": null,
                    "detail": changes.join(", "),
                }),
            )?;
        }
        tx.commit()?;
        Ok(settings)
    }
}
