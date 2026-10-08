//! Finishing a job: finalize it from its status.json, or sweep it when its supervisor is gone without one (D8); stop a
//! running job (D9); copy the live samples the supervisors write (D8).

use super::start::record_start;
use super::{
    Finish, TERM_GRACE, command_of, ended_without_status_note, finish_running, gone_note, is_open,
    job_dir, read_task, run_error, set_note, start_args, supervisor_of, verified_alive,
};
use crate::{DaemonError, Shared};
use agora_run::jobdir::{JobDir, Sample, log_path, work_root};
use agora_run::proc::{Sig, alive, signal_group, signal_pid};
use agora_sched::model::{JobStartArgs, StopTarget, Swept, TaskRecord, TaskState};
use std::time::Duration;
use tokio::time::Instant;

/// Finishes `task` once its supervisor is done: from status.json when the supervisor wrote one, else by the
/// dead-supervisor rule (D8) when the task is still running. A task that is not open is left alone.
pub async fn finalize(sh: &Shared, task: &str) -> Result<(), DaemonError> {
    let Some(record) = read_task(sh, task).await? else {
        return Ok(());
    };
    if !is_open(record.state, record.requested_ms) {
        sh.jobs.forget(task);
        return Ok(());
    }
    let dir = job_dir(sh, task)?;
    match dir.read_status().map_err(run_error)? {
        Some(status) => {
            // The supervisor started the command, but the daemon stopped before job.start was recorded (C1-M1). The
            // job cannot finish from Claimed, so the start is recorded first.
            if record.state == TaskState::Claimed && status.started {
                record_unseen_start(sh, &dir, &record).await?;
            }
            // finish_running forgets the job's table entries once it has ended.
            finish_running(sh, task, Finish::from_status(&status)).await?;
        }
        None if record.state == TaskState::Running => {
            sweep_one(sh, &record).await?;
        }
        None => {}
    }
    Ok(())
}

/// C1-M1: records `job.start` for a claimed job whose supervisor reported a started command (status.json says so).
/// The arguments come from supervisor.json when it names the command's pid. Otherwise they are minimal: pids 0, since
/// the command's pid was never reported, and the start time is the one the store stamps. A start that the store
/// refuses only fails the finalize when the task is still claimed; when another path already recorded the start or
/// the end, the finish below sees that state instead.
async fn record_unseen_start(
    sh: &Shared,
    dir: &JobDir,
    record: &TaskRecord,
) -> Result<(), DaemonError> {
    let info = dir.read_info().map_err(run_error)?;
    let spec = dir.read_spec().ok();
    let exclusive = spec.as_ref().is_some_and(|spec| spec.exclusive);
    let log = match spec {
        Some(spec) => spec.log,
        None => log_path(&work_root(&sh.config.home), &record.id)
            .to_str()
            .unwrap_or_default()
            .to_owned(),
    };
    let args = info
        .as_ref()
        .and_then(|info| start_args(info, exclusive, None, &log))
        .unwrap_or_else(|| JobStartArgs {
            pid: 0,
            supervisor_pid: 0,
            supervisor_start_s: None,
            qos: String::new(),
            exclusive,
            quiet_until_ms: None,
            log: log.clone(),
        });
    match record_start(sh, &record.id, args).await {
        Ok(_) => Ok(()),
        Err(e) => match read_task(sh, &record.id).await? {
            Some(current) if current.state == TaskState::Claimed => Err(e),
            _ => {
                tracing::debug!(task = %record.id, "the job's start was recorded by another path");
                Ok(())
            }
        },
    }
}

/// The dead-supervisor rule (D8) for one running task that has no status.json. While its supervisor lives, nothing.
/// When its command outlives the supervisor, the job stays running with a note saying so. Otherwise the job ends
/// failed without a status, and the store cancels it instead if a cancel was requested.
async fn sweep_one(sh: &Shared, record: &TaskRecord) -> Result<Option<Swept>, DaemonError> {
    let dir = job_dir(sh, &record.id)?;
    let info = dir.read_info().map_err(run_error)?;
    let supervisor = supervisor_of(info.as_ref(), record.supervisor_pid);
    if supervisor.is_some_and(|(pid, start)| alive(pid, start)) {
        return Ok(None);
    }
    // An unknown supervisor pid is written as 0; a job with no supervisor record never had a live one to report.
    let supervisor_pid = supervisor.map_or(0, |(pid, _)| pid);
    let command = command_of(info.as_ref(), record.pid);
    if command.is_some_and(|(pid, start)| alive(pid, start)) {
        let note = gone_note(supervisor_pid, command.map_or(0, |(pid, _)| pid));
        if record.note != note {
            set_note(sh, &record.id, note.clone()).await?;
        }
        return Ok(Some(Swept {
            id: record.id.clone(),
            state: record.state.as_str().to_owned(),
            note,
        }));
    }
    let fin = Finish {
        status: None,
        note: ended_without_status_note(supervisor_pid),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: sh.clock.now_ms(),
        before_start: false,
    };
    let finished = finish_running(sh, &record.id, fin).await?;
    Ok(finished.map(|task| Swept {
        id: task.id,
        state: task.state.as_str().to_owned(),
        note: task.note,
    }))
}

/// Runs the dead-supervisor rule over every running task: at startup, for WorkStatus, and on the reconciler tick. A
/// task whose status.json is already there is finalized instead. Returns the rows it changed or annotated.
pub async fn sweep(sh: &Shared) -> Vec<Swept> {
    let running = match sh.reader.exec(|s| s.work_running()).await {
        Ok(running) => running,
        Err(e) => {
            tracing::error!(error = %e, "could not list the running jobs");
            return Vec::new();
        }
    };
    let mut swept = Vec::new();
    for record in &running {
        if let Some(row) = sweep_record(sh, record).await {
            swept.push(row);
        }
    }
    swept
}

