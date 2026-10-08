//! Startup adoption (D8): the supervisors that outlived the previous daemon are watched until their jobs end.

use super::finish::finalize;
use super::start::record_start;
use super::{job_dir, read_task, run_error, start_args, supervisor_of, sweep};
use crate::Shared;
use agora_run::jobdir::JobDir;
use agora_run::proc::alive;
use agora_sched::model::{TaskRecord, TaskState};
use std::path::PathBuf;
use std::time::Duration;

/// How often an adopted supervisor's liveness is checked when no file event arrives first (the 5 s check of D8).
const LIVENESS: Duration = Duration::from_secs(5);

/// Startup: running jobs whose supervisors are alive and have not written status.json are adopted, and a tracked
/// watcher finalizes each one when its status appears or its supervisor dies. A task that was waiting for its start
/// when the daemon stopped is settled first (O-22). Then the sweep settles every other running job.
pub async fn adopt_all(sh: &Shared) {
    let running = match sh.reader.exec(|s| s.work_running()).await {
        Ok(running) => running,
        Err(e) => {
            tracing::error!(error = %e, "could not list the running jobs for adoption");
            return;
        }
    };
    let waiting = match sh.reader.exec(|s| s.work_waiting()).await {
        Ok(waiting) => waiting,
        Err(e) => {
            tracing::error!(error = %e, "could not list the waiting jobs for adoption");
            Vec::new()
        }
    };
    let mut candidates = running;
    for record in waiting {
        if let Some(started) = settle_waiting(sh, record).await {
            candidates.push(started);
        }
    }
    for record in candidates {
        let dir = match job_dir(sh, &record.id) {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(task = %record.id, error = %e, "a running job has no usable directory");
                continue;
            }
        };
        // Finished jobs are the sweep's; an unreadable status is left to it as well.
        if !matches!(dir.read_status(), Ok(None)) {
            continue;
        }
        let info = dir.read_info().ok().flatten();
        let Some((pid, start)) = supervisor_of(info.as_ref(), record.supervisor_pid) else {
            continue;
        };
        if !alive(pid, start) {
            continue;
        }
        let owned = sh.clone();
        sh.jobs
            .spawn(watch_adopted(owned, record.id, dir.dir, pid, start));
    }
    sweep(sh).await;
}

/// O-22: a task that waits for its start (claimed and requested) whose supervisor reported before the daemon stopped.
/// A status.json finalizes it. A supervisor.json records its start, and the running task is returned for adoption.
/// Anything else stays as it is for the admission loop.
async fn settle_waiting(sh: &Shared, record: TaskRecord) -> Option<TaskRecord> {
    let dir = match job_dir(sh, &record.id) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(task = %record.id, error = %e, "a waiting job has no usable directory");
            return None;
        }
    };
    match dir.read_status() {
        Ok(Some(_)) => {
            if let Err(e) = finalize(sh, &record.id).await {
                tracing::warn!(task = %record.id, error = %e, "could not finalize a waiting job");
            }
            return None;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(task = %record.id, error = %e, "could not read a waiting job's status");
            return None;
        }
    }
    let info = match dir.read_info() {
        Ok(Some(info)) => info,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(task = %record.id, error = %run_error(e), "could not read a waiting job's supervisor report");
            return None;
        }
    };
    let spec = match dir.read_spec() {
        Ok(spec) => spec,
        Err(e) => {
            tracing::warn!(task = %record.id, error = %run_error(e), "could not read a waiting job's spec");
            return None;
        }
    };
    // The quiet window of the job's admission is not kept on disk, so an adopted start records none.
    let args = start_args(&info, spec.exclusive, None, &spec.log)?;
    match record_start(sh, &record.id, args).await {
        Ok(started) if started.state == TaskState::Running => Some(started),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(task = %record.id, error = %e, "could not record the start of an adopted job");
            None
        }
    }
}

/// Watches one adopted job until it ends. It finalizes when status.json appears (watched by name) or the supervisor is
/// gone (the finalize then sweeps a job with neither), and stops when the task is no longer running or the daemon
/// shuts down.
async fn watch_adopted(sh: Shared, task: String, dir: PathBuf, pid: u32, start: Option<u64>) {
    let job = JobDir { dir: dir.clone() };
    let status_path = dir.join("status.json");
    loop {
        match read_task(&sh, &task).await {
            Ok(Some(record)) if record.state == TaskState::Running => {}
            Ok(_) => break,
            Err(e) => {
                tracing::warn!(task = %task, error = %e, "could not read an adopted job");
                break;
            }
        }
        if !alive(pid, start) || !matches!(job.read_status(), Ok(None)) {
            if let Err(e) = finalize(&sh, &task).await {
                tracing::error!(task = %task, error = %e, "could not finalize an adopted job");
            }
            break;
        }
        tokio::select! {
            () = crate::watch::changed(&status_path, LIVENESS) => {}
            () = sh.shutdown.cancelled() => break,
        }
    }
    sh.jobs.forget(&task);
}
