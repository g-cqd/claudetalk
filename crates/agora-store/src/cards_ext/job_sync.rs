//! D7: the card side of a job's end. `card.job_sync` is a system-only operation: it moves
//! the card along one of the sync edges, or leaves it and tells the owner why.

use super::{CardFields, load_card, supervisors};
use crate::identity::{active_role, unhex32};
use crate::policy::post_notice;
use crate::system::authorize_system;
use crate::{SignedOp, Store, StoreError};
use agora_core::{CardState, Role};
use agora_sched::model::{CardSyncArgs, JobOutcome, TaskState};
use rusqlite::{OptionalExtension, Transaction, params};

/// D7, called by `job_finish` inside its transaction. The system identity only; the card
/// moves along one of the sync edges, or, when the card is in another state, nothing moves
/// and the owner gets a notice saying why. The sync moves the card only while the task's
/// author still owns it or the author is a supervisor or higher (R3-2). `task_id` and
/// `final_state` are the job being finished and the state `job_finish` gave it; the sync
/// must name that job and carry the outcome that corresponds to that state (R3-4).
pub(crate) fn card_job_sync_tx(
    tx: &Transaction,
    now: i64,
    op: &SignedOp,
    task_id: &str,
    final_state: TaskState,
) -> Result<(), StoreError> {
    authorize_system(tx, now, op, "card.job_sync")?;
    let args: CardSyncArgs = serde_json::from_str(&op.arg)
        .map_err(|_| StoreError::Invalid("card job sync arg must be JSON"))?;
    if args.task != task_id || outcome_state(args.outcome) != final_state {
        return Err(StoreError::Invalid(
            "card.job_sync does not match the job it finishes",
        ));
    }
    let linked: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT card, author FROM work_tasks WHERE id=?",
            [args.task.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (linked, author) =
        linked.ok_or_else(|| StoreError::NotFound(format!("task {}", args.task)))?;
    let Some(card) = linked else {
        return Ok(());
    };
    let c = load_card(tx, &card)?;
    let state = c.state()?;
    let actor = op.actor_hex();
    let outcome = outcome_word(args.outcome);
    let trusted = author_may_move(tx, c.owner.as_deref(), &author)?;
    let Some(steps) = sync_path(args.outcome, state).filter(|_| trusted) else {
        return skip_notice(tx, now, &actor, &card, &c, &args.task, outcome);
    };
    let mut from = state;
    for to in steps {
        sync_move(tx, now, &actor, &card, &args.task, from, to)?;
        from = to;
    }
    let status = args
        .status
        .map_or_else(|| "none".to_owned(), |s| s.to_string());
    let log = args.log.as_deref().unwrap_or("-");
    match args.outcome {
        JobOutcome::Done => {
            record_evidence(
                tx,
                now,
                &actor,
                &card,
                &format!("job:{} status={status} log={log}", args.task),
            )?;
        }
        JobOutcome::Failed => {
            let action = format!(
                "job {} failed with status {status}: {}; inspect {log} and re-run or cancel",
                args.task, args.note
            );
            tx.execute(
                "UPDATE cards SET needs_owner=1, owner_action=? WHERE id=?",
                params![action, card],
            )?;
            record_evidence(
                tx,
                now,
                &actor,
                &card,
                &format!("job:{} status={status} log={log}", args.task),
            )?;
        }
        JobOutcome::Cancelled => {}
    }
    Ok(())
}

/// The moves a job outcome makes from a card state, or `None` when the edge is not one of
/// the D7 edges: done → Review (through Doing from Claimed), failed → Blocked, cancelled → Ready.
fn sync_path(outcome: JobOutcome, state: CardState) -> Option<Vec<CardState>> {
    Some(match (outcome, state) {
        (JobOutcome::Done, CardState::Claimed) => vec![CardState::Doing, CardState::Review],
        (JobOutcome::Done, CardState::Doing) => vec![CardState::Review],
        (JobOutcome::Failed, CardState::Claimed | CardState::Doing | CardState::Review) => {
            vec![CardState::Blocked]
        }
        (JobOutcome::Cancelled, CardState::Claimed | CardState::Doing | CardState::Review) => {
            vec![CardState::Ready]
        }
        _ => return None,
    })
}

/// R3-2: the task's author may move the card when the author still owns it, or when the author
/// is an active identity with role Supervisor or higher. An unknown, revoked or non-key author
/// may not move it; a database error is still an error.
fn author_may_move(
    tx: &Transaction,
    owner: Option<&str>,
    author: &str,
) -> Result<bool, StoreError> {
    if owner == Some(author) {
        return Ok(true);
    }
    let Some(pk) = unhex32(author) else {
        return Ok(false);
    };
    match active_role(tx, &pk) {
        Ok(role) => Ok(role >= Role::Supervisor),
        Err(StoreError::Invalid(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// One sync move: state, owner (cleared on Ready), approvals (cleared on Doing and Ready,
/// as `transition` does), and a `card.moved` event.
fn sync_move(
    tx: &Transaction,
    now: i64,
    actor: &str,
    card: &str,
    job: &str,
    from: CardState,
    to: CardState,
) -> Result<(), StoreError> {
    if matches!(to, CardState::Doing | CardState::Ready) {
        tx.execute("DELETE FROM approvals WHERE card=?", [card])?;
    }
    tx.execute(
        "UPDATE cards SET state=?1, owner=CASE WHEN ?2 THEN NULL ELSE owner END, updated=?3 WHERE id=?4",
        params![to.as_str(), to == CardState::Ready, now, card],
    )?;
    Store::append(
        tx,
        now,
        actor,
        "card.moved",
        card,
        &serde_json::json!({"from": from.as_str(), "to": to.as_str(), "job": job}),
    )?;
    Ok(())
}

fn record_evidence(
    tx: &Transaction,
    now: i64,
    actor: &str,
    card: &str,
    reference: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT OR IGNORE INTO evidence(card, ref, by) VALUES (?,?,?)",
        params![card, reference, actor],
    )?;
    Store::append(
        tx,
        now,
        actor,
        "card.evidence",
        card,
        &serde_json::json!({"ref": reference}),
    )?;
    Ok(())
}

/// Skipped sync: the owner (or every supervisor, when unowned) hears why the card did not move.
fn skip_notice(
    tx: &Transaction,
    now: i64,
    actor: &str,
    card: &str,
    c: &CardFields,
    job: &str,
    outcome: &str,
) -> Result<(), StoreError> {
    let targets = match &c.owner {
        Some(owner) => vec![owner.clone()],
        None => supervisors(tx)?,
    };
    let text = format!(
        "job {job} ended {outcome} but card {card} ({}) is {}, so the card was not moved",
        c.title, c.state
    );
    for target in &targets {
        post_notice(tx, now, target, "job_sync", card, &text)?;
    }
    Store::append(
        tx,
        now,
        actor,
        "card.sync_skipped",
        card,
        &serde_json::json!({"job": job, "outcome": outcome, "state": c.state, "notified": targets.len()}),
    )?;
    Ok(())
}

/// The job state an outcome corresponds to: Done, Failed or Cancelled (R3-4).
fn outcome_state(outcome: JobOutcome) -> TaskState {
    match outcome {
        JobOutcome::Done => TaskState::Done,
        JobOutcome::Failed => TaskState::Failed,
        JobOutcome::Cancelled => TaskState::Cancelled,
    }
}

fn outcome_word(outcome: JobOutcome) -> &'static str {
    match outcome {
        JobOutcome::Done => "done",
        JobOutcome::Failed => "failed",
        JobOutcome::Cancelled => "cancelled",
    }
}
