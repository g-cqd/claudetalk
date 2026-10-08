//! `Req::WorkWait`: one bounded wait for a condition (tasks, cards, messages, notices, pid, file, gone, log).
//!
//! Task, card, message and notice waits are bus-driven, as in `wait.rs`: subscribe first, check, then wait for a
//! relevant event, a lagged receiver, or the deadline; each wake-up re-checks. File, gone and log waits wake on a
//! `watch::Watcher` created before their first check and kept for the whole call; pid waits probe every 250 ms
//! (documented: no portable process-exit event without unsafe). Every deadline is read through `sh.clock`, so tests
//! drive time with a `FakeClock`. The cursor, the status rules and the shared wait loop live in the submodules
//! `cursor`, `rules` and `turn`; what a wait reports beyond its status lives in `report`.

mod cursor;
mod log;
mod report;
mod rules;
#[cfg(test)]
mod tests;
mod turn;

use self::cursor::{Cursor, decode_cursor};
use self::log::log_wait;
use self::report::{latest_notice_seq, task_tail};
use self::rules::{
    Caller, card_not_found, card_states, file_status, known_floor, need_worker, non_empty,
    non_final, refuse_job_paths, sole_path, task_done, task_status, within_targets,
};
use self::turn::{Ctx, Found, Wake};
use crate::{Conn, DaemonError, Shared, watch};
use agora_core::{CardState, Role};
use agora_proto::{Event, MAX_WAIT_MS};
use agora_run::jobdir;
use agora_sched::model::{TaskFilter, TaskState, WaitResult, WaitSpec};
use agora_store::{Pub, hex};
use std::path::{Path, PathBuf};

/// Pid liveness probe period.
const PID_PROBE_MS: i64 = 250;

/// Returns before `min(timeout_ms, MAX_WAIT_MS)` with a [`WaitResult`]; `cursor` (opaque base64 JSON, see `cursor`)
/// resumes the log offset, the accumulated seconds and the notice floor. Task/card/message/notice conditions are
/// bus-driven (the `wait_for` pattern: subscribe first, then check); file/gone/log use a `watch::Watcher`; pid uses a
/// 250 ms liveness probe. The caller must hold the Worker role for the pid, file, gone and log conditions, and a
/// file, gone or log path under the work root needs Supervisor. A task or All wait returns a log tail only to the
/// selected task's author or claimant, or to a Supervisor or more.
pub async fn wait(
    sh: &Shared,
    conn: &Conn,
    spec: WaitSpec,
    timeout_ms: u64,
    cursor: Option<String>,
) -> Result<WaitResult, DaemonError> {
    let me = conn.pubkey;
    let role = sh.reader.exec(move |s| s.role_of(&me)).await?;
    let caller = Caller {
        key: hex(&me),
        role,
    };
    if matches!(
        spec,
        WaitSpec::Pid { .. } | WaitSpec::File { .. } | WaitSpec::Gone { .. } | WaitSpec::Log { .. }
    ) {
        need_worker(role)?;
    }
    job_path_gate(sh, &spec, role)?;
    let resumed = decode_cursor(cursor.as_deref())?;
    let start_ms = sh.clock.now_ms();
    let timeout = i64::try_from(timeout_ms.min(MAX_WAIT_MS)).unwrap_or(i64::MAX);
    let ctx = Ctx {
        sh,
        condition: condition_name(&spec),
        start_ms,
        deadline_ms: start_ms.saturating_add(timeout),
        elapsed_before_ms: resumed.as_ref().map_or(0, |c| c.elapsed_ms.max(0)),
    };
    match spec {
        WaitSpec::Tasks { ids, any } => {
            non_empty(&ids, "name at least one task")?;
            within_targets(ids.len())?;
            task_wait(&ctx, ids, any, false, &caller).await
        }
        WaitSpec::All { scope } => all_wait(&ctx, scope, resumed, &caller).await,
        WaitSpec::Cards { ids, states } => {
            non_empty(&ids, "name at least one card")?;
            within_targets(ids.len())?;
            within_targets(states.len())?;
            cards_wait(&ctx, ids, card_states(states)?).await
        }
        WaitSpec::Messages { channel, after } => messages_wait(&ctx, me, channel, after).await,
        WaitSpec::Notices { after } => notices_wait(&ctx, me, after, resumed).await,
        WaitSpec::Pid { pids } => pid_wait(&ctx, pids).await,
        WaitSpec::File { paths } => file_wait(&ctx, paths).await,
        WaitSpec::Gone { paths } => gone_wait(&ctx, paths).await,
        WaitSpec::Log { path, pattern } => log_wait(&ctx, path, pattern, resumed).await,
    }
}

/// The work-root gate of the file, gone and log conditions (see `rules::refuse_job_paths`).
fn job_path_gate(sh: &Shared, spec: &WaitSpec, role: Role) -> Result<(), DaemonError> {
    let work = jobdir::work_root(&sh.config.home);
    match spec {
        WaitSpec::File { paths } | WaitSpec::Gone { paths } => refuse_job_paths(paths, role, &work),
        WaitSpec::Log { path, .. } => refuse_job_paths(std::slice::from_ref(path), role, &work),
        _ => Ok(()),
    }
}

