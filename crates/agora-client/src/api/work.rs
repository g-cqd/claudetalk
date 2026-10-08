//! work *: scopes, tasks, claims, runs, waits, status, plan, settings.
//!
//! JSON keys of the `Out`s: `{"task"}`, `{"tasks"}`, `{"claimed", "reason"}`, `{"task", "first", "digest"}`,
//! `{"scope"}`, `{"scopes"}`, `{"report"}`, `{"events"}`, `{"plan", "text"}`, `{"wait"}`, `{"settings", "view"}`.
//! Text comes from [`super::work_text`].
//!
//! Every task this module returns carries its `digest`: the D5 digest of its command (`None` without one), which
//! `work.accept` must echo. The store does not keep it on the record, so it is derived here with the same
//! `claims::digest` the store checks against.

use super::work_text;
use crate::client::Client;
use crate::ops::{now_ms, sign_op};
use crate::{ClientError, Ctx, Out};
use agora_proto::{MAX_WAIT_MS, OpWire, Reply, Req};
use agora_sched::claims;
use agora_sched::envscrub;
use agora_sched::model::{
    AcceptArgs, AddArgs, CancelArgs, FinishArgs, NextArgs, NextOutcome, ReclaimArgs, RunArgs,
    ScopeArgs, SettingsArgs, TaskFilter, TaskRecord, UpdateArgs, WaitResult, WaitSpec,
};
use agora_sched::order::OrderPolicy;
use agora_sched::pyfmt::printable;
use agora_sched::settings::SettingsPatch;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
mod paths;
use paths::{absolute_spec, canonicalise_add, current_dir_string, home_dir, prepare_add};

pub async fn scope_set(ctx: &Ctx, a: ScopeArgs) -> Result<Out, ClientError> {
    let op = signed(ctx, "work.scope", &a.id, &a)?;
    let Reply::Scope { scope } = call(ctx, Req::WorkScope { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"scope": to_json(&scope)?}),
        text: work_text::scopes_text(std::slice::from_ref(&scope)),
    })
}

pub async fn scopes(ctx: &Ctx, root: Option<String>) -> Result<Out, ClientError> {
    let Reply::Scopes { scopes } = call(ctx, Req::WorkScopes { root }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"scopes": to_json(&scopes)?}),
        text: work_text::scopes_text(&scopes),
    })
}

/// Canonicalises `a.cwd` (default: the current directory) and `a.files` (`std::fs::canonicalize` of the longest
/// existing prefix, then `agora_sched::claims::canonical_path`) before signing, when the task names a command, a
/// cwd or files. A bare title is signed with no cwd.
pub async fn add(ctx: &Ctx, a: AddArgs) -> Result<Out, ClientError> {
    let mut a = a;
    prepare_add(&mut a, current_dir_string, home_dir().as_deref())?;
    let op = signed(ctx, "work.add", "", &a)?;
    let task = call_task(ctx, Req::WorkAdd { op }).await?;
    task_out(&task)
}

/// JSON adds `"digest"`; the text frames the command as untrusted data.
pub async fn show(ctx: &Ctx, id: &str) -> Result<Out, ClientError> {
    let record = task(ctx, id).await?;
    Ok(Out {
        json: show_json(&record)?,
        text: work_text::show_text(&record, now_ms()),
    })
}

pub async fn next(ctx: &Ctx, a: NextArgs) -> Result<Out, ClientError> {
    let op = signed(ctx, "work.next", "", &a)?;
    let Reply::Next { mut outcome } = call(ctx, Req::WorkNext { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    outcome.claimed = outcome.claimed.map(with_digest);
    Ok(Out {
        json: next_json(&outcome)?,
        text: work_text::next_text(&outcome),
    })
}

pub async fn claim(ctx: &Ctx, id: &str) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "work.claim", id, "{}");
    let task = call_task(ctx, Req::WorkClaim { op }).await?;
    task_out(&task)
}

