//! Starting a job: prepare its directory, spawn its supervisor, and record `job.start` once the supervisor has reported
//! the command it started (D8). A failure before the command runs is recorded as a failed start, never left waiting.

use super::finish::finalize;
use super::{
    TERM_GRACE, awaiting_start, context_line, dir_in, drop_env, finish_unstarted, job_env,
    read_task, run_error, set_note, start_args,
};
use crate::system::sign;
use crate::{DaemonError, Shared};
use agora_proto::Event;
use agora_run::jobdir::{JobDir, JobSpec, SupervisorInfo, create_log, log_path, work_root};
use agora_run::proc::{Sig, signal_pid};
use agora_run::spawn::spawn_supervisor;
use agora_sched::model::{JobStartArgs, TaskRecord};
use std::time::Duration;
use tokio::process::Child;

/// How long a new supervisor may take to report that it started, or that it did not. Thirty seconds covers a
/// supervisor exec under heavy load.
const START_WAIT_MS: i64 = 30_000;

/// The longest the start wait sleeps before it re-checks, when no file event (or child exit) arrives first.
const POLL: Duration = Duration::from_millis(500);

/// Starts an admitted task: its job directory, log and spec, then its supervisor. Once the supervisor reports the
/// command, `job.start` records it and a tracked task finalizes the job when the supervisor exits. Failures before
/// the command runs finish the job as failed before its start and return `Ok`, so the caller never retries it. A task
/// that stopped waiting since admission gets no supervisor.
pub async fn start(
    sh: &Shared,
    task: &TaskRecord,
    exclusive: bool,
    quiet_until_ms: Option<i64>,
) -> Result<(), DaemonError> {
    let id = task.id.clone();
    let launch = match prepare_and_spawn(sh, task, exclusive).await {
        Ok(Some(launch)) => launch,
        Ok(None) => {
            drop_env(sh, &id);
            return Ok(());
        }
        Err(e) => {
            finish_unstarted(sh, &id, format!("it could not be started: {e}")).await?;
            return Ok(());
        }
    };
    let Launch {
        dir,
        log,
        mut child,
        env_note,
    } = launch;
    let ready = match wait_for_supervisor(sh, &dir, &mut child).await {
        Ok(ready) => ready,
        Err(e) => {
            end_supervisor(sh, &id, &dir, child);
            finish_unstarted(sh, &id, format!("its supervisor could not be watched: {e}")).await?;
            return Ok(());
        }
    };
    match ready {
        Ready::Info(info) => match start_args(&info, exclusive, quiet_until_ms, &log) {
            Some(args) => start_running(sh, &id, &dir, child, args, env_note).await,
            None => {
                end_supervisor(sh, &id, &dir, child);
                finish_unstarted(
                    sh,
                    &id,
                    "its supervisor reported no pid for the command".into(),
                )
                .await
            }
        },
        Ready::Status => {
            reap_then_finalize(sh, id.clone(), child);
            finalize(sh, &id).await
        }
        Ready::Exited(reason) => finish_unstarted(sh, &id, reason).await,
        Ready::Shutdown => {
            tracing::info!(task = %id, "the daemon stopped while a job waited for its supervisor; the job is adopted on restart");
            Ok(())
        }
        Ready::TimedOut => {
            end_supervisor(sh, &id, &dir, child);
            finish_unstarted(
                sh,
                &id,
                format!(
                    "its supervisor did not start within {} s",
                    START_WAIT_MS / 1000
                ),
            )
            .await
        }
    }
}

/// The supervisor reported the command: record it as started. If the store refuses the start, the supervisor is
/// ended, so a command that is not recorded as running does not run on.
async fn start_running(
    sh: &Shared,
    id: &str,
    dir: &JobDir,
    child: Child,
    args: JobStartArgs,
    env_note: Option<String>,
) -> Result<(), DaemonError> {
    match record_start(sh, id, args).await {
        Ok(_) => {
            if let Some(note) = env_note
                && let Err(e) = set_note(sh, id, note).await
            {
                tracing::warn!(task = %id, error = %e, "could not record the environment note of a job");
            }
            reap_then_finalize(sh, id.to_owned(), child);
            Ok(())
        }
        Err(e) => {
            end_supervisor(sh, id, dir, child);
            Err(e)
        }
    }
}

/// Everything a launched supervisor needs from the daemon: its job directory, its log, the child handle, and the
/// note to record when the job runs with the minimal environment.
struct Launch {
    dir: JobDir,
    log: String,
    child: Child,
    env_note: Option<String>,
}

