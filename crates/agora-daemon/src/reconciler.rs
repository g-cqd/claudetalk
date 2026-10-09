//! The reconciler: periodic, idempotent maintenance (leases, stale cards, WIP, floors, sprints, retention).
//!
//! Each pass reads a snapshot, plans with the pure `agora_sched::reconcile::plan`, and applies every action as a
//! signed system op. The store's marks make a repeated action a no-op, so a pass can run again at any time.

use crate::{DaemonError, Shared, jobs, system};
use agora_proto::Event;
use agora_run::jobdir::{JobDir, log_path, work_root};
use agora_sched::reconcile::{ReconcileAction, ReconcileInput};
use agora_sched::settings::WorkSettings;
use agora_store::Store;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::Notify;

/// A kick closer than this to the last pass waits until it has passed.
const KICK_DEBOUNCE_MS: i64 = 5_000;
/// Retention runs at most this often.
const PRUNE_EVERY_MS: i64 = 3_600_000;
const DAY_SECONDS: f64 = 86_400.0;

/// Last retention run per daemon home. It lives in memory: a restart prunes again at its first pass.
static LAST_PRUNE: Mutex<BTreeMap<PathBuf, i64>> = Mutex::new(BTreeMap::new());

/// Loop every `settings.reconcile_interval_s` via the clock (also on `kick`, debounced to once per 5 s):
/// `jobs::refresh_samples`, `jobs::sweep`, build the `ReconcileInput` (machine snapshot), `plan()`, sign and
/// apply each action as a system op, publish events (NoticePosted, CardChanged, LeaseChanged,
/// WorkSettingsChanged for an admission pause, WorkChanged for an expired claim), kick admission on
/// Admission/CloseQuiet/ExpireLease/ExpireClaim, retention
/// (`work_prune` + delete job dirs/logs of the returned ids) at most once per hour.
pub async fn run(sh: Shared, kick: Arc<Notify>) {
    if !sh.config.work_enabled {
        sh.shutdown.cancelled().await;
        return;
    }
    // Let the startup tasks begin (adoption runs first) before the first pass.
    tokio::task::yield_now().await;
    let clock = sh.clock.clone();
    loop {
        match pass(&sh).await {
            Ok(_) => {}
            Err(DaemonError::ActorGone) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reconciler pass failed; retrying at the next tick")
            }
        }
        let done = clock.now_ms();
        let interval = interval_ms(reconcile_interval_s(&sh).await);
        let next = done.saturating_add(interval);
        tokio::select! {
            () = sh.shutdown.cancelled() => return,
            () = clock.sleep_until(next) => {}
            () = kick.notified() => {
                if let Some(until) = debounce_deadline(clock.now_ms(), done) {
                    tokio::select! {
                        () = sh.shutdown.cancelled() => return,
                        () = clock.sleep_until(until) => {}
                    }
                }
            }
        }
    }
}

/// One pass (used by tests through the WatchdogReport path or directly): returns the actions applied.
pub async fn pass(sh: &Shared) -> Result<Vec<ReconcileAction>, DaemonError> {
    let now = sh.clock.now_ms();
    jobs::refresh_samples(sh).await;
    jobs::sweep(sh).await;
    let input = snapshot(sh, now).await?;
    let actions = agora_sched::reconcile::plan(&input);
    let mut applied = Vec::with_capacity(actions.len());
    for action in actions {
        match apply(sh, now, &input, &action).await {
            Ok(true) => {
                if kicks_admission(&action) {
                    sh.kick.notify_one();
                }
                applied.push(action);
            }
            // Marked since the snapshot: another pass applied it first.
            Ok(false) => {}
            Err(e) => tracing::warn!(
                key = action.key(),
                op = action.op_kind(),
                error = %e,
                "reconcile action failed; continuing with the rest"
            ),
        }
    }
    prune_if_due(sh, now, &input.settings).await;
    Ok(applied)
}