pub async fn accept(ctx: &Ctx, id: &str, digest: &str) -> Result<Out, ClientError> {
    let op = signed(
        ctx,
        "work.accept",
        id,
        &AcceptArgs {
            digest: digest.to_string(),
        },
    )?;
    let Reply::Accepted { task, first } = call(ctx, Req::WorkAccept { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let task = with_digest(task);
    Ok(Out {
        json: accepted_json(&task, first)?,
        text: work_text::task_line(&task),
    })
}

/// Signs `work.run`; `env` = `agora_sched::envscrub::scrub(std::env::vars())` unless `env` is `Some`.
pub async fn run(
    ctx: &Ctx,
    a: RunArgs,
    env: Option<BTreeMap<String, String>>,
) -> Result<Out, ClientError> {
    let mut a = a;
    if let Some(add) = a.add.as_mut() {
        let base = current_dir_string()?;
        canonicalise_add(add, &base, home_dir().as_deref())?;
    }
    let subject = a.task.as_deref().unwrap_or("");
    let op = signed(ctx, "work.run", subject, &a)?;
    let env = select_env(env, std::env::vars_os());
    let task = call_task(ctx, Req::WorkRun { op, env }).await?;
    task_out(&task)
}

/// One `WorkWait` round trip (the timeout is clamped to 55 000 ms). File, gone and log paths are sent absolute, made
/// against the caller's current directory: the daemon resolves a relative path against its own.
pub async fn wait(
    ctx: &Ctx,
    spec: WaitSpec,
    timeout_ms: u64,
    cursor: Option<String>,
) -> Result<WaitResult, ClientError> {
    let spec = absolute_spec(spec, current_dir_string)?;
    let timeout_ms = clamp_wait(timeout_ms);
    let Reply::Wait { result } = call(
        ctx,
        Req::WorkWait {
            spec,
            timeout_ms,
            cursor,
        },
    )
    .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(result)
}

pub async fn finish(ctx: &Ctx, id: &str, a: FinishArgs) -> Result<Out, ClientError> {
    let op = signed(ctx, "work.finish", id, &a)?;
    let task = call_task(ctx, Req::WorkFinish { op }).await?;
    task_out(&task)
}

pub async fn cancel(ctx: &Ctx, id: &str, note: &str) -> Result<Out, ClientError> {
    let op = signed(
        ctx,
        "work.cancel",
        id,
        &CancelArgs {
            note: note.to_string(),
        },
    )?;
    let task = call_task(ctx, Req::WorkCancel { op }).await?;
    task_out(&task)
}

pub async fn update(ctx: &Ctx, id: &str, a: UpdateArgs) -> Result<Out, ClientError> {
    let op = signed(ctx, "work.update", id, &a)?;
    let task = call_task(ctx, Req::WorkUpdate { op }).await?;
    task_out(&task)
}

/// `subject` is `task:<id>` or `lease:<id>`.
pub async fn reclaim(ctx: &Ctx, subject: &str, note: &str) -> Result<Out, ClientError> {
    let op = signed(
        ctx,
        "work.reclaim",
        subject,
        &ReclaimArgs {
            note: note.to_string(),
        },
    )?;
    let Reply::Unit = call(ctx, Req::WorkReclaim { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(done(subject, format!("reclaimed {}", printable(subject))))
}

pub async fn status(ctx: &Ctx, scope: Option<String>) -> Result<Out, ClientError> {
    let Reply::WorkStatus { report } = call(ctx, Req::WorkStatus { scope }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"report": to_json(&report)?}),
        text: work_text::status_text(&report, now_ms()),
    })
}

pub async fn list(ctx: &Ctx, f: TaskFilter) -> Result<Out, ClientError> {
    let Reply::Tasks { tasks } = call(ctx, Req::WorkList { filter: f }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let tasks: Vec<TaskRecord> = tasks.into_iter().map(with_digest).collect();
    let text = tasks
        .iter()
        .map(work_text::task_line)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Out {
        json: json!({"tasks": to_json(&tasks)?}),
        text,
    })
}

pub async fn log(
    ctx: &Ctx,
    task: Option<String>,
    actor: Option<String>,
    scope: Option<String>,
    limit: u32,
) -> Result<Out, ClientError> {
    let Reply::WorkLog { events } = call(
        ctx,
        Req::WorkLog {
            task,
            actor,
            scope,
            limit,
        },
    )
    .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"events": to_json(&events)?}),
        text: work_text::log_text(&events),
    })
}

pub async fn plan(
    ctx: &Ctx,
    scope: Option<String>,
    policy: OrderPolicy,
    agents: u32,
    assign: bool,
    conflict_cost: Option<f64>,
) -> Result<Out, ClientError> {
    let Reply::WorkPlan { json: plan, text } = call(
        ctx,
        Req::WorkPlan {
            scope,
            policy,
            agents,
            assign,
            conflict_cost,
        },
    )
    .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: plan_json(&plan, &text)?,
        text,
    })
}