/// The job directory, the log (truncated), the spec (environment, PWD, and for exclusive jobs the context line), and
/// the supervisor process. `None` when the task no longer waits for its start: nothing is spawned, and the job
/// directory written so far is removed (R4-2).
async fn prepare_and_spawn(
    sh: &Shared,
    task: &TaskRecord,
    exclusive: bool,
) -> Result<Option<Launch>, DaemonError> {
    let argv = task
        .command
        .clone()
        .filter(|argv| !argv.is_empty())
        .ok_or_else(|| DaemonError::Config(format!("task {} has no command to run", task.id)))?;
    let cwd = task
        .cwd
        .clone()
        .ok_or_else(|| DaemonError::Config(format!("task {} has no working directory", task.id)))?;
    let root = work_root(&sh.config.home);
    let dir = dir_in(&root, &task.id)?;
    let settings = sh.reader.exec(|s| s.work_settings()).await?;
    let context_start = if exclusive {
        Some(context_start(sh, &task.id).await?)
    } else {
        None
    };
    let (mut env, env_note) = job_env(sh, &task.id).await;
    env.insert("PWD".to_owned(), cwd.clone());
    let log = log_path(&root, &task.id);
    let log_text = log
        .to_str()
        .ok_or_else(|| DaemonError::Config("the log path is not valid UTF-8".into()))?
        .to_owned();
    dir.create().map_err(run_error)?;
    clear_reports(&dir)?;
    create_log(&log).map_err(run_error)?;
    let spec = JobSpec {
        task: task.id.clone(),
        argv,
        cwd,
        env,
        log: log_text.clone(),
        log_limit: settings.log_limit,
        qos: settings.job_qos,
        exclusive,
        context_start,
    };
    dir.write_spec(&spec).map_err(run_error)?;
    if !still_waiting(sh, &task.id).await? {
        // Nothing is spawned, so the spec with its environment goes too.
        let _ = dir.remove();
        return Ok(None);
    }
    let supervisor = sh.config.supervisor_command()?;
    let child = spawn_supervisor(&supervisor, &dir)?;
    Ok(Some(Launch {
        dir,
        log: log_text,
        child,
        env_note,
    }))
}

/// C1-M1: removes what an earlier attempt at this job left in its directory (supervisor.json, status.json and the cancel
/// file), so the attempt about to spawn does not read them as its own. A file that is not there is nothing to remove.
fn clear_reports(dir: &JobDir) -> Result<(), DaemonError> {
    for name in ["supervisor.json", "status.json", "cancel"] {
        match std::fs::remove_file(dir.dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(DaemonError::Io(e)),
        }
    }
    Ok(())
}

/// Whether `task` still waits for its start, read now (R4-2). A task cancelled or started since admission chose it
/// must not get a supervisor.
async fn still_waiting(sh: &Shared, task: &str) -> Result<bool, DaemonError> {
    Ok(read_task(sh, task)
        .await?
        .is_some_and(|t| awaiting_start(t.state, t.requested_ms, t.started_ms)))
}

/// The "work: context at start" line of an exclusive job. The machine probe may block (sysctl), so it runs on the
/// blocking pool.
async fn context_start(sh: &Shared, task: &str) -> Result<String, DaemonError> {
    let machine = sh.machine.clone();
    let snapshot = tokio::task::spawn_blocking(move || machine.snapshot())
        .await
        .map_err(|e| DaemonError::Config(format!("the machine probe failed: {e}")))?;
    let (load1, idle_pct) = snapshot
        .signals
        .map_or((None, None), |signals| (signals.load1, signals.idle_pct));
    let id = task.to_owned();
    let running = sh.reader.exec(move |s| s.work_running()).await?;
    let others: Vec<String> = running
        .into_iter()
        .map(|record| record.id)
        .filter(|other| *other != id)
        .collect();
    Ok(context_line(load1, idle_pct, &others))
}

/// How the start handshake ended.
enum Ready {
    /// The supervisor reported the command it started (supervisor.json).
    Info(SupervisorInfo),
    /// The supervisor finished without starting the command (status.json only).
    Status,
    /// The supervisor exited before it reported anything; the reason goes to the job's note.
    Exited(String),
    /// Neither file appeared within `START_WAIT_MS`.
    TimedOut,
    /// The daemon began shutting down during the wait. The supervisor keeps running and the job stays claimed, so the
    /// next daemon adopts it (O-22).
    Shutdown,
}

