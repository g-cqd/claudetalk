//! Tool handlers: work_scope, work_add, work_show, work_next, work_claim, work_accept, work_run, work_wait,
//! work_status, work_cancel, work_finish, work_settings, work_plan. They call `agora_client::api::work`; the argument
//! mapping and refusals are in `build`. Output that echoes another agent's titles, commands or job output is framed
//! as untrusted data. Over HTTP, `work_run`, `work_add` with a command, a working directory or files, `work_accept` of a
//! task with a command, and `work_wait` pid/file/gone/log are refused.

mod build;

use self::build::*;
use super::work_args::*;
use super::{ToolCx, ToolError, ToolOutput, ToolResult, framed_output, output};
use crate::framing::{UNTRUSTED_DATA_NOTICE, frame_text};
use agora_client::ClientError;
use agora_client::api::work as api;
use agora_client::api::work_text;
use agora_client::ops::now_ms;
use agora_sched::model::{FinishArgs, NextArgs, TaskRecord, WaitResult, WaitSpec};
use serde::Serialize;
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// Create or update a scope when a setter is given, else show the scopes under `id` (all when omitted).
pub async fn work_scope(cx: &ToolCx<'_>, a: WorkScopeArgs) -> ToolResult {
    let root = a.id.clone();
    match scope_setter(a)? {
        Some(args) => Ok(framed_output(api::scope_set(&cx.peer.ctx, args).await?)),
        None => Ok(framed_output(api::scopes(&cx.peer.ctx, root).await?)),
    }
}

/// Queue a task. A command, a working directory or files are refused over HTTP; the daemon checks the rest.
pub async fn work_add(cx: &ToolCx<'_>, a: WorkAddArgs) -> ToolResult {
    add_local_only(&a, cx.engine.config.allow_paths)?;
    let args = add_args(a)?;
    Ok(framed_output(api::add(&cx.peer.ctx, args).await?))
}

/// One task with its command, digest and acceptance line.
pub async fn work_show(cx: &ToolCx<'_>, a: WorkShowArgs) -> ToolResult {
    Ok(framed_output(api::show(&cx.peer.ctx, &a.id).await?))
}

/// Claim the next claimable task of the scope and policy given.
pub async fn work_next(cx: &ToolCx<'_>, a: WorkNextArgs) -> ToolResult {
    let args = NextArgs {
        scope: a.scope,
        policy: policy_of(a.policy.as_deref())?,
    };
    Ok(framed_output(api::next(&cx.peer.ctx, args).await?))
}

/// Claim one task by id.
pub async fn work_claim(cx: &ToolCx<'_>, a: WorkClaimArgs) -> ToolResult {
    Ok(framed_output(api::claim(&cx.peer.ctx, &a.id).await?))
}

/// Accept the command of a task this identity claimed, by its digest. Over HTTP a task with a command is refused.
pub async fn work_accept(cx: &ToolCx<'_>, a: WorkAcceptArgs) -> ToolResult {
    let allow_local = cx.engine.config.allow_paths;
    if !allow_local {
        let task = api::task(&cx.peer.ctx, &a.id).await?;
        accept_local_only(task.command.is_some(), allow_local)?;
    }
    Ok(framed_output(
        api::accept(&cx.peer.ctx, &a.id, &a.digest).await?,
    ))
}

/// Start a new command or a claimed, accepted task as a job, then wait up to `timeout_secs` for it to end. Structured
/// `{task, wait}`; the text is the job's status line. A timeout leaves the job running. Stdio only.
pub async fn work_run(cx: &ToolCx<'_>, a: WorkRunArgs) -> ToolResult {
    local_only(cx.engine.config.allow_paths, "running a command")?;
    let secs = wait_secs(a.timeout_secs);
    let args = run_args(&a)?;
    let started = api::run(&cx.peer.ctx, args, None).await?;
    let (task, id) = started_task(&started.json)?;
    let spec = WaitSpec::Tasks {
        ids: vec![id],
        any: false,
    };
    let wait = api::wait(&cx.peer.ctx, spec, secs * 1000, None);
    let mut result = during_wait(cx, secs, "waiting for the job", wait).await?;
    let (wait, text) = wait_output(&mut result, true, cx.engine.config.allow_paths)?;
    Ok(framed_parts(json!({"task": task, "wait": wait}), &text))
}

/// Wait for exactly one condition, up to `timeout_secs` (max 55). Pid, file, gone and log waits are refused over
/// HTTP. Structured: the WaitResult; a timed-out call returns a cursor to continue with.
pub async fn work_wait(cx: &ToolCx<'_>, a: WorkWaitArgs) -> ToolResult {
    let spec = wait_spec(&a)?;
    if let Some(what) = local_wait(&spec) {
        local_only(cx.engine.config.allow_paths, what)?;
    }
    let secs = wait_secs(a.timeout_secs);
    let tasks = matches!(spec, WaitSpec::Tasks { .. });
    let wait = api::wait(&cx.peer.ctx, spec, secs * 1000, a.cursor);
    let mut result = during_wait(cx, secs, "waiting", wait).await?;
    let (structured, text) = wait_output(&mut result, tasks, cx.engine.config.allow_paths)?;
    Ok(framed_parts(structured, &text))
}

