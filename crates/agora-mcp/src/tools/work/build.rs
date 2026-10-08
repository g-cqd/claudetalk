//! Argument mapping of the work tools: each `work_*` argument to the `agora_sched` request the api takes, and the
//! refusals made before any call (one wait condition, a non-empty command, the agent cap, the local-only checks).
//! `given_tasks_plan` runs `agora_sched::compat` in-process; nothing here does I/O.

use crate::tools::ToolError;
use crate::tools::work_args::*;
use agora_client::{ClientError, Out};
use agora_sched::compat;
use agora_sched::model::{AddArgs, RunArgs, ScopeArgs, ScopeKind, TaskState, WaitSpec};
use agora_sched::order::OrderPolicy;
use agora_sched::plan::check_agents;
use agora_sched::settings::SettingsPatch;
use serde_json::{Value, json};

/// Longest wait of one call, in seconds.
const MAX_WAIT_SECS: u64 = 55;
/// The name of the given tasks document inside `compat::run`; its `read` closure serves this name and nothing else.
const GIVEN_TASKS_PATH: &str = "work-plan-tasks.json";
/// The usage message when a wait names no condition, or more than one.
pub(super) const WAIT_ONE: &str = "wait for one thing: tasks, all, cards, channel, notices, pid, file, gone or log_path with pattern";
/// Refuses what needs this machine (a command, a process or a path to watch) when the request came over HTTP. The
/// flag is `EngineConfig::allow_paths`, which is set on stdio only, as `chat`'s `path_guard` uses it.
pub(super) fn local_only(allow_local: bool, what: &str) -> Result<(), ToolError> {
    if allow_local {
        Ok(())
    } else {
        Err(ToolError::usage(format!(
            "{what} is not available over HTTP"
        )))
    }
}

/// What a wait on this machine watches, when the spec is one of those; `None` for the daemon-side conditions.
pub(super) fn local_wait(spec: &WaitSpec) -> Option<&'static str> {
    match spec {
        WaitSpec::Pid { .. } => Some("waiting for a pid"),
        WaitSpec::File { .. } => Some("waiting for a file"),
        WaitSpec::Gone { .. } => Some("waiting for a path to disappear"),
        WaitSpec::Log { .. } => Some("watching a log"),
        _ => None,
    }
}

/// Refuses a `work_add` over HTTP when it names a command, a working directory or files: the daemon would run the
/// command or read the paths on this machine, which an HTTP peer cannot ask for. A file list that is empty is no files.
pub(super) fn add_local_only(a: &WorkAddArgs, allow_local: bool) -> Result<(), ToolError> {
    if a.command.is_some() {
        local_only(allow_local, "a command in work_add")?;
    }
    if a.cwd.is_some() || present(a.files.as_deref()).is_some() {
        local_only(allow_local, "cwd and files in work_add")?;
    }
    Ok(())
}

/// Refuses the accept of a task that has a command when the request came over HTTP: accepting it lets the claimant
/// run the command, and an HTTP peer can never cause a command to run.
pub(super) fn accept_local_only(has_command: bool, allow_local: bool) -> Result<(), ToolError> {
    if has_command {
        local_only(allow_local, "accepting a command")
    } else {
        Ok(())
    }
}

/// `timeout_secs` capped at the longest wait of one call.
pub(super) fn wait_secs(timeout_secs: u64) -> u64 {
    timeout_secs.min(MAX_WAIT_SECS)
}

/// The list when it has at least one item.
fn present<T>(items: Option<&[T]>) -> Option<&[T]> {
    items.filter(|items| !items.is_empty())
}

