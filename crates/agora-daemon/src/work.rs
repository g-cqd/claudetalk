//! Handlers for the work queue: scopes, tasks, claims, runs, cancel, status, plan, settings.
//!
//! Mutations: `op_from_wire` (actor == connection) -> `sh.writer.exec(store fn)` -> events
//! `[Event::WorkChanged{task}]` -> `sh.kick.notify_one()` when the admission set may change (run, cancel,
//! settings, finish). `WorkRun`: the start gate (`sh.jobs.gate()`) is held from `Store::work_request` until
//! `jobs::register_env(task, env)`, so no start reads the job's environment before it is kept (the env is kept
//! in memory until spawn; a daemon restart before spawn loses it, so the job then runs with the minimal env
//! PATH/HOME/LANG and a note). `WorkCancel` of a running task fills `StopTarget.job_dir` and calls
//! `jobs::stop`; a cancel with no stop target drops the kept environment (`jobs::drop_env`). `WorkStatus`: store
//! reads + machine snapshot + `agora_sched::admit` to fill `MachineView.holds/quiet`, live samples from
//! `sample.json` of running jobs, and `swept` = `jobs::sweep`; the samples and the sweep run at most once per
//! `view::SAMPLE_INTERVAL_MS` per daemon home.

mod view;

use crate::dispatch::{daemon_error, op_from_wire};
use crate::{Conn, Shared};
use agora_proto::{ErrorBody, ErrorCode, Event, Reply, Req};
use agora_sched::admit::MachineSnapshot;
use agora_sched::model::{LeaseKind, StatusReport, Swept, TaskRecord};
use agora_sched::settings::{WorkSettings, settings_json, settings_text};
use agora_store::{Pub, Store, StoreError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Routes `WorkScope`, `WorkScopes`, `WorkAdd`, `WorkShow`, `WorkNext`, `WorkClaim`, `WorkAccept`, `WorkRun`,
/// `WorkFinish`, `WorkCancel`, `WorkUpdate`, `WorkReclaim`, `WorkStatus`, `WorkList`, `WorkLog`, `WorkPlan`
/// and `WorkSettings`; anything else is `dispatch::unrouted()`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    match req {
        Req::WorkScope { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let scope = commit(sh, false, move |s| {
                s.work_scope_set(now, &op).map(|scope| {
                    let event = work_changed(scope.id.clone());
                    (scope, vec![event])
                })
            })
            .await?;
            Ok(Reply::Scope { scope })
        }
        Req::WorkScopes { root } => {
            let scopes = read(sh, conn.pubkey, move |s| s.work_scopes(root.as_deref())).await?;
            Ok(Reply::Scopes { scopes })
        }
        Req::WorkAdd { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let task = commit(sh, false, move |s| s.work_add(now, &op).map(task_written)).await?;
            Ok(Reply::Task { task })
        }
        Req::WorkShow { task } => {
            let task = read(sh, conn.pubkey, move |s| s.work_task(&task)).await?;
            Ok(Reply::Task { task })
        }
        Req::WorkNext { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let outcome = commit(sh, false, move |s| {
                s.work_next(now, &op).map(|outcome| {
                    let events = outcome
                        .claimed
                        .iter()
                        .map(|task| work_changed(task.id.clone()))
                        .collect();
                    (outcome, events)
                })
            })
            .await?;
            Ok(Reply::Next { outcome })
        }
        Req::WorkClaim { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let task = commit(sh, false, move |s| s.work_claim(now, &op).map(task_written)).await?;
            Ok(Reply::Task { task })
        }
        Req::WorkAccept { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let (task, first) = commit(sh, false, move |s| {
                s.work_accept(now, &op).map(|(task, first)| {
                    let event = work_changed(task.id.clone());
                    ((task, first), vec![event])
                })
            })
            .await?;
            Ok(Reply::Accepted { task, first })
        }
        Req::WorkRun { op, env } => {
            let op = op_from_wire(conn, &op)?;
            view::check_env_size(&env).map_err(|m| ErrorBody::new(ErrorCode::Invalid, m))?;
            let now = sh.clock.now_ms();
            let cpus = machine_snapshot(sh).await?.cpus;
            let logs = log_dir(&agora_run::jobdir::work_root(&sh.config.home))?;
            // The gate is taken before the commit and released once the environment is registered. The commit
            // publishes WorkChanged, and a start takes the same gate before it reads a job's environment, so no
            // start can run the job before its environment is kept (O-18).
            let gate = sh.jobs.gate().lock().await;
            let task = commit(sh, false, move |s| {
                s.work_request(now, &op, cpus, &logs).map(task_written)
            })
            .await?;
            crate::jobs::register_env(sh, &task.id, agora_sched::envscrub::scrub(env));
            drop(gate);
            sh.kick.notify_one();
            Ok(Reply::Task { task })
        }
        Req::WorkFinish { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let task = commit(sh, true, move |s| s.work_finish(now, &op).map(task_written)).await?;
            Ok(Reply::Task { task })
        }
        Req::WorkCancel { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let outcome = commit(sh, false, move |s| {
                s.work_cancel(now, &op).map(|outcome| {
                    let event = work_changed(outcome.task.id.clone());
                    (outcome, vec![event])
                })
            })
            .await?;
            let task = outcome.task;
            match outcome.stop {
                Some(mut target) => {
                    target.job_dir = job_dir(&sh.config.home, &task.id)?;
                    crate::jobs::stop(sh, target, &task.id).await;
                }
                // Nothing runs for the task, so the environment kept for its start will never be used. The start gate
                // is held while it is dropped, so a start that is taking the environment cannot interleave (C1-m8).
                None => {
                    let _gate = sh.jobs.gate().lock().await;
                    crate::jobs::drop_env(sh, &task.id);
                }
            }
            sh.kick.notify_one();
            Ok(Reply::Task { task })
        }
        Req::WorkUpdate { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let task = commit(sh, true, move |s| s.work_update(now, &op).map(task_written)).await?;
            Ok(Reply::Task { task })
        }
        Req::WorkReclaim { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let task_id = op.subject.clone();
            commit(sh, true, move |s| {
                s.work_reclaim(now, &op)
                    .map(|()| ((), vec![work_changed(task_id)]))
            })
            .await?;
            Ok(Reply::Unit)
        }
        Req::WorkStatus { scope } => {
            let pk = conn.pubkey;
            // An identity that may not read the queue gets no samples taken and no sweep run for it.
            read(sh, pk, |_| Ok(())).await?;
            let now = sh.clock.now_ms();
            // At most one sampling pass per `SAMPLE_INTERVAL_MS` per home: a poll loop must not probe every process
            // on each call. A throttled call reports no swept jobs.
            let swept = if claim_sampling(&sh.config.home, now) {
                crate::jobs::refresh_samples(sh).await;
                crate::jobs::sweep(sh).await
            } else {
                Vec::new()
            };
            let snapshot = machine_snapshot(sh).await?;
            let report = read(sh, pk, move |s| {
                status_report(s, scope.as_deref(), &snapshot, now, swept)
            })
            .await?;
            Ok(Reply::WorkStatus { report })
        }
        Req::WorkList { filter } => {
            let tasks = read(sh, conn.pubkey, move |s| s.work_tasks(&filter)).await?;
            Ok(Reply::Tasks { tasks })
        }
        Req::WorkLog {
            task,
            actor,
            scope,
            limit,
        } => {
            let limit = view::log_limit(limit);
            let events = read(sh, conn.pubkey, move |s| {
                s.work_log(task.as_deref(), actor.as_deref(), scope.as_deref(), limit)
            })
            .await?;
            Ok(Reply::WorkLog { events })
        }
        Req::WorkPlan {
            scope,
            policy,
            agents,
            assign,
            conflict_cost,
        } => {
            let mode = view::plan_mode(agents, assign, conflict_cost)
                .map_err(|m| ErrorBody::new(ErrorCode::Invalid, m))?;
            // A queue over the cap is refused before the plan is built from it (S-6).
            let counted = scope.clone();
            let queued = read(sh, conn.pubkey, move |s| {
                s.work_counts(counted.as_deref())
                    .map(|counts| counts.queued)
            })
            .await?;
            view::check_plan_size(queued).map_err(|m| ErrorBody::new(ErrorCode::Invalid, m))?;
            let (json, text) = read(sh, conn.pubkey, move |s| {
                s.work_plan(scope.as_deref(), policy, &mode)
            })
            .await?;
            Ok(Reply::WorkPlan { json, text })
        }
        Req::WorkSettings { op: None } => {
            let settings = read(sh, conn.pubkey, |s| s.work_settings()).await?;
            settings_reply(sh, settings).await
        }
        Req::WorkSettings { op: Some(op) } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            let settings = commit(sh, true, move |s| {
                s.work_settings_set(now, &op)
                    .map(|settings| (settings, vec![Event::WorkSettingsChanged]))
            })
            .await?;
            settings_reply(sh, settings).await
        }
        _ => Err(crate::dispatch::unrouted()),
    }
}