/// Queue, running and waiting work of the scope, and the machine.
pub async fn work_status(cx: &ToolCx<'_>, a: WorkStatusArgs) -> ToolResult {
    Ok(framed_output(api::status(&cx.peer.ctx, a.scope).await?))
}

/// Cancel a task. A running job is stopped with a SIGTERM to its supervisor.
pub async fn work_cancel(cx: &ToolCx<'_>, a: WorkCancelArgs) -> ToolResult {
    let note = a.note.as_deref().unwrap_or_default();
    Ok(framed_output(api::cancel(&cx.peer.ctx, &a.id, note).await?))
}

/// Mark a task without a command done or failed.
pub async fn work_finish(cx: &ToolCx<'_>, a: WorkFinishArgs) -> ToolResult {
    let args = FinishArgs {
        state: finish_state(&a.state)?,
        note: a.note.unwrap_or_default(),
    };
    Ok(framed_output(api::finish(&cx.peer.ctx, &a.id, args).await?))
}

/// The work settings. With no field given they are read; with any field given they are changed (controller only).
pub async fn work_settings(cx: &ToolCx<'_>, a: WorkSettingsArgs) -> ToolResult {
    let patch = settings_patch(a);
    let patch = (!patch.is_empty()).then_some(patch);
    Ok(output(api::settings(&cx.peer.ctx, patch).await?))
}

/// Plan the queue of a scope, or the `tasks` given, for `agents` agents. Given tasks are planned on this side with no
/// daemon: the same code as `agora work plan --tasks`.
pub async fn work_plan(cx: &ToolCx<'_>, a: WorkPlanArgs) -> ToolResult {
    let agents = plan_agents(a.agents)?;
    let policy = policy_of(a.policy.as_deref())?;
    let assign = a.assign.unwrap_or(false);
    match &a.tasks {
        Some(tasks) => {
            // The compat planner is CPU-bound; it runs off the async workers.
            let tasks = tasks.clone();
            let conflict_cost = a.conflict_cost;
            let scope = a.scope.clone();
            let plan = tokio::task::spawn_blocking(move || {
                given_tasks_plan(
                    &tasks,
                    agents,
                    policy,
                    assign,
                    conflict_cost,
                    scope.as_deref(),
                )
            })
            .await
            .map_err(|e| ToolError {
                code: "internal".into(),
                message: format!("the plan stopped: {e}"),
            })??;
            Ok(framed_output(plan))
        }
        None => {
            let plan = api::plan(
                &cx.peer.ctx,
                a.scope,
                policy,
                a.agents,
                assign,
                a.conflict_cost,
            );
            Ok(framed_output(plan.await?))
        }
    }
}

/// The status lines of a finished wait: a task wait reports each task with the job output tail, any other condition
/// its own lines. The timeout hint is rewritten for MCP (see `mcp_hints`).
fn wait_lines(result: &WaitResult, tasks: bool) -> Vec<String> {
    let lines = if tasks {
        let tails = tails_of(&result.tasks, &result.tail, result.timed_out);
        work_text::report_text(&result.tasks, &tails, result.timed_out, now_ms())
    } else {
        work_text::wait_condition_text(result)
    };
    mcp_hints(lines)
}

/// The tail lines aligned with `tasks` for `report_text`: the daemon's tail belongs to one task, the first final task
/// of a finished wait or, on a timeout, the first task with a log. A started task stands in for "has a log" here, as
/// MCP does not read job logs. That task gets `tail`; every other task gets no lines.
fn tails_of(tasks: &[TaskRecord], tail: &[String], timed_out: bool) -> Vec<Vec<String>> {
    let mut tails = vec![Vec::new(); tasks.len()];
    if tail.is_empty() {
        return tails;
    }
    let selected = if timed_out {
        tasks.iter().position(|t| t.started_ms.is_some())
    } else {
        tasks.iter().position(|t| t.state.is_final())
    };
    if let Some(slot) = selected.and_then(|index| tails.get_mut(index)) {
        *slot = tail.to_vec();
    }
    tails
}

/// The hint of a wait that timed out names the CLI (`call `agora work wait ...` to keep waiting`); over MCP the model
/// continues with `work_wait` and the returned cursor. Every line that holds that hint is rewritten from the hint on:
/// its leading text (a status line's `work: ...; `) is kept, and the other lines are kept as they are.
fn mcp_hints(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().map(mcp_hint_line).collect()
}

/// One line with its CLI hint rewritten to the `work_wait` continuation; a line without the hint is returned as is.
fn mcp_hint_line(line: String) -> String {
    let Some((head, _)) = line.split_once("`agora work wait") else {
        return line;
    };
    let head = head.strip_suffix("call ").unwrap_or(head);
    format!("{head}call work_wait again with the returned cursor to keep waiting")
}