/// The reconciler's input: the store snapshot plus a machine probe on a blocking thread. A failed probe leaves
/// the floors unchecked for this pass; the other rules still run.
async fn snapshot(sh: &Shared, now: i64) -> Result<ReconcileInput, DaemonError> {
    let machine = sh.machine.clone();
    let machine = match tokio::task::spawn_blocking(move || machine.snapshot()).await {
        Ok(snapshot) => Some(snapshot),
        Err(e) => {
            tracing::warn!(error = %e, "machine probe failed; machine floors are not checked this pass");
            None
        }
    };
    let require_review = sh.config.require_review;
    sh.reader
        .exec(move |store: &Store| store.reconcile_input(now, machine, require_review))
        .await
}

/// Signs one action as a system op and applies it. `Ok(true)` when it changed state (its bus events are
/// published by the writer, in commit order), `Ok(false)` when its key was already marked.
async fn apply(
    sh: &Shared,
    now: i64,
    input: &ReconcileInput,
    action: &ReconcileAction,
) -> Result<bool, DaemonError> {
    let arg = serde_json::to_string(action).map_err(std::io::Error::other)?;
    let op = system::sign(&sh.system, now, action.op_kind(), action.key(), &arg);
    let action = action.clone();
    let known_blocker = input.blocker_card.clone();
    let require_review = sh.config.require_review;
    sh.writer
        .exec(move |store: &mut Store| {
            if !store.apply_reconcile(now, &op)? {
                return Ok((false, Vec::new()));
            }
            // A newly opened blocker gets its id from the store; the snapshot knew none.
            let card = match (&action, known_blocker) {
                (ReconcileAction::Blocker { open: true, .. }, None) => {
                    store
                        .reconcile_input(now, None, require_review)?
                        .blocker_card
                }
                (_, known) => known,
            };
            Ok((true, events_for(&action, card.as_deref())))
        })
        .await
}

/// The bus events an applied action produces, so watchers of the touched entity wake up.
fn events_for(action: &ReconcileAction, blocker_card: Option<&str>) -> Vec<Event> {
    match action {
        ReconcileAction::ExpireLease { lease, .. } | ReconcileAction::CloseQuiet { lease, .. } => {
            vec![Event::LeaseChanged {
                lease: lease.clone(),
            }]
        }
        // The notice's sequence number is not returned by the reconciler's apply path.
        ReconcileAction::Notice { target, .. } => vec![Event::NoticePosted {
            target: target.clone(),
            seq: 0,
        }],
        ReconcileAction::ReclaimCard { card, .. } => {
            vec![Event::CardChanged { card: card.clone() }]
        }
        ReconcileAction::Blocker { .. } => blocker_card
            .map(|card| Event::CardChanged {
                card: card.to_owned(),
            })
            .into_iter()
            .collect(),
        ReconcileAction::Admission { .. } => vec![Event::WorkSettingsChanged],
        ReconcileAction::ExpireClaim { task, .. } => {
            vec![Event::WorkChanged { task: task.clone() }]
        }
    }
}

/// Actions after which admission may be able to start or stop a job.
fn kicks_admission(action: &ReconcileAction) -> bool {
    matches!(
        action,
        ReconcileAction::Admission { .. }
            | ReconcileAction::CloseQuiet { .. }
            | ReconcileAction::ExpireLease { .. }
            | ReconcileAction::ExpireClaim { .. }
    )
}

/// The reconciler's sleep: `reconcile_interval_s` from the store, or the default when it cannot be read.
async fn reconcile_interval_s(sh: &Shared) -> f64 {
    match sh.reader.exec(|store: &Store| store.work_settings()).await {
        Ok(settings) => settings.reconcile_interval_s,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the reconcile interval; using the default");
            WorkSettings::default().reconcile_interval_s
        }
    }
}

/// Milliseconds for a reconcile interval in seconds. A value that is not finite or below one second gives the
/// default interval.
fn interval_ms(reconcile_interval_s: f64) -> i64 {
    let seconds = if reconcile_interval_s.is_finite() && reconcile_interval_s >= 1.0 {
        reconcile_interval_s
    } else {
        WorkSettings::default().reconcile_interval_s
    };
    seconds_to_ms(seconds)
}