/// Runs one write on the writer thread and, on success, kicks admission when `kick` is set.
async fn commit<T, F>(sh: &Shared, kick: bool, write: F) -> Result<T, ErrorBody>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<(T, Vec<Event>), StoreError> + Send + 'static,
{
    let value = sh.writer.exec(write).await.map_err(daemon_error)?;
    if kick {
        sh.kick.notify_one();
    }
    Ok(value)
}

/// Runs one read on the reader thread after checking that `pk` is an active identity.
async fn read<T, F>(sh: &Shared, pk: Pub, f: F) -> Result<T, ErrorBody>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
{
    sh.reader
        .exec(move |s| {
            s.role_of(&pk)?;
            f(s)
        })
        .await
        .map_err(daemon_error)
}

/// The event for a task write, with the task it describes.
fn task_written(task: TaskRecord) -> (TaskRecord, Vec<Event>) {
    let event = work_changed(task.id.clone());
    (task, vec![event])
}

fn work_changed(task: String) -> Event {
    Event::WorkChanged { task }
}

/// Probes the machine on a blocking thread: the sysctl child behind the memory probe may take up to 2 s.
async fn machine_snapshot(sh: &Shared) -> Result<MachineSnapshot, ErrorBody> {
    let machine = sh.machine.clone();
    tokio::task::spawn_blocking(move || machine.snapshot())
        .await
        .map_err(internal)
}

