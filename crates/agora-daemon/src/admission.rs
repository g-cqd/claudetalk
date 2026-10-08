//! The admission loop: decides which waiting jobs start, using the pure `agora_sched::quiet::decide`.
//!
//! Each pass reads the scheduling state through the reader, takes a machine snapshot on a blocking thread (the
//! macOS pressure probe may run a child process), builds the pure input, and applies the decision: fails and
//! starts are signed system ops, held reasons go to the job notes. Then the loop waits for the earliest of: a
//! kick, a relevant bus event, the decision's next wake-up, a 2 s machine re-check (only while a job is held by a
//! machine gate), or shutdown. Nothing else wakes it.

use crate::clock::Clock;
use crate::{DaemonError, Shared, jobs, system};
use agora_crypto::Identity;
use agora_proto::Event;
use agora_sched::admit::{MachineSnapshot, RunningJob, WaitingJob};
use agora_sched::model::{LeaseKind, LeaseRecord, TaskRecord};
use agora_sched::model_ops::{CardSyncArgs, JobFinishArgs, JobNoteArgs, JobOutcome};
use agora_sched::predict::Level;
use agora_sched::quiet::{AdmitInput, ManualQuiet};
use agora_sched::settings::WorkSettings;
use agora_store::{SignedOp, Store, StoreError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};

/// The machine gate re-probe interval while a job is held by idle, memory, pressure or load.
const MACHINE_RECHECK_MS: i64 = 2_000;
/// Retry interval after a failed pass (a store read or a probe that panicked).
const RETRY_MS: i64 = 2_000;

/// Everything the pure decision needs from the store, read in one reader job.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub settings: WorkSettings,
    pub running: Vec<TaskRecord>,
    pub waiting: Vec<TaskRecord>,
    pub window_closes: Vec<(String, i64)>,
    pub paused: Option<String>,
    pub leases: Vec<LeaseRecord>,
}

/// Loop until shutdown: build `AdmitInput` (reader: work_running, work_waiting, work_window_closes, settings,
/// admission_paused, quiet leases; `machine.snapshot()`; clock), `decide()`, apply: fails via `job.finish`
/// (before_start) system ops, starts via `jobs::start`; then wait for the earliest of: `kick.notified()`, bus
/// `Event::WorkChanged`/`LeaseChanged`/`WorkSettingsChanged`, `clock.sleep_until(next_wakeup)`,
/// `clock.sleep_until(now + 2000)` only if `decision.machine_gated`, shutdown. No other periodic wake. Events
/// queued before a pass are drained first, so one pass covers all of them.
pub async fn run(sh: Shared) {
    if !sh.config.work_enabled {
        sh.shutdown.cancelled().await;
        return;
    }
    // Subscribe before the first pass, so an event published during it is not missed.
    let mut bus = sh.bus.subscribe();
    let clock = sh.clock.clone();
    let mut held_since: HashMap<String, f64> = HashMap::new();
    loop {
        if !drain_queued(&mut bus) {
            return;
        }
        let wake = match pass(&sh, &mut held_since).await {
            Ok(wake) => wake,
            Err(e) => {
                tracing::warn!(error = %e, "admission pass failed; retrying in 2 s");
                Some(clock.now_ms().saturating_add(RETRY_MS))
            }
        };
        tokio::select! {
            () = sh.shutdown.cancelled() => return,
            () = sh.kick.notified() => {}
            open = relevant_event(&mut bus) => {
                if !open {
                    return;
                }
            }
            () = sleep_until_opt(&clock, wake) => {}
        }
    }
}