/// When a kick that arrives at `now` may start a pass: `None` at once, or the instant of the debounce end when
/// the last pass (finished at `last_pass_ms`) was less than five seconds ago.
fn debounce_deadline(now: i64, last_pass_ms: i64) -> Option<i64> {
    let until = last_pass_ms.saturating_add(KICK_DEBOUNCE_MS);
    (now < until).then_some(until)
}

/// Whether retention is due: never run in this process, an hour or more since the last run, or the clock went
/// back before the last run.
fn prune_due(last_ms: Option<i64>, now: i64) -> bool {
    last_ms.is_none_or(|last| now < last || now.saturating_sub(last) >= PRUNE_EVERY_MS)
}

/// The cutoff for retention: `keep_days` before `now`. A `keep_days` that is not a positive finite number
/// falls back to the default; a huge one prunes nothing rather than everything.
fn prune_cutoff(now: i64, keep_days: f64) -> i64 {
    let days = if keep_days.is_finite() && keep_days > 0.0 {
        keep_days
    } else {
        WorkSettings::default().keep_days
    };
    now.saturating_sub(seconds_to_ms(days * DAY_SECONDS))
}

/// Seconds as milliseconds; a value out of range saturates to `i64::MAX`.
fn seconds_to_ms(seconds: f64) -> i64 {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

/// Runs retention when it is due (D17): the store forgets the ended tasks older than `keep_days` (their events
/// and log pointer) and returns their ids, then their job dirs and logs are deleted. A failure is logged and
/// never fails the pass.
async fn prune_if_due(sh: &Shared, now: i64, settings: &WorkSettings) {
    let home = sh.config.home.clone();
    if !prune_due(last_prune(&home), now) {
        return;
    }
    // Recorded before the work, so a failing store is retried at the next hour, not at the next pass.
    lock_last_prune().insert(home.clone(), now);
    let cutoff = prune_cutoff(now, settings.keep_days);
    let ids = match sh
        .writer
        .exec(move |store: &mut Store| Ok((store.work_prune(cutoff)?, Vec::new())))
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "retention could not prune the ended tasks");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    let root = work_root(&home);
    let cleanup = tokio::task::spawn_blocking(move || {
        for id in &ids {
            if let Err(e) = remove_task_files(&root, id) {
                tracing::warn!(task = %id, error = %e, "could not delete a pruned task's files");
            }
        }
    })
    .await;
    if let Err(e) = cleanup {
        tracing::warn!(error = %e, "retention file cleanup stopped");
    }
}

fn lock_last_prune() -> MutexGuard<'static, BTreeMap<PathBuf, i64>> {
    // A panic while holding the lock must not stop retention for good.
    LAST_PRUNE.lock().unwrap_or_else(|p| p.into_inner())
}

fn last_prune(home: &Path) -> Option<i64> {
    lock_last_prune().get(home).copied()
}

/// Deletes the job dir and the log of one pruned task under `root` (`<home>/work`). Both paths are rebuilt from
/// the id, which must be a valid task name. A symlink or another non-regular file where a log belongs is refused,
/// never followed. Both removals are attempted; the first error is returned.
fn remove_task_files(root: &Path, task: &str) -> Result<(), std::io::Error> {
    let job = JobDir::new(root, task).map_err(|e| std::io::Error::other(e.to_string()))?;
    let dir = job
        .remove()
        .map_err(|e| std::io::Error::other(e.to_string()));
    let log = remove_log(&log_path(root, task)).map(|_| ());
    dir.and(log)
}

/// Removes a task log that is a regular file. A missing log is not an error (`Ok(false)`); a symlink or a
/// directory is an error and is left alone.
fn remove_log(path: &Path) -> Result<bool, std::io::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            std::fs::remove_file(path)?;
            Ok(true)
        }
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is not a regular file; refusing to delete it",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests;
