//! `agora work run` and `agora work wait`: loop `WorkWait` round trips until the job ends or the overall
//! `--timeout` passes, then print the work.py report. Ctrl-C exits 130 and the job continues.

mod output;
mod request;
#[cfg(test)]
mod tests;
mod wait_spec;

use crate::{CliError, Ctx, WorkRunArgs, WorkWaitArgs};
use agora_client::api::work;
use agora_sched::exit::{EXIT_INPUT, EXIT_INTERRUPTED};
use agora_sched::model::{TaskRecord, WaitResult, WaitSpec};
use output::{Announcer, finish_condition, finish_tasks, waiting_line};
use request::{env_scope, run_request, timeout};
use serde_json::Value;
use std::time::Instant;
use wait_spec::wait_spec;

const POLL_CAP_MS: u64 = 55_000;

/// Returns the process exit status (`agora_sched::exit::wait_exit`, 124 on timeout).
pub async fn run(ctx: &Ctx, json: bool, a: WorkRunArgs) -> Result<i32, CliError> {
    let base = std::env::current_dir()?;
    let request = match run_request(&a, env_scope().as_deref(), &base) {
        Ok(request) => request,
        Err(message) => return Ok(usage(&message)),
    };
    let timeout = match timeout(a.timeout) {
        Ok(timeout) => timeout,
        Err(message) => return Ok(usage(&message)),
    };
    let out = work::run(ctx, request, None).await?;
    let started = task_of(&out.json)?;
    let deadline = Instant::now() + timeout;
    let spec = WaitSpec::Tasks {
        ids: vec![started.id.clone()],
        any: false,
    };
    let mut announcer = Announcer::default();
    let waited = tokio::select! {
        result = wait_until(ctx, spec, deadline, |result| {
            if let (false, Some(task)) = (json, result.tasks.first())
                && let Some(line) = announcer.observe(task)
            {
                println!("{line}");
            }
        }) => result?,
        _ = tokio::signal::ctrl_c() => return Ok(EXIT_INTERRUPTED),
    };
    finish_tasks(&waited, false, a.tail, json)
}

/// Returns the process exit status.
pub async fn wait(ctx: &Ctx, json: bool, a: WorkWaitArgs) -> Result<i32, CliError> {
    let spec = match wait_spec(&a, env_scope().as_deref()) {
        Ok(spec) => spec,
        Err(message) => return Ok(usage(&message)),
    };
    let timeout = match timeout(a.timeout) {
        Ok(timeout) => timeout,
        Err(message) => return Ok(usage(&message)),
    };
    let task_wait = match &spec {
        WaitSpec::Tasks { ids, any } => Some((ids.clone(), *any)),
        _ => None,
    };
    if let Some((ids, any)) = &task_wait
        && !json
    {
        println!("{}", waiting_line(ids, *any));
    }
    let deadline = Instant::now() + timeout;
    let mut result = tokio::select! {
        result = wait_until(ctx, spec, deadline, |_| {}) => result?,
        _ = tokio::signal::ctrl_c() => return Ok(EXIT_INTERRUPTED),
    };
    match task_wait {
        Some((_, any)) => finish_tasks(&result, any, a.tail, json),
        None => Ok(finish_condition(&mut result, a.tail, json)),
    }
}

fn usage(message: &str) -> i32 {
    eprintln!("work: {message}");
    EXIT_INPUT
}

/// The task record inside a `work.run` reply (`{"task": …}`).
fn task_of(json: &Value) -> Result<TaskRecord, CliError> {
    let task = json.get("task").cloned().ok_or(CliError::UnexpectedReply)?;
    serde_json::from_value(task).map_err(|_| CliError::UnexpectedReply)
}

/// Repeats `WorkWait` round trips until the wait ends on its own or `deadline` passes, and returns the last result.
/// A server-side timeout with time left just loops, with no sleep. `on_result` sees every round trip.
async fn wait_until(
    ctx: &Ctx,
    spec: WaitSpec,
    deadline: Instant,
    mut on_result: impl FnMut(&WaitResult),
) -> Result<WaitResult, CliError> {
    let mut cursor = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .min(POLL_CAP_MS);
        let result = work::wait(ctx, spec.clone(), timeout_ms, cursor.take()).await?;
        on_result(&result);
        if !result.timed_out || Instant::now() >= deadline {
            return Ok(result);
        }
        cursor = result.cursor.clone();
    }
}