/// Waits for the supervisor's report or its exit. The report files are watched by name (supervisor.json for a started
/// command, status.json for one that did not), and the child is awaited, so an exit is seen at once; the re-check
/// interval `POLL` bounds the wait when no event arrives.
async fn wait_for_supervisor(
    sh: &Shared,
    dir: &JobDir,
    child: &mut Child,
) -> Result<Ready, DaemonError> {
    let deadline = sh.clock.now_ms().saturating_add(START_WAIT_MS);
    let info_path = dir.dir.join("supervisor.json");
    let status_path = dir.dir.join("status.json");
    let mut exit = None;
    loop {
        // The exit is read before the files: a supervisor that has exited has written everything it will.
        if exit.is_none() {
            exit = child.try_wait()?;
        }
        if let Some(ready) = reported(dir)? {
            return Ok(ready);
        }
        if let Some(status) = exit {
            return Ok(Ready::Exited(format!(
                "its supervisor exited ({status}) before it started the job"
            )));
        }
        if sh.shutdown.is_cancelled() {
            return Ok(Ready::Shutdown);
        }
        if sh.clock.now_ms() >= deadline {
            return Ok(Ready::TimedOut);
        }
        let waited = tokio::select! {
            status = child.wait() => Some(status),
            () = crate::watch::changed(&info_path, POLL) => None,
            () = crate::watch::changed(&status_path, POLL) => None,
            () = sh.clock.sleep_until(deadline) => None,
            () = sh.shutdown.cancelled() => None,
        };
        if let Some(status) = waited {
            exit = Some(status?);
        }
    }
}

/// The supervisor's report, if it has written one. supervisor.json (it started the command) is read before
/// status.json (it did not).
fn reported(dir: &JobDir) -> Result<Option<Ready>, DaemonError> {
    if crate::watch::file_ready(&dir.dir.join("supervisor.json"))
        && let Some(info) = dir.read_info().map_err(run_error)?
    {
        return Ok(Some(Ready::Info(info)));
    }
    if crate::watch::file_ready(&dir.dir.join("status.json")) {
        return Ok(Some(Ready::Status));
    }
    Ok(None)
}

/// `job.start` (system op): the task starts running with the supervisor's pids. Publishes `WorkChanged`. The op is
/// signed before the writer runs. Also used by adoption (O-22).
pub(super) async fn record_start(
    sh: &Shared,
    task: &str,
    args: JobStartArgs,
) -> Result<TaskRecord, DaemonError> {
    let now = sh.clock.now_ms();
    let arg = serde_json::to_string(&args)
        .map_err(|_| DaemonError::Config("could not encode job.start".into()))?;
    let op = sign(&sh.system, now, "job.start", task, &arg);
    let id = task.to_owned();
    sh.writer
        .exec(move |s| {
            let started = s.job_start(now, &op)?;
            Ok((started, vec![Event::WorkChanged { task: id }]))
        })
        .await
}

/// Ends a supervisor this daemon spawned and still owns, whose job will not be started: the cancel file, then TERM
/// while the child is unreaped (`try_wait` reaps it on exit, so a reaped pid is never signalled), and KILL only when
/// it outlives `TERM_GRACE`. In a tracked task, the child is reaped and the job finalized (R4-3).
fn end_supervisor(sh: &Shared, task: &str, dir: &JobDir, mut child: Child) {
    if let Err(e) = dir.request_cancel() {
        tracing::warn!(task = %task, error = %e, "could not write the cancel file of a job");
    }
    if matches!(child.try_wait(), Ok(None))
        && let Some(pid) = child.id()
        && let Err(e) = signal_pid(pid, Sig::Term)
    {
        tracing::debug!(task = %task, error = %e, "could not signal a job's supervisor");
    }
    let owned = sh.clone();
    let task = task.to_owned();
    sh.jobs.spawn(async move {
        if tokio::time::timeout(TERM_GRACE, child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Err(e) = finalize(&owned, &task).await {
            tracing::error!(task = %task, error = %e, "could not finalize a job");
        }
    });
}

/// Waits for the child in a tracked task, then finalizes the job. Tracked, so shutdown aborts the wait.
fn reap_then_finalize(sh: &Shared, task: String, mut child: Child) {
    let owned = sh.clone();
    sh.jobs.spawn(async move {
        if let Err(e) = child.wait().await {
            tracing::warn!(task = %task, error = %e, "could not wait for a job's supervisor");
        }
        if let Err(e) = finalize(&owned, &task).await {
            tracing::error!(task = %task, error = %e, "could not finalize a job");
        }
    });
}