/// One admission pass. Returns the instant (ms) of the next wake-up that is not an event, if any.
async fn pass(
    sh: &Shared,
    held_since: &mut HashMap<String, f64>,
) -> Result<Option<i64>, DaemonError> {
    // The leases are read as of the moment the snapshot starts. The decision and its records use the time read after
    // the awaited reads, so a job submitted while they ran is judged at the time it is judged (O-41).
    let read_at = sh.clock.now_ms();
    let snap = sh
        .reader
        .exec(move |store: &Store| read_snapshot(store, read_at))
        .await?;
    let machine = sh.machine.clone();
    let machine: MachineSnapshot = tokio::task::spawn_blocking(move || machine.snapshot())
        .await
        .map_err(std::io::Error::other)?;
    let now = sh.clock.now_ms();

    let input = build_input(now, &snap, machine, held_since);
    let d = agora_sched::quiet::decide(&input);

    for (id, since) in &d.machine_held_since {
        match since {
            Some(s) => {
                held_since.insert(id.clone(), *s);
            }
            None => {
                held_since.remove(id);
            }
        }
    }
    let waiting: HashMap<&str, &TaskRecord> =
        snap.waiting.iter().map(|t| (t.id.as_str(), t)).collect();

    // A job that could not be ended is still waiting, and no bus event may come for it: the pass asks for a retry.
    let mut failed = false;
    for f in &d.fail {
        failed |= !fail_before_start(sh, now, &waiting, &f.id, f.note.clone()).await;
    }
    for s in &d.start {
        if sh.shutdown.is_cancelled() {
            break;
        }
        let Some(task) = waiting.get(s.id.as_str()) else {
            tracing::warn!(task = %s.id, "admission started a task that is not waiting; skipped");
            continue;
        };
        let quiet_until_ms = s.quiet_until_s.map(secs_to_ms);
        if let Err(e) = jobs::start(sh, task, s.exclusive, quiet_until_ms).await {
            tracing::warn!(task = %s.id, error = %e, "could not start the supervisor");
            let note = format!("could not start the supervisor: {e}");
            failed |= !fail_before_start(sh, now, &waiting, &s.id, note).await;
        }
    }
    for h in &d.held {
        if let Some(t) = waiting.get(h.id.as_str())
            && t.note != h.reason
        {
            set_note(sh, now, &h.id, h.reason.clone()).await;
        }
    }
    held_since.retain(|id, _| waiting.contains_key(id.as_str()));

    let mut wake = d.next_wakeup_s.map(s_to_ms_ceil);
    if d.machine_gated {
        let recheck = now.saturating_add(MACHINE_RECHECK_MS);
        wake = Some(wake.map_or(recheck, |w| w.min(recheck)));
    }
    Ok(earliest(wake, retry_at(now, failed)))
}

/// The store reads of one pass, in one reader job.
fn read_snapshot(store: &Store, now: i64) -> Result<Snapshot, StoreError> {
    Ok(Snapshot {
        settings: store.work_settings()?,
        running: store.work_running()?,
        waiting: store.work_waiting()?,
        window_closes: store.work_window_closes()?,
        paused: store.admission_paused()?,
        leases: store.leases(now, Some(LeaseKind::Quiet), None, true)?,
    })
}

/// Pure conversion of one pass's inputs into the decision input. `held_since` carries each waiting job's
/// `machine_held_since` (seconds) from the previous pass; jobs it does not name have none.
pub(crate) fn build_input(
    now_ms: i64,
    snap: &Snapshot,
    machine: MachineSnapshot,
    held_since: &HashMap<String, f64>,
) -> AdmitInput {
    AdmitInput {
        now_s: ms_to_s(now_ms),
        settings: snap.settings.clone(),
        machine,
        running: snap.running.iter().map(running_job).collect(),
        waiting: snap
            .waiting
            .iter()
            .map(|t| waiting_job(t, held_since))
            .collect(),
        window_closes: snap
            .window_closes
            .iter()
            .map(|(id, ms)| (id.clone(), ms_to_s(*ms)))
            .collect(),
        paused: snap.paused.clone(),
        manual_quiet: manual_quiet(&snap.leases),
    }
}

fn running_job(t: &TaskRecord) -> RunningJob {
    RunningJob {
        id: t.id.clone(),
        weight: t.weight,
        pred_cores: pred_cores(t),
        pred_rss_p90_kib: t.prediction.as_ref().and_then(|p| p.rss_p90_kib),
        pred_duration_p90_s: t.prediction.as_ref().and_then(|p| p.duration_p90_s),
        rss_kib: t.rss_kib,
        started_s: t.started_ms.map(ms_to_s),
        quiet_until_s: t.quiet_until_ms.map(ms_to_s),
    }
}

