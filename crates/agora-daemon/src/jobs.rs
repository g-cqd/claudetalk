//! Job execution (D8, D9): spawn one supervisor per admitted job, finalize the job when its supervisor ends, and
//! adopt the supervisors that outlived the previous daemon.
//!
//! The flows live in the submodules: `start` (launch and the start handshake), `finish` (finalize, sweep, stop,
//! samples) and `adopt` (startup adoption). This file holds the table, the store writes they share, and the pure
//! helpers they and the tests use.

mod adopt;
mod finish;
mod start;
#[cfg(test)]
mod tests;

pub use adopt::adopt_all;
pub use finish::{finalize, refresh_samples, stop, sweep};
pub use start::start;

use crate::system::sign;
use crate::{DaemonError, Shared};
use agora_proto::Event;
use agora_run::RunError;
use agora_run::jobdir::{JobDir, JobStatus, SupervisorInfo, work_root};
use agora_run::proc::alive;
use agora_sched::model::{
    CardSyncArgs, JobFinishArgs, JobNoteArgs, JobOutcome, JobStartArgs, TaskRecord, TaskState,
};
use agora_store::StoreError;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::task::JoinSet;

/// The environment names a job keeps when its registered environment was lost.
const MINIMAL_ENV_NAMES: [&str; 3] = ["PATH", "HOME", "LANG"];

/// The note of a job that runs with the minimal environment (its registered one was lost in a restart).
const MINIMAL_ENV_NOTE: &str = "the daemon restarted before the job started, so its environment was lost; \
     it runs with only PATH, HOME and LANG";

/// How long a job's processes get after TERM before they are killed.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How many times a finish is signed again because the task changed between its read and its write.
const FINISH_ATTEMPTS: usize = 3;

/// Mutable state behind [`JobTable`].
#[derive(Default)]
struct Inner {
    /// Scrubbed environment per task, kept in memory until the supervisor spawns.
    env: HashMap<String, BTreeMap<String, String>>,
    /// Background tasks of the jobs layer (child waiters, adoption watchers). They hold a `Shared`, so
    /// `shutdown` aborts and joins them before the actor threads are joined.
    tasks: JoinSet<()>,
}

/// Process-wide job bookkeeping (cheap to clone; clones share the table and the gate).
#[derive(Clone, Default)]
pub struct JobTable {
    inner: Arc<Mutex<Inner>>,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl JobTable {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock must not poison the table for everyone else.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The start gate. WorkRun holds it from the commit of a new job until its environment is registered, and a
    /// start holds it while it takes that environment, so a job is never started with the minimal environment
    /// because of a race between the two (O-18).
    pub fn gate(&self) -> &tokio::sync::Mutex<()> {
        &self.gate
    }

    /// Run `fut` as a tracked background task: `shutdown` aborts it. Every task of the jobs layer
    /// that holds a `Shared` must be spawned here (or exit on `sh.shutdown`).
    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let mut inner = self.lock();
        // Reap the tasks that finished since the last spawn, so a long-lived daemon keeps no entry per job.
        while inner.tasks.try_join_next().is_some() {}
        inner.tasks.spawn(fut);
    }