/// The one condition a `work_wait` call names, as the daemon's `WaitSpec`: exactly one of tasks, all, cards, channel,
/// notices, pid, file, gone or log (log needs both `log_path` and `pattern`).
pub(super) fn wait_spec(a: &WorkWaitArgs) -> Result<WaitSpec, ToolError> {
    let log = match (a.log_path.as_deref(), a.pattern.as_deref()) {
        (Some(path), Some(pattern)) => Some((path, pattern)),
        (None, None) => None,
        _ => {
            return Err(ToolError::usage(
                "the log condition needs both log_path and pattern",
            ));
        }
    };
    let tasks = present(a.tasks.as_deref());
    let cards = present(a.cards.as_deref());
    let pids = present(a.pid.as_deref());
    let files = present(a.file.as_deref());
    let gone = present(a.gone.as_deref());
    let all = a.all == Some(true);
    let notices = a.notices == Some(true);
    let named: Vec<&str> = [
        ("tasks", tasks.is_some()),
        ("all", all),
        ("cards", cards.is_some()),
        ("channel", a.channel.is_some()),
        ("notices", notices),
        ("pid", pids.is_some()),
        ("file", files.is_some()),
        ("gone", gone.is_some()),
        ("log", log.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, given)| given.then_some(name))
    .collect();
    match named.as_slice() {
        [_] => {}
        [] => return Err(ToolError::usage(WAIT_ONE)),
        _ => {
            return Err(ToolError::usage(format!(
                "{WAIT_ONE}, not {}",
                named.join(" and ")
            )));
        }
    }
    let scope = a.scope.as_deref().filter(|scope| !scope.is_empty());
    if a.any == Some(true) && tasks.is_none() {
        return Err(ToolError::usage("any goes with tasks"));
    }
    if scope.is_some() && !all {
        return Err(ToolError::usage("scope goes with all"));
    }
    if let Some(ids) = tasks {
        return Ok(WaitSpec::Tasks {
            ids: ids.to_vec(),
            any: a.any == Some(true),
        });
    }
    if all {
        return Ok(WaitSpec::All {
            scope: scope.map(str::to_owned),
        });
    }
    if let Some(ids) = cards {
        return Ok(WaitSpec::Cards {
            ids: ids.to_vec(),
            states: a.states.clone().unwrap_or_default(),
        });
    }
    if let Some(channel) = &a.channel {
        return Ok(WaitSpec::Messages {
            channel: channel.clone(),
            after: a.after.unwrap_or(0),
        });
    }
    if notices {
        return Ok(WaitSpec::Notices {
            after: a.after.unwrap_or(-1),
        });
    }
    if let Some(pids) = pids {
        if let Some(pid) = pids.iter().find(|pid| **pid <= 0) {
            return Err(ToolError::usage(format!("pid {pid} is not a process id")));
        }
        return Ok(WaitSpec::Pid {
            pids: pids.to_vec(),
        });
    }
    if let Some(paths) = files {
        return Ok(WaitSpec::File {
            paths: paths.to_vec(),
        });
    }
    if let Some(paths) = gone {
        return Ok(WaitSpec::Gone {
            paths: paths.to_vec(),
        });
    }
    if let Some((path, pattern)) = log {
        return Ok(WaitSpec::Log {
            path: path.to_owned(),
            pattern: pattern.to_owned(),
        });
    }
    Err(ToolError::usage(WAIT_ONE))
}
/// The `policy` argument: fifo when absent.
pub(super) fn policy_of(policy: Option<&str>) -> Result<OrderPolicy, ToolError> {
    match policy {
        None => Ok(OrderPolicy::default()),
        Some(text) => OrderPolicy::parse(text)
            .ok_or_else(|| ToolError::usage("policy must be fifo, wsjf or critical-path")),
    }
}

/// The `state` argument of `work_finish`: done or failed only.
pub(super) fn finish_state(state: &str) -> Result<TaskState, ToolError> {
    match TaskState::parse(state) {
        Some(state @ (TaskState::Done | TaskState::Failed)) => Ok(state),
        _ => Err(ToolError::usage("state must be done or failed")),
    }
}