/// The settings reply: the settings, their JSON view and their text, both against a fresh machine snapshot.
async fn settings_reply(sh: &Shared, settings: WorkSettings) -> Result<Reply, ErrorBody> {
    let snapshot = machine_snapshot(sh).await?;
    let view = settings_json(&settings, &snapshot);
    let text = settings_text(&settings, &snapshot, cfg!(target_os = "macos"));
    Ok(Reply::WorkSettings {
        settings,
        view,
        text,
    })
}

/// The status of the queue: scopes, tasks and counts in `scope`, and the machine as a whole (its usage counts
/// every running task, whatever the scope).
fn status_report(
    s: &Store,
    scope: Option<&str>,
    snapshot: &MachineSnapshot,
    now: i64,
    swept: Vec<Swept>,
) -> Result<StatusReport, StoreError> {
    let settings = s.work_settings()?;
    let all_running = s.work_running()?;
    let manual = s.leases(now, Some(LeaseKind::Quiet), None, true)?;
    let paused = s.admission_paused()?;
    let machine = view::machine_view(&settings, snapshot, &all_running, &manual, paused, now);
    let running = all_running
        .into_iter()
        .filter(|task| view::in_scope(task, scope))
        .collect();
    let waiting = s
        .work_waiting()?
        .into_iter()
        .filter(|task| view::in_scope(task, scope))
        .collect();
    Ok(StatusReport {
        time_ms: now,
        scope: scope.map(str::to_string),
        machine,
        scopes: s.work_scopes(scope)?,
        running,
        waiting,
        counts: s.work_counts(scope)?,
        swept,
    })
}

/// When each daemon home last ran a sampling pass, keyed by home so that two daemons in one process do not throttle
/// each other. One entry per home; the map does not grow with the number of jobs.
static LAST_SAMPLING: OnceLock<Mutex<HashMap<PathBuf, i64>>> = OnceLock::new();

/// Takes the sampling slot of `home` at `now_ms`: true when no pass ran within `view::SAMPLE_INTERVAL_MS` before
/// it, and then records this one. The check and the record happen under one lock, so concurrent status calls run
/// one pass between them.
fn claim_sampling(home: &Path, now_ms: i64) -> bool {
    let map = LAST_SAMPLING.get_or_init(|| Mutex::new(HashMap::new()));
    let mut last = map.lock().unwrap_or_else(|p| p.into_inner());
    if !view::sample_due(last.get(home).copied(), now_ms) {
        return false;
    }
    last.insert(home.to_path_buf(), now_ms);
    true
}

/// The job log directory the store records for each run: `<work_root>/logs`.
fn log_dir(work_root: &Path) -> Result<String, ErrorBody> {
    path_text(&work_root.join("logs"))
}

/// The job directory of `task` under the work root of `home`, as `StopTarget.job_dir` expects it.
fn job_dir(home: &Path, task: &str) -> Result<String, ErrorBody> {
    let dir = agora_run::jobdir::JobDir::new(&agora_run::jobdir::work_root(home), task)
        .map_err(internal)?;
    path_text(&dir.dir)
}

fn path_text(path: &Path) -> Result<String, ErrorBody> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| internal("a work path is not UTF-8"))
}

/// Logs the fault with its detail; the client gets only the fixed message.
fn internal(e: impl std::fmt::Display) -> ErrorBody {
    tracing::error!(error = %e, "work handler failed");
    ErrorBody::new(ErrorCode::Internal, "internal error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_logs_under_the_work_root() {
        let root = agora_run::jobdir::work_root(Path::new("/home/agora"));
        assert_eq!(
            log_dir(&root).ok().as_deref(),
            Some("/home/agora/work/logs")
        );
    }

    #[test]
    fn job_dir_is_the_task_directory_under_the_work_root() {
        assert_eq!(
            job_dir(Path::new("/home/agora"), "task-1").ok().as_deref(),
            Some("/home/agora/work/jobs/task-1")
        );
    }

    #[test]
    fn job_dir_refuses_a_task_id_that_is_not_a_name() {
        assert!(job_dir(Path::new("/home/agora"), "../escape").is_err());
    }

    #[test]
    fn sampling_slot_is_throttled_per_home() {
        // Homes that no other test uses, since the slots are process-wide.
        let a = Path::new("/nonexistent/agora-work-claim-a");
        let b = Path::new("/nonexistent/agora-work-claim-b");
        assert!(claim_sampling(a, 100_000));
        assert!(!claim_sampling(a, 101_000));
        assert!(claim_sampling(b, 101_000));
        assert!(claim_sampling(a, 102_000));
    }

    #[test]
    fn internal_errors_keep_their_detail_from_the_client() {
        let body = internal("disk on fire");
        assert_eq!(body.code, ErrorCode::Internal);
        assert_eq!(body.message, "internal error");
    }
}