/// The condition name the result reports.
fn condition_name(spec: &WaitSpec) -> &'static str {
    match spec {
        WaitSpec::Tasks { .. } => "tasks",
        WaitSpec::All { .. } => "all",
        WaitSpec::Cards { .. } => "cards",
        WaitSpec::Messages { .. } => "messages",
        WaitSpec::Notices { .. } => "notices",
        WaitSpec::Pid { .. } => "pid",
        WaitSpec::File { .. } => "file",
        WaitSpec::Gone { .. } => "gone",
        WaitSpec::Log { .. } => "log",
    }
}

/// Tasks (given ids) and All (the scope snapshot): done when every task is final, or when one is for `any`.
/// `snapshot` keeps the ids in the cursor so a resumed All call waits on the same set.
async fn task_wait(
    ctx: &Ctx<'_>,
    ids: Vec<String>,
    any: bool,
    snapshot: bool,
    caller: &Caller,
) -> Result<WaitResult, DaemonError> {
    let mut wake = Wake::Bus {
        rx: ctx.sh.bus.subscribe(),
        relevant: Box::new(|e: &Event| matches!(e, Event::WorkChanged { .. })),
    };
    loop {
        let read = ids.clone();
        let tasks = ctx
            .sh
            .reader
            .exec(move |s| {
                read.iter()
                    .map(|id| s.work_task(id))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await?;
        if task_done(&tasks, any) {
            let status = task_status(&tasks, any);
            let tail = task_tail(ctx.sh, tasks.clone(), false, caller.clone()).await?;
            return Ok(ctx.end(
                false,
                Found {
                    status,
                    targets: ids,
                    tasks,
                    tail,
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            let cursor = Cursor {
                ids: if snapshot { ids.clone() } else { Vec::new() },
                ..Cursor::default()
            };
            let pending = non_final(&tasks);
            let tail = task_tail(ctx.sh, tasks.clone(), true, caller.clone()).await?;
            return Ok(ctx.end(
                true,
                Found {
                    targets: ids,
                    pending,
                    tasks,
                    tail,
                    cursor,
                    ..Found::default()
                },
            ));
        }
    }
}

/// All: the snapshot of running and waiting tasks in scope, taken on the first call; a resumed call keeps the
/// ids in its cursor. An empty snapshot is done at once with status 0.
async fn all_wait(
    ctx: &Ctx<'_>,
    scope: Option<String>,
    resumed: Option<Cursor>,
    caller: &Caller,
) -> Result<WaitResult, DaemonError> {
    let ids = match resumed {
        Some(cursor) => cursor.ids,
        None => running_ids(ctx.sh, scope).await?,
    };
    within_targets(ids.len())?;
    if ids.is_empty() {
        return Ok(ctx.end(false, Found::default()));
    }
    task_wait(ctx, ids, false, true, caller).await
}

/// Running tasks, and claimed tasks that asked to run (waiting for capacity), in scope, in arrival order.
async fn running_ids(sh: &Shared, scope: Option<String>) -> Result<Vec<String>, DaemonError> {
    let filter = TaskFilter {
        scope,
        states: vec![TaskState::Running, TaskState::Claimed],
        ids: Vec::new(),
        limit: 0,
    };
    let tasks = sh.reader.exec(move |s| s.work_tasks(&filter)).await?;
    Ok(tasks
        .into_iter()
        .filter(|t| {
            t.state == TaskState::Running
                || (t.state == TaskState::Claimed && t.requested_ms.is_some())
        })
        .map(|t| t.id)
        .collect())
}

/// Cards: done when every card's state is one of `wanted`.
async fn cards_wait(
    ctx: &Ctx<'_>,
    ids: Vec<String>,
    wanted: Vec<String>,
) -> Result<WaitResult, DaemonError> {
    let mut wake = Wake::Bus {
        rx: ctx.sh.bus.subscribe(),
        relevant: Box::new(|e: &Event| matches!(e, Event::CardChanged { .. })),
    };
    loop {
        let read = ids.clone();
        let states = ctx
            .sh
            .reader
            .exec(move |s| {
                read.iter()
                    .map(|id| s.card_state(id).map_err(|e| card_not_found(e, id)))
                    .collect::<Result<Vec<CardState>, _>>()
            })
            .await?;
        let pending: Vec<String> = ids
            .iter()
            .zip(&states)
            .filter(|(_, st)| !wanted.iter().any(|w| w == st.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        if pending.is_empty() {
            return Ok(ctx.end(
                false,
                Found {
                    targets: ids,
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            return Ok(ctx.end(
                true,
                Found {
                    targets: ids,
                    pending,
                    ..Found::default()
                },
            ));
        }
    }
}

/// Messages: done when `channel` has a message with seq > `after` that `me` can read. A channel `me` is not a
/// member of is refused (`NotMember`), as the read API refuses it.
async fn messages_wait(
    ctx: &Ctx<'_>,
    me: Pub,
    channel: String,
    after: i64,
) -> Result<WaitResult, DaemonError> {
    let watched = channel.clone();
    let mut wake = Wake::Bus {
        rx: ctx.sh.bus.subscribe(),
        relevant: Box::new(move |e: &Event| match e {
            Event::MessageAppended { channel, .. } | Event::ChannelChanged { channel } => {
                *channel == watched
            }
            _ => false,
        }),
    };
    loop {
        let read = channel.clone();
        let rows = ctx
            .sh
            .reader
            .exec(move |s| s.read_channel(&me, &read, after, 1))
            .await?;
        if !rows.is_empty() {
            return Ok(ctx.end(
                false,
                Found {
                    targets: vec![channel],
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            return Ok(ctx.end(
                true,
                Found {
                    targets: vec![channel.clone()],
                    pending: vec![channel],
                    ..Found::default()
                },
            ));
        }
    }
}

/// Notices: done when a notice for `me` with seq > the floor exists. A negative `after` floors at the latest seq when
/// the wait began; the floor is stored in the cursor, so a resumed call keeps it and misses no notice in between.
async fn notices_wait(
    ctx: &Ctx<'_>,
    me: Pub,
    after: i64,
    resumed: Option<Cursor>,
) -> Result<WaitResult, DaemonError> {
    let key = hex(&me);
    let watched = key.clone();
    let mut wake = Wake::Bus {
        rx: ctx.sh.bus.subscribe(),
        relevant: Box::new(
            move |e: &Event| matches!(e, Event::NoticePosted { target, .. } if *target == watched),
        ),
    };
    let floor = match known_floor(after, resumed.and_then(|c| c.notice_after)) {
        Some(floor) => floor,
        None => {
            let read = key.clone();
            ctx.sh
                .reader
                .exec(move |s| latest_notice_seq(s, &read))
                .await?
        }
    };
    loop {
        let read = key.clone();
        let rows = ctx
            .sh
            .reader
            .exec(move |s| s.notices(&read, floor, 1))
            .await?;
        if !rows.is_empty() {
            return Ok(ctx.end(false, Found::default()));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            let cursor = Cursor {
                notice_after: Some(floor),
                ..Cursor::default()
            };
            return Ok(ctx.end(
                true,
                Found {
                    cursor,
                    ..Found::default()
                },
            ));
        }
    }
}

/// Pid: done when none of the processes is alive (another user's process counts as exited).
async fn pid_wait(ctx: &Ctx<'_>, pids: Vec<i64>) -> Result<WaitResult, DaemonError> {
    non_empty(&pids, "name at least one process")?;
    within_targets(pids.len())?;
    if pids.iter().any(|p| *p <= 0) {
        return Err(rules::invalid("a process id must be greater than 0"));
    }
    let targets: Vec<String> = pids.iter().map(i64::to_string).collect();
    let mut wake = Wake::Timer;
    loop {
        let alive: Vec<String> = pids
            .iter()
            .filter(|p| watch::pid_alive(**p))
            .map(i64::to_string)
            .collect();
        if alive.is_empty() {
            return Ok(ctx.end(
                false,
                Found {
                    targets,
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, Some(PID_PROBE_MS)).await? {
            return Ok(ctx.end(
                true,
                Found {
                    targets,
                    pending: alive,
                    ..Found::default()
                },
            ));
        }
    }
}

/// File: done when every file exists with a size above zero. When exactly one distinct path was given and the
/// wait did not time out, its content may set the status (see `rules::numeric_status`).
async fn file_wait(ctx: &Ctx<'_>, paths: Vec<String>) -> Result<WaitResult, DaemonError> {
    non_empty(&paths, "name at least one file")?;
    within_targets(paths.len())?;
    let mut wake = Wake::Fs(watch::Watcher::new(&to_paths(&paths)));
    loop {
        let pending: Vec<String> = paths
            .iter()
            .filter(|p| !watch::file_ready(Path::new(p.as_str())))
            .cloned()
            .collect();
        if pending.is_empty() {
            let status = sole_path(&paths).map_or(0, |p| file_status(Path::new(p)));
            return Ok(ctx.end(
                false,
                Found {
                    status,
                    targets: paths,
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            return Ok(ctx.end(
                true,
                Found {
                    targets: paths,
                    pending,
                    ..Found::default()
                },
            ));
        }
    }
}

/// Gone: done when none of the paths exists.
async fn gone_wait(ctx: &Ctx<'_>, paths: Vec<String>) -> Result<WaitResult, DaemonError> {
    non_empty(&paths, "name at least one path")?;
    within_targets(paths.len())?;
    let mut wake = Wake::Fs(watch::Watcher::new(&to_paths(&paths)));
    loop {
        let pending: Vec<String> = paths
            .iter()
            .filter(|p| Path::new(p.as_str()).exists())
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(ctx.end(
                false,
                Found {
                    targets: paths,
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            return Ok(ctx.end(
                true,
                Found {
                    targets: paths,
                    pending,
                    ..Found::default()
                },
            ));
        }
    }
}

/// The paths of a request list, as the watcher and the checks take them.
fn to_paths(paths: &[String]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}