/// The `ScopeArgs` of a setter call, or `None` when no setter is given (the call shows scopes).
pub(super) fn scope_setter(a: WorkScopeArgs) -> Result<Option<ScopeArgs>, ToolError> {
    if a.kind.is_none() && a.parent.is_none() && a.task.is_none() {
        return Ok(None);
    }
    let id = match a.id {
        Some(id) if !id.is_empty() => id,
        _ => return Err(ToolError::usage("a scope id is required to set a scope")),
    };
    let kind = a
        .kind
        .as_deref()
        .map(|kind| {
            ScopeKind::parse(kind)
                .ok_or_else(|| ToolError::usage("kind must be project, conversation or agent"))
        })
        .transpose()?;
    Ok(Some(ScopeArgs {
        id,
        kind,
        parent: a.parent,
        task: a.task,
    }))
}

/// The `AddArgs` of a `work_add` call, with the queue's defaults (kind other, size, value and weight 1).
pub(super) fn add_args(a: WorkAddArgs) -> Result<AddArgs, ToolError> {
    if let Some(argv) = &a.command {
        non_empty_argv(argv)?;
    }
    let defaults = AddArgs::default();
    Ok(AddArgs {
        id: a.id,
        title: a.title.unwrap_or_default(),
        kind: a.kind.unwrap_or(defaults.kind),
        scope: a.scope,
        after: a.after.unwrap_or_default(),
        size: a.size.unwrap_or(defaults.size),
        value: a.value.unwrap_or(defaults.value),
        files: a.files.unwrap_or_default(),
        weight: a.weight.unwrap_or(defaults.weight),
        command: a.command,
        cwd: a.cwd,
        card: a.card,
    })
}

/// The `RunArgs` of a `work_run` call: a claimed task with no job fields, or a new command.
pub(super) fn run_args(a: &WorkRunArgs) -> Result<RunArgs, ToolError> {
    match (a.task.as_deref(), a.command.as_deref()) {
        (Some(task), None) => {
            let job_fields = a.cwd.is_some()
                || a.title.is_some()
                || a.kind.is_some()
                || a.weight.is_some()
                || a.files.is_some()
                || a.scope.is_some()
                || a.card.is_some();
            if job_fields {
                return Err(ToolError::usage(
                    "cwd, title, kind, weight, files, scope and card go with a new command, not with a task",
                ));
            }
            Ok(RunArgs {
                task: Some(task.to_owned()),
                add: None,
            })
        }
        (None, Some(argv)) => {
            let add = WorkAddArgs {
                title: a.title.clone(),
                kind: a.kind.clone(),
                weight: a.weight,
                files: a.files.clone(),
                scope: a.scope.clone(),
                cwd: a.cwd.clone(),
                card: a.card.clone(),
                command: Some(argv.to_vec()),
                ..WorkAddArgs::default()
            };
            Ok(RunArgs {
                task: None,
                add: Some(add_args(add)?),
            })
        }
        (Some(_), Some(_)) => Err(ToolError::usage("give a task or a command, not both")),
        (None, None) => Err(ToolError::usage("give a task you claimed or a command")),
    }
}

/// A command is an argument vector with at least a program name.
fn non_empty_argv(argv: &[String]) -> Result<(), ToolError> {
    if argv.is_empty() {
        Err(ToolError::usage("command must not be empty"))
    } else {
        Ok(())
    }
}

/// The task object and its id from a `work.run` reply (`{"task": …}`).
pub(super) fn started_task(json: &Value) -> Result<(Value, String), ToolError> {
    let task = json.get("task").cloned().ok_or_else(unexpected_reply)?;
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(unexpected_reply)?;
    Ok((task, id))
}

fn unexpected_reply() -> ToolError {
    ToolError::from(ClientError::UnexpectedReply)
}

/// `agents` checked against 1..=256 (`agora_sched::plan::check_agents`); a refusal is a usage error.
pub(super) fn plan_agents(agents: u32) -> Result<usize, ToolError> {
    let n = usize::try_from(agents).unwrap_or(usize::MAX);
    check_agents(n).map_err(ToolError::usage)
}