/// The structured JSON and the text of a finished wait. Over HTTP (`allow_paths` false) the log tail is left out of
/// both: it is a job's output, and a remote peer does not read job logs. On stdio the tail stays in both; the text is
/// framed by the caller.
fn wait_output(
    result: &mut WaitResult,
    tasks: bool,
    allow_paths: bool,
) -> Result<(Value, String), ToolError> {
    if !allow_paths {
        result.tail.clear();
    }
    let text = wait_lines(result, tasks).join("\n");
    let mut structured = json_of(result)?;
    if let Some(map) = structured.as_object_mut().filter(|_| !allow_paths) {
        map.remove("tail");
    }
    Ok((structured, text))
}

/// Awaits a daemon wait of `secs` seconds. With a progress token it reports the elapsed seconds every
/// `progress_interval_ms`; a cancelled request drops the wait, and the work itself carries on.
async fn during_wait<T>(
    cx: &ToolCx<'_>,
    secs: u64,
    what: &str,
    fut: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ToolError> {
    tokio::pin!(fut);
    let started = Instant::now();
    let mut ticks = match &cx.progress {
        Some(_) => {
            let period = Duration::from_millis(cx.engine.config.progress_interval_ms.max(1));
            let mut ticks = tokio::time::interval(period);
            // The first tick of an interval is immediate; progress starts one period in.
            ticks.tick().await;
            Some(ticks)
        }
        None => None,
    };
    let total = f64::from(u32::try_from(secs).unwrap_or(u32::MAX));
    loop {
        tokio::select! {
            res = &mut fut => return res.map_err(ToolError::from),
            () = cx.cancel.cancelled() => {
                return Err(ToolError {
                    code: "cancelled".into(),
                    message: "request cancelled".into(),
                });
            }
            at = next_tick(&mut ticks) => {
                if let Some(progress) = &cx.progress {
                    let elapsed = at.duration_since(started).as_secs_f64();
                    progress.report(elapsed, Some(total), what);
                }
            }
        }
    }
}

/// Resolves at each progress tick; pends forever when there is no progress token.
async fn next_tick(ticks: &mut Option<tokio::time::Interval>) -> Instant {
    match ticks {
        Some(ticks) => ticks.tick().await,
        None => std::future::pending().await,
    }
}
/// A tool result whose text is framed as untrusted data; a structured object gets the same notice.
fn framed_parts(mut structured: Value, text: &str) -> ToolOutput {
    if let Value::Object(map) = &mut structured {
        map.insert(
            "notice".to_owned(),
            Value::String(UNTRUSTED_DATA_NOTICE.to_owned()),
        );
    }
    ToolOutput {
        structured,
        text: frame_text(text),
    }
}

/// `value` as JSON; a serialisation failure is a protocol error.
fn json_of<T: Serialize>(value: &T) -> Result<Value, ToolError> {
    serde_json::to_value(value).map_err(|e| ToolError::from(ClientError::Proto(e.into())))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod hint_tests {
    use super::{mcp_hint_line, mcp_hints};

    #[test]
    fn the_report_hint_is_rewritten_for_mcp_and_the_other_lines_are_kept() {
        let lines = vec![
            "work: job t5 still running after 5.0 s (waited 1.0 s for capacity)".to_owned(),
            "work: call `agora work wait t5 t6` to keep waiting".to_owned(),
        ];
        assert_eq!(
            mcp_hints(lines),
            [
                "work: job t5 still running after 5.0 s (waited 1.0 s for capacity)",
                "work: call work_wait again with the returned cursor to keep waiting",
            ]
        );
    }

    #[test]
    fn the_condition_hint_keeps_its_leading_text_and_drops_the_cli_command() {
        let line = "work: still present after 1.0 s: /tmp/a; call `agora work wait --gone /tmp/a` to keep waiting";
        assert_eq!(
            mcp_hint_line(line.to_owned()),
            "work: still present after 1.0 s: /tmp/a; call work_wait again with the returned cursor to keep waiting"
        );
    }

    #[test]
    fn a_line_with_the_cli_hint_anywhere_is_rewritten_from_the_hint_on() {
        let line = "no match after 3.0 s: /tmp/x.log; call `agora work wait --log /tmp/x.log PATTERN` to keep waiting";
        assert_eq!(
            mcp_hint_line(line.to_owned()),
            "no match after 3.0 s: /tmp/x.log; call work_wait again with the returned cursor to keep waiting"
        );
    }

    #[test]
    fn no_hint_line_gives_the_lines_back_unchanged() {
        let lines = vec!["work: task t6 is still queued".to_owned()];
        assert_eq!(mcp_hints(lines.clone()), lines);
        assert!(mcp_hints(Vec::new()).is_empty());
    }
}