    /// Abort and join every tracked task. Called by `Daemon::run` before the `Shared` is dropped.
    pub async fn shutdown(&self) {
        let mut tasks = std::mem::take(&mut self.lock().tasks);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    fn put_env(&self, task: &str, env: BTreeMap<String, String>) {
        self.lock().env.insert(task.to_owned(), env);
    }

    fn take_env(&self, task: &str) -> Option<BTreeMap<String, String>> {
        self.lock().env.remove(task)
    }

    /// Drops the environment kept for a task that ended or will not start.
    fn forget(&self, task: &str) {
        self.lock().env.remove(task);
    }
}

/// Keep `env` for `task` until its supervisor spawns.
pub fn register_env(sh: &Shared, task: &str, env: BTreeMap<String, String>) {
    sh.jobs.put_env(task, env);
}

/// Drops the environment kept for `task` when it will never start (cancelled or failed before its start).
pub fn drop_env(sh: &Shared, task: &str) {
    sh.jobs.forget(task);
}

/// The job directory of `task` under the daemon home.
pub(crate) fn job_dir(sh: &Shared, task: &str) -> Result<JobDir, DaemonError> {
    dir_in(&work_root(&sh.config.home), task)
}

fn dir_in(root: &Path, task: &str) -> Result<JobDir, DaemonError> {
    JobDir::new(root, task).map_err(run_error)
}

fn run_error(e: RunError) -> DaemonError {
    match e {
        RunError::Io(e) => DaemonError::Io(e),
        RunError::Json(e) => DaemonError::Config(format!("a job file is not valid JSON: {e}")),
        RunError::Invalid(message) => DaemonError::Config(message),
    }
}

/// The task as the work queue reports it; `None` when it does not exist.
async fn read_task(sh: &Shared, task: &str) -> Result<Option<TaskRecord>, DaemonError> {
    let id = task.to_owned();
    match sh.reader.exec(move |s| s.work_task(&id)).await {
        Ok(record) => Ok(Some(record)),
        Err(DaemonError::Store(StoreError::NotFound(_))) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What ending a job records, before the task's own note is considered.
#[derive(Debug, Clone, PartialEq)]
struct Finish {
    status: Option<i32>,
    note: String,
    cpu_seconds: Option<f64>,
    rss_peak_kib: Option<u64>,
    ended_ms: i64,
    before_start: bool,
}

impl Finish {
    /// The end of a job as its supervisor reported it. A job whose command never started ends before its start.
    fn from_status(st: &JobStatus) -> Finish {
        Finish {
            status: st.status,
            note: st.note.clone(),
            cpu_seconds: st.cpu_seconds,
            rss_peak_kib: st.rss_peak_kib,
            ended_ms: st.ended_ms,
            before_start: !st.started,
        }
    }
}

/// A task whose job can still be finished: running, or claimed and requested but not yet started.
fn is_open(state: TaskState, requested_ms: Option<i64>) -> bool {
    state == TaskState::Running || (state == TaskState::Claimed && requested_ms.is_some())
}

/// A task that waits for its supervisor's start: claimed, requested and not started (the store's `work_waiting`).
/// Re-checked right before a supervisor spawns, because the task may have been cancelled since admission chose it.
fn awaiting_start(state: TaskState, requested_ms: Option<i64>, started_ms: Option<i64>) -> bool {
    state == TaskState::Claimed && requested_ms.is_some() && started_ms.is_none()
}

/// Whether a failed finish is retried without its card sync (O-19): only a store error of an attempt that carried
/// the sync. A dead writer is not retried.
fn retry_without_sync(synced: bool, e: &DaemonError) -> bool {
    synced && matches!(e, DaemonError::Store(_))
}

/// Whether `pid` is alive and verifiably the process recorded with `start_s`. A pid without its start time may have
/// been reused, so nothing is signalled on it: the cancel file stands instead (R4-5).
fn verified_alive(pid: u32, start_s: Option<u64>) -> bool {
    start_s.is_some() && alive(pid, start_s)
}

/// Whether two reads of a task agree on every field a finish signs.
fn same_reading(a: &TaskRecord, b: &TaskRecord) -> bool {
    a.note == b.note
        && a.log == b.log
        && a.cancel_requested == b.cancel_requested
        && a.card == b.card
}

/// The card outcome of a job (D7), chosen the way the store chooses the job state: a cancel wins, a job that never
/// started fails, status 0 is done, anything else fails.
fn outcome(cancelled: bool, before_start: bool, status: Option<i32>) -> JobOutcome {
    if cancelled {
        JobOutcome::Cancelled
    } else if !before_start && status == Some(0) {
        JobOutcome::Done
    } else {
        JobOutcome::Failed
    }
}

/// The note a finished job keeps: the supervisor's when it has one, else the task's own (a cancel note, or the
/// note of a minimal environment).
fn effective_note(reported: &str, recorded: &str) -> String {
    if reported.is_empty() {
        recorded.to_owned()
    } else {
        reported.to_owned()
    }
}

/// What one finish attempt found.
enum Attempt {
    /// The job was finished (`Some`), or the task was no longer open (`None`).
    Done(Option<Box<TaskRecord>>),
    /// The task changed between the read the ops were signed from and the write: sign again.
    Stale,
}

/// Finishes the open job `task` with `fin` (D7). The task is read and the system ops are signed from that read; the
/// writer then checks the read still holds before it writes `job.finish`, and `card.job_sync` as well when the task is
/// linked to a card. If the card sync fails, the job is finished without it (O-19): the job still ends, and the card
/// stays where it was. A task that is not open is left alone and gives `None`.
async fn finish_running(
    sh: &Shared,
    task: &str,
    fin: Finish,
) -> Result<Option<TaskRecord>, DaemonError> {
    let mut with_sync = true;
    let mut stale = 0;
    let finished = loop {
        let Some(snap) = read_task(sh, task).await? else {
            return Ok(None);
        };
        let synced = with_sync && snap.card.is_some();
        match finish_attempt(sh, &snap, &fin, synced).await {
            Ok(Attempt::Done(finished)) => break finished.map(|task| *task),
            Ok(Attempt::Stale) => {
                stale += 1;
                if stale >= FINISH_ATTEMPTS {
                    return Err(DaemonError::Config(format!(
                        "job {task} kept changing while it was finished; the next sweep finishes it"
                    )));
                }
            }
            Err(e) if retry_without_sync(synced, &e) => {
                tracing::warn!(task = %task, error = %e, "could not finish a job with its card sync; finishing it without the card sync");
                with_sync = false;
            }
            Err(e) => return Err(e),
        }
    };
    sh.jobs.forget(task);
    if finished.is_some() {
        clear_spec_env(sh, task);
    }
    sh.kick.notify_one();
    Ok(finished)
}

/// One finish attempt: `job.finish` and, when `synced`, `card.job_sync`, both signed from `snap` before the writer runs.
/// The writer transaction re-checks the task against `snap` and then writes both ops together.
async fn finish_attempt(
    sh: &Shared,
    snap: &TaskRecord,
    fin: &Finish,
    synced: bool,
) -> Result<Attempt, DaemonError> {
    if !is_open(snap.state, snap.requested_ms) {
        return Ok(Attempt::Done(None));
    }
    let system = sh.system.clone();
    let now = sh.clock.now_ms();
    let id = snap.id.clone();
    let note = effective_note(&fin.note, &snap.note);
    let args = JobFinishArgs {
        status: fin.status,
        note: note.clone(),
        cpu_seconds: fin.cpu_seconds,
        rss_peak_kib: fin.rss_peak_kib,
        ended_ms: fin.ended_ms,
        before_start: fin.before_start,
    };
    let arg = serde_json::to_string(&args)
        .map_err(|_| DaemonError::Config("could not encode job.finish".into()))?;
    let op = sign(&system, now, "job.finish", &id, &arg);
    let sync = match &snap.card {
        Some(card) if synced => {
            let sync_args = CardSyncArgs {
                task: id.clone(),
                outcome: outcome(snap.cancel_requested, fin.before_start, fin.status),
                status: fin.status,
                log: snap.log.clone(),
                note,
            };
            let arg = serde_json::to_string(&sync_args)
                .map_err(|_| DaemonError::Config("could not encode card.job_sync".into()))?;
            Some(sign(&system, now, "card.job_sync", card, &arg))
        }
        _ => None,
    };
    let seen = snap.clone();
    sh.writer
        .exec(move |s| {
            let cur = s.work_task(&id)?;
            if !is_open(cur.state, cur.requested_ms) {
                return Ok((Attempt::Done(None), Vec::new()));
            }
            if !same_reading(&cur, &seen) {
                return Ok((Attempt::Stale, Vec::new()));
            }
            let finished = s.job_finish(now, &op, sync.as_ref())?;
            let mut events = vec![Event::WorkChanged { task: id }];
            if let Some(card) = seen.card {
                events.push(Event::CardChanged { card });
            }
            Ok((Attempt::Done(Some(Box::new(finished))), events))
        })
        .await
}

/// O-21: rewrites the job's spec.json with an empty environment, so the environment does not linger in the job
/// directory until the job is pruned. A job that never wrote a spec has no environment to clear.
fn clear_spec_env(sh: &Shared, task: &str) {
    let Ok(dir) = job_dir(sh, task) else {
        return;
    };
    let Ok(mut spec) = dir.read_spec() else {
        return;
    };
    if spec.env.is_empty() {
        return;
    }
    spec.env.clear();
    if let Err(e) = dir.write_spec(&spec) {
        tracing::warn!(task = %task, error = %e, "could not clear the environment of a job");
    }
}

/// Records a job that never ran as failed before it started: no status, no run time.
async fn finish_unstarted(sh: &Shared, task: &str, note: String) -> Result<(), DaemonError> {
    let fin = Finish {
        status: None,
        note,
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: sh.clock.now_ms(),
        before_start: true,
    };
    finish_running(sh, task, fin).await?;
    Ok(())
}

/// Replaces the note of `task` (`job.note`, a system op) and publishes `WorkChanged`.
async fn set_note(sh: &Shared, task: &str, note: String) -> Result<(), DaemonError> {
    let now = sh.clock.now_ms();
    let arg = serde_json::to_string(&JobNoteArgs { note })
        .map_err(|_| DaemonError::Config("could not encode job.note".into()))?;
    let op = sign(&sh.system, now, "job.note", task, &arg);
    let id = task.to_owned();
    sh.writer
        .exec(move |s| {
            s.job_note(now, &op)?;
            Ok(((), vec![Event::WorkChanged { task: id }]))
        })
        .await
}

/// The `job.start` arguments from the supervisor's report. `None` when it reported no pid for the command.
fn start_args(
    info: &SupervisorInfo,
    exclusive: bool,
    quiet_until_ms: Option<i64>,
    log: &str,
) -> Option<JobStartArgs> {
    Some(JobStartArgs {
        pid: i64::from(info.pid?),
        supervisor_pid: i64::from(info.supervisor_pid),
        supervisor_start_s: info.supervisor_start_s,
        qos: info.qos.clone(),
        exclusive,
        quiet_until_ms,
        log: log.to_owned(),
    })
}

/// The first line of an exclusive job's log (ref-admission): the load, the idle CPU, and the jobs running beside it.
/// Unknown signals are written as such, never as zero.
fn context_line(load1: Option<f64>, idle_pct: Option<f64>, others: &[String]) -> String {
    let load = load1.map_or_else(|| "unknown".to_owned(), |v| format!("{v:.2}"));
    let idle = idle_pct.map_or_else(|| "idle unknown".to_owned(), |v| format!("{v:.0}% idle"));
    let running = if others.is_empty() {
        "no other job".to_owned()
    } else {
        others.join(", ")
    };
    format!("work: context at start: load {load}, CPU {idle}; running: {running}")
}

/// The environment of a job whose registered one was lost: the named variables the daemon has, nothing else.
fn minimal_env(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    MINIMAL_ENV_NAMES
        .into_iter()
        .filter_map(|name| lookup(name).map(|value| (name.to_owned(), value)))
        .collect()
}

/// The environment of `task` for its supervisor: the registered one, taken, or the minimal one when the registration
/// was lost (with the note saying so). The start gate orders this read after WorkRun's commit and registration (O-18).
async fn job_env(sh: &Shared, task: &str) -> (BTreeMap<String, String>, Option<String>) {
    let _gate = sh.jobs.gate().lock().await;
    match sh.jobs.take_env(task) {
        Some(env) => (env, None),
        None => (
            minimal_env(|name| std::env::var(name).ok()),
            Some(MINIMAL_ENV_NOTE.to_owned()),
        ),
    }
}

/// A recorded pid as the u32 the OS uses; a negative or out-of-range value is no pid.
fn recorded_pid(pid: Option<i64>) -> Option<u32> {
    pid.and_then(|p| u32::try_from(p).ok())
}

/// (pid, start time) of a job's supervisor: from supervisor.json, else the recorded pid with no start time.
fn supervisor_of(
    info: Option<&SupervisorInfo>,
    recorded: Option<i64>,
) -> Option<(u32, Option<u64>)> {
    match info {
        Some(info) => Some((info.supervisor_pid, info.supervisor_start_s)),
        None => recorded_pid(recorded).map(|pid| (pid, None)),
    }
}

/// (pid, start time) of a job's command: from supervisor.json, else the recorded pid with no start time.
fn command_of(info: Option<&SupervisorInfo>, recorded: Option<i64>) -> Option<(u32, Option<u64>)> {
    info.and_then(|info| info.pid.map(|pid| (pid, info.pid_start_s)))
        .or_else(|| recorded_pid(recorded).map(|pid| (pid, None)))
}

/// The note of a running job whose supervisor is gone while its command lives on (D8).
fn gone_note(supervisor: u32, command: u32) -> String {
    format!(
        "its supervisor, process {supervisor}, is gone; the command runs on as process {command}, \
         and its status will not be recorded"
    )
}

/// The note of a running job whose supervisor is gone and whose command is gone too (D8).
fn ended_without_status_note(supervisor: u32) -> String {
    format!(
        "the job ended without a recorded status: its supervisor, process {supervisor}, is gone"
    )
}