/// `work plan --tasks` run in-process: the given tasks (a JSON array) are the document `compat::run` reads under
/// `GIVEN_TASKS_PATH`. Structured `{plan, text}`, as `api::work::plan`: the plan is the `--json` run, the text is the
/// same plan as a person reads it (the run without `--json`). A refusal of `compat::run` is a usage error.
pub(super) fn given_tasks_plan(
    tasks: &[Value],
    agents: usize,
    policy: OrderPolicy,
    assign: bool,
    conflict_cost: Option<f64>,
    scope: Option<&str>,
) -> Result<Out, ToolError> {
    let tasks_text =
        serde_json::to_string(tasks).map_err(|e| ToolError::from(ClientError::Proto(e.into())))?;
    let plan_text = compat_stdout(
        &plan_argv(agents, policy, assign, conflict_cost, scope, true),
        &tasks_text,
    )?;
    let plan: Value = serde_json::from_str(&plan_text)
        .map_err(|e| ToolError::from(ClientError::Proto(e.into())))?;
    let text = compat_stdout(
        &plan_argv(agents, policy, assign, conflict_cost, scope, false),
        &tasks_text,
    )?
    .trim_end()
    .to_owned();
    Ok(Out {
        json: json!({"plan": plan, "text": text}),
        text,
    })
}

/// The `work plan --tasks` argument vector of the options given, with `--json` when `json`.
fn plan_argv(
    agents: usize,
    policy: OrderPolicy,
    assign: bool,
    conflict_cost: Option<f64>,
    scope: Option<&str>,
    json: bool,
) -> Vec<String> {
    let mut argv = vec![
        "plan".to_owned(),
        "--agents".to_owned(),
        agents.to_string(),
        "--policy".to_owned(),
        policy.as_str().to_owned(),
    ];
    if json {
        argv.push("--json".to_owned());
    }
    argv.extend(["--tasks".to_owned(), GIVEN_TASKS_PATH.to_owned()]);
    if assign {
        argv.push("--assign".to_owned());
    }
    if let Some(cost) = conflict_cost {
        argv.extend(["--conflict-cost".to_owned(), cost.to_string()]);
    }
    if let Some(scope) = scope {
        argv.extend(["--scope".to_owned(), scope.to_owned()]);
    }
    argv
}

/// The stdout of `compat::run` over `argv`, with `tasks_text` as the given tasks file; a refusal is a usage error.
fn compat_stdout(argv: &[String], tasks_text: &str) -> Result<String, ToolError> {
    let out = compat::run(argv, &mut |path: &str| {
        if path == GIVEN_TASKS_PATH {
            Ok(tasks_text.to_owned())
        } else {
            Err(format!("no such tasks file: {path}"))
        }
    });
    if out.exit_code != 0 {
        return Err(ToolError::usage(compat_message(&out.stderr)));
    }
    Ok(out.stdout)
}

/// The message of a `compat::run` refusal: its stderr line without the `work: ` prefix.
fn compat_message(stderr: &str) -> String {
    let line = stderr.trim_end();
    line.strip_prefix("work: ").unwrap_or(line).to_owned()
}
/// The `SettingsPatch` of the given fields; a patch with no field reads the settings.
pub(super) fn settings_patch(a: WorkSettingsArgs) -> SettingsPatch {
    SettingsPatch {
        cpu_budget: a.cpu_budget,
        capacity: a.capacity,
        memory_budget: a.memory_budget,
        idle_min: a.idle_min,
        memory_limit: a.memory_limit,
        load_factor: a.load_factor,
        load_wait: a.load_wait,
        log_limit: a.log_limit,
        keep_days: a.keep_days,
        backfill_wait: a.backfill_wait,
        small_weight: a.small_weight,
        aging: a.aging,
        job_qos: a.job_qos,
        quiet_drain: a.quiet_drain,
        quiet_max: a.quiet_max,
        self_accept: a.self_accept,
        timer_quota: a.timer_quota,
        disk_floor: a.disk_floor,
        memory_floor: a.memory_floor,
        idle_floor: a.idle_floor,
        reconcile_interval: a.reconcile_interval,
        lease_ttl: a.lease_ttl,
        run_role: a.run_role,
        max_waiting: a.max_waiting,
    }
}