/// One running task of [`sweep`]. Failures are logged and skipped, so one bad job does not stop the others.
async fn sweep_record(sh: &Shared, record: &TaskRecord) -> Option<Swept> {
    let dir = match job_dir(sh, &record.id) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(task = %record.id, error = %e, "a running job has no usable directory");
            return None;
        }
    };
    match dir.read_status() {
        Ok(Some(_)) => {
            if let Err(e) = finalize(sh, &record.id).await {
                tracing::warn!(task = %record.id, error = %e, "could not finalize a job");
            }
            None
        }
        Ok(None) => match sweep_one(sh, record).await {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(task = %record.id, error = %e, "could not sweep a job");
                None
            }
        },
        Err(e) => {
            tracing::warn!(task = %record.id, error = %e, "could not read a job's status");
            None
        }
    }
}

/// D9: asks a running job to stop. The cancel file is written first, so the job ends cancelled whichever way it ends.
/// A live supervisor, verified by its start time, gets TERM and ends the job with its recorded status. When the
/// supervisor is gone but its command lives on (verified), the command is ended (O-20). Otherwise the job is finalized
/// now. Nothing is signalled on a pid without a verified start time.
pub async fn stop(sh: &Shared, target: StopTarget, task: &str) {
    let dir = match job_dir(sh, task) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(task = %task, error = %e, "cannot stop a job without a directory");
            return;
        }
    };
    if let Err(e) = dir.request_cancel() {
        tracing::warn!(task = %task, error = %e, "could not write the cancel file of a job");
    }
    let supervisor = u32::try_from(target.supervisor_pid).ok();
    if let Some(pid) = supervisor.filter(|pid| verified_alive(*pid, target.supervisor_start_s)) {
        if let Err(e) = signal_pid(pid, Sig::Term) {
            // The supervisor may have exited since the probe; the cancel file stands, and the finalize below or the
            // sweep settles the job.
            tracing::debug!(task = %task, error = %e, "could not signal a job's supervisor");
        }
        return;
    }
    // A supervisor that is not verifiably gone may still be running: its own cancel handling ends the command.
    let supervisor_gone = supervisor.is_none_or(|pid| !alive(pid, target.supervisor_start_s));
    if supervisor_gone
        && let Some((pid, start)) = orphan_command(sh, &dir, task).await
        && verified_alive(pid, start)
    {
        end_orphan_command(sh, task, pid, start);
        return;
    }
    if let Err(e) = finalize(sh, task).await {
        tracing::warn!(task = %task, error = %e, "could not finalize a cancelled job");
    }
}

/// The (pid, start time) of a job's command, from its supervisor.json or the task's recorded pid.
async fn orphan_command(sh: &Shared, dir: &JobDir, task: &str) -> Option<(u32, Option<u64>)> {
    let record = read_task(sh, task).await.ok()??;
    let info = dir.read_info().ok().flatten();
    command_of(info.as_ref(), record.pid)
}

/// O-20: the supervisor of a job is gone while its command, verified by its start time, lives on. The command's group
/// gets TERM, and KILL after `TERM_GRACE` if it is still there. Then the job is finalized in a tracked task: it ends
/// cancelled, because the cancel file and the store's cancel request both stand.
fn end_orphan_command(sh: &Shared, task: &str, pid: u32, start_s: Option<u64>) {
    if let Err(e) = signal_group(pid, Sig::Term) {
        tracing::debug!(task = %task, error = %e, "could not signal the command of a cancelled job");
    }
    let owned = sh.clone();
    let task = task.to_owned();
    sh.jobs.spawn(async move {
        if !wait_gone(pid, start_s, TERM_GRACE).await
            && let Err(e) = signal_group(pid, Sig::Kill)
        {
            tracing::warn!(task = %task, error = %e, "could not kill the command of a cancelled job");
        }
        if let Err(e) = finalize(&owned, &task).await {
            tracing::warn!(task = %task, error = %e, "could not finalize a cancelled job");
        }
    });
}

/// Polls until the verified process `pid` is gone, for at most `grace`. True when it is gone.
async fn wait_gone(pid: u32, start_s: Option<u64>, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while alive(pid, start_s) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

/// Copies the sample of each running job into the store (`job_sample`, not an audited operation). A job with no
/// readable sample keeps its row as it is.
pub async fn refresh_samples(sh: &Shared) {
    let running = match sh.reader.exec(|s| s.work_running()).await {
        Ok(running) => running,
        Err(e) => {
            tracing::warn!(error = %e, "could not list the running jobs for their samples");
            return;
        }
    };
    let samples: Vec<(String, Sample)> = running
        .into_iter()
        .filter_map(|record| {
            let dir = job_dir(sh, &record.id).ok()?;
            let sample = dir.read_sample().ok().flatten()?;
            Some((record.id, sample))
        })
        .collect();
    if samples.is_empty() {
        return;
    }
    let stored = sh
        .writer
        .exec(move |s| {
            for (id, sample) in &samples {
                if let Err(e) = s.job_sample(
                    id,
                    sample.cpu_percent,
                    sample.rss_kib,
                    sample.rss_peak_kib,
                    sample.sampled_ms,
                ) {
                    tracing::warn!(task = %id, error = %e, "could not store a job sample");
                }
            }
            Ok(((), Vec::new()))
        })
        .await;
    if let Err(e) = stored {
        tracing::warn!(error = %e, "could not store the job samples");
    }
}