fn waiting_job(t: &TaskRecord, held_since: &HashMap<String, f64>) -> WaitingJob {
    WaitingJob {
        id: t.id.clone(),
        kind: t.kind.clone(),
        seq: t.seq,
        // A waiting task always has `requested`; the queue time is only a fallback for a corrupt row.
        requested_s: ms_to_s(t.requested_ms.unwrap_or(t.queued_ms)),
        weight: t.weight,
        pred_cores: pred_cores(t),
        pred_duration_s: t.prediction.as_ref().map(|p| p.duration_s),
        pred_duration_p90_s: t.prediction.as_ref().and_then(|p| p.duration_p90_s),
        pred_rss_p90_kib: t.prediction.as_ref().and_then(|p| p.rss_p90_kib),
        machine_held_since_s: held_since.get(&t.id).copied(),
    }
}

/// The predicted cores of a task (work.py `estimated`). A prediction with a level is used as it is. Otherwise,
/// when the last CPU sample exists, the estimate is `min(weight, max(0.1, cpu / 100))`. Otherwise the stored
/// prediction's cores, or None (the declared weight counts) when there is no prediction.
fn pred_cores(t: &TaskRecord) -> Option<f64> {
    match &t.prediction {
        Some(p) if p.level != Level::None => Some(p.cores),
        _ => match t.cpu_percent {
            Some(cpu) => Some(t.weight.min((cpu / 100.0).max(0.1))),
            None => t.prediction.as_ref().map(|p| p.cores),
        },
    }
}

/// Active quiet leases with an expected end; a lease without one cannot bound a window and is left out.
fn manual_quiet(leases: &[LeaseRecord]) -> Vec<ManualQuiet> {
    leases
        .iter()
        .filter_map(|l| {
            Some(ManualQuiet {
                lease: l.id.clone(),
                holder_name: l.holder_name.clone(),
                until_s: ms_to_s(l.expected_end_ms?),
                note: l.note.clone(),
            })
        })
        .collect()
}

/// The card outcome of a job, chosen the way the store chooses the final task state in `job.finish`: a cancel wins,
/// a job that ends before its start fails, status 0 is done, anything else fails.
fn sync_outcome(cancel_requested: bool, before_start: bool, status: Option<i32>) -> JobOutcome {
    if cancel_requested {
        JobOutcome::Cancelled
    } else if !before_start && status == Some(0) {
        JobOutcome::Done
    } else {
        JobOutcome::Failed
    }
}

/// Ends a waiting job before its start: `job.finish` (before_start) with `note`, and `card.job_sync` when the task is
/// linked to a card (D7), in one transaction. A task with a card whose write fails with a store error is retried once
/// without the card sync, with the ops signed afresh (O-19), so the job still ends. Returns whether the job was
/// ended. When it was not, the caller asks for a retry, since the job is still waiting and no event may come for it.
/// On success the job's environment is dropped, since the job will never start.
async fn fail_before_start(
    sh: &Shared,
    now: i64,
    waiting: &HashMap<&str, &TaskRecord>,
    id: &str,
    note: String,
) -> bool {
    let task_row = waiting.get(id);
    let card = task_row.and_then(|t| t.card.clone());
    let outcome = sync_outcome(task_row.is_some_and(|t| t.cancel_requested), true, None);
    let mut with_sync = card.is_some();
    loop {
        let attempt_card = if with_sync { card.clone() } else { None };
        let attempt =
            match sign_before_start(&sh.system, now, id, &note, outcome, attempt_card.as_deref()) {
                Ok(ops) => write_before_start(sh, now, id, attempt_card, ops).await,
                Err(e) => Err(e),
            };
        match attempt {
            Ok(()) => {
                jobs::drop_env(sh, id);
                return true;
            }
            Err(e) if retry_without_sync(with_sync, &e) => {
                tracing::warn!(task = %id, error = %e, "could not end a job with its card sync; ending it without the card sync");
                with_sync = false;
            }
            Err(e) => {
                tracing::warn!(task = %id, error = %e, "could not record the job as failed before it started");
                return false;
            }
        }
    }
}