/// `None` reads the settings; `Some(patch)` signs `work.settings`.
pub async fn settings(ctx: &Ctx, patch: Option<SettingsPatch>) -> Result<Out, ClientError> {
    let op = patch
        .map(|patch| signed(ctx, "work.settings", "", &SettingsArgs { patch }))
        .transpose()?;
    let Reply::WorkSettings {
        settings,
        view,
        text,
    } = call(ctx, Req::WorkSettings { op }).await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(Out {
        json: json!({"settings": to_json(&settings)?, "view": view}),
        text,
    })
}

/// `WorkShow`, typed.
pub async fn task(ctx: &Ctx, id: &str) -> Result<TaskRecord, ClientError> {
    call_task(
        ctx,
        Req::WorkShow {
            task: id.to_string(),
        },
    )
    .await
}

/// Sends one request on a fresh connection.
async fn call(ctx: &Ctx, req: Req) -> Result<Reply, ClientError> {
    let mut client = Client::connect(ctx).await?;
    client.call(req).await
}

/// Sends a request whose reply is one task.
async fn call_task(ctx: &Ctx, req: Req) -> Result<TaskRecord, ClientError> {
    let Reply::Task { task } = call(ctx, req).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(with_digest(task))
}

/// Signs one op: `arg` is the compact JSON of the matching `*Args` struct.
fn signed<T: Serialize>(
    ctx: &Ctx,
    kind: &str,
    subject: &str,
    arg: &T,
) -> Result<OpWire, ClientError> {
    Ok(sign_op(&ctx.identity, kind, subject, &arg_json(arg)?))
}

fn arg_json<T: Serialize>(arg: &T) -> Result<String, ClientError> {
    serde_json::to_string(arg).map_err(proto_error)
}

/// The environment a run carries: `env` as given, else the scrubbed variables whose names and values are UTF-8.
fn select_env(
    env: Option<BTreeMap<String, String>>,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<String, String> {
    env.unwrap_or_else(|| {
        envscrub::scrub(vars.into_iter().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }))
    })
}

fn clamp_wait(timeout_ms: u64) -> u64 {
    timeout_ms.min(MAX_WAIT_MS)
}

/// The task with `digest` set to the D5 digest of its command (`None` without a command).
fn with_digest(mut task: TaskRecord) -> TaskRecord {
    task.digest = task
        .command
        .as_ref()
        .map(|argv| claims::digest(argv, task.cwd.as_deref().unwrap_or(""), &task.files));
    task
}

/// Output of a mutation that returns its task.
fn task_out(task: &TaskRecord) -> Result<Out, ClientError> {
    Ok(Out {
        json: json!({"task": to_json(task)?}),
        text: work_text::task_line(task),
    })
}

/// `{"task": …, "digest": …}`.
fn show_json(task: &TaskRecord) -> Result<serde_json::Value, ClientError> {
    Ok(json!({"task": to_json(task)?, "digest": task.digest}))
}

/// `{"claimed": task | null, "reason": …}`.
fn next_json(outcome: &NextOutcome) -> Result<serde_json::Value, ClientError> {
    let claimed = outcome.claimed.as_ref().map(to_json).transpose()?;
    Ok(json!({"claimed": claimed, "reason": outcome.reason}))
}

/// `{"task": …, "first": bool, "digest": …}`.
fn accepted_json(task: &TaskRecord, first: bool) -> Result<serde_json::Value, ClientError> {
    Ok(json!({"task": to_json(task)?, "first": first, "digest": task.digest}))
}

/// `{"plan": <the plan parsed from its JSON text>, "text": …}`.
fn plan_json(plan: &str, text: &str) -> Result<serde_json::Value, ClientError> {
    let plan: serde_json::Value = serde_json::from_str(plan).map_err(proto_error)?;
    Ok(json!({"plan": plan, "text": text}))
}

/// The `--json` result of a mutation that returns no record.
fn done(id: &str, text: String) -> Out {
    Out {
        json: json!({"ok": true, "id": id}),
        text,
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, ClientError> {
    serde_json::to_value(value).map_err(proto_error)
}

fn proto_error(e: serde_json::Error) -> ClientError {
    ClientError::Proto(e.into())
}

#[cfg(test)]
mod tests;