/// Signs the ops that end a waiting job before its start: `job.finish` for the task, and `card.job_sync` when `card`
/// is set. The sync's subject is the card, as `jobs::finish_attempt` signs it; the job's own op names the task.
fn sign_before_start(
    system: &Identity,
    now: i64,
    task: &str,
    note: &str,
    outcome: JobOutcome,
    card: Option<&str>,
) -> Result<(SignedOp, Option<SignedOp>), DaemonError> {
    let args = JobFinishArgs {
        status: None,
        note: note.to_owned(),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: now,
        before_start: true,
    };
    let arg = serde_json::to_string(&args)
        .map_err(|_| DaemonError::Config("could not encode job.finish".into()))?;
    let finish = system::try_sign(system, now, "job.finish", task, &arg)?;
    let sync = match card {
        Some(card) => {
            let args = CardSyncArgs {
                task: task.to_owned(),
                outcome,
                status: None,
                log: None,
                note: note.to_owned(),
            };
            let arg = serde_json::to_string(&args)
                .map_err(|_| DaemonError::Config("could not encode card.job_sync".into()))?;
            Some(system::try_sign(system, now, "card.job_sync", card, &arg)?)
        }
        None => None,
    };
    Ok((finish, sync))
}

/// The writer transaction of [`fail_before_start`]: the job ends, and the card changes when its sync was written.
async fn write_before_start(
    sh: &Shared,
    now: i64,
    task: &str,
    card: Option<String>,
    (op, sync): (SignedOp, Option<SignedOp>),
) -> Result<(), DaemonError> {
    let task = task.to_owned();
    sh.writer
        .exec(move |store: &mut Store| {
            store.job_finish(now, &op, sync.as_ref())?;
            let mut events = vec![Event::WorkChanged { task }];
            if let Some(card) = card {
                events.push(Event::CardChanged { card });
            }
            Ok(((), events))
        })
        .await
}

/// Whether a failed write that carried a card sync is retried without it (O-19): only a store error, since a sync the
/// store refused is what the retry sheds. A dead writer, or an encoding or signing failure, is not retried this way.
fn retry_without_sync(synced: bool, e: &DaemonError) -> bool {
    synced && matches!(e, DaemonError::Store(_))
}

/// The wake-up a pass asks for after a failed write: `RETRY_MS` after `now_ms`, or none when nothing failed.
fn retry_at(now_ms: i64, failed: bool) -> Option<i64> {
    failed.then(|| now_ms.saturating_add(RETRY_MS))
}

/// The earlier of two optional wake-ups; a missing one does not count as the earliest.
fn earliest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    a.into_iter().chain(b).min()
}

/// Replace a waiting job's note with its held reason. No event: the reason can change on every pass, and a
/// published event would wake the loop again.
async fn set_note(sh: &Shared, now: i64, id: &str, note: String) {
    let Ok(arg) = serde_json::to_string(&JobNoteArgs { note }) else {
        tracing::warn!(task = %id, "could not encode job.note");
        return;
    };
    let op = system::sign(&sh.system, now, "job.note", id, &arg);
    let result = sh
        .writer
        .exec(move |store: &mut Store| store.job_note(now, &op).map(|()| ((), Vec::new())))
        .await;
    if let Err(e) = result {
        tracing::warn!(task = %id, error = %e, "could not update the held reason");
    }
}

/// Drops the bus events already queued. Each was published after its store write, and the pass that follows reads
/// the store after this, so it covers them. A lagged receiver lost events, and the pass re-reads everything anyway.
/// Returns false when the bus is closed.
fn drain_queued(bus: &mut broadcast::Receiver<Event>) -> bool {
    loop {
        match bus.try_recv() {
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Closed) => return false,
        }
    }
}

/// Waits for the next `WorkChanged`, `LeaseChanged` or `WorkSettingsChanged`. A lagged receiver missed some
/// events, so it counts as one (the pass re-reads everything). Returns false when the bus is closed.
async fn relevant_event(bus: &mut broadcast::Receiver<Event>) -> bool {
    loop {
        match bus.recv().await {
            Ok(
                Event::WorkChanged { .. } | Event::LeaseChanged { .. } | Event::WorkSettingsChanged,
            ) => {
                return true;
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => return true,
            Err(RecvError::Closed) => return false,
        }
    }
}

/// Sleeps until `at` when there is one, and never otherwise.
async fn sleep_until_opt(clock: &Arc<dyn Clock>, at: Option<i64>) {
    match at {
        Some(at) => clock.sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn ms_to_s(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Rounds up, so a wake-up never lands before the instant it was computed for.
fn s_to_ms_ceil(s: f64) -> i64 {
    (s * 1000.0).ceil() as i64
}

/// Nearest millisecond, so a stored window end survives the seconds round trip.
fn secs_to_ms(s: f64) -> i64 {
    (s * 1000.0).round() as i64
}

#[cfg(test)]
mod tests;
