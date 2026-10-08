//! `agora work ...` except `run` and `wait` (those are in `cmd_work_run`): clap args -> `agora_client::api::{work,
//! sched}` -> work.py-style output. Exit codes (ref-queue-cli §7): success 0, refusal 1, input error 2, `next` with
//! nothing claimable 1 (its message on stdout). `--agent` is accepted and ignored (D2).

use crate::{
    CliError, Ctx, Out, ScheduleAddArgs, ScheduleCmd, ScheduleWhen, ScopeCmd, WorkAddArgs,
    WorkArgs, WorkCloseArgs, WorkCmd, WorkSettingsArgs, WorkUpdateArgs, cmd_work_run,
};
use agora_client::api::{sched, work};
use agora_client::identity;
use agora_daemon::Paths;
use agora_sched::claims::{check_command, validate_kind, validate_name};
use agora_sched::compat::{self, CompatOutput};
use agora_sched::exit::{EXIT_INPUT, EXIT_REFUSED};
use agora_sched::model::{
    AddArgs, FinishArgs, NextArgs, ScopeArgs, ScopeKind, TaskState, TimerArgs, UpdateArgs,
};
use agora_sched::plan;
use agora_sched::settings::SettingsPatch;
use agora_sched::timer::{self, TimerSpec};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

/// Handles every `work` verb except `run` and `wait` (`cmd_work_run` runs those) and returns the exit code, masked
/// to 0..=255. Loads the folder identity and builds its own runtime. An error goes to stdout as
/// `{"error": {code, message}}` with `--json`, else to stderr as `work: <message>`.
pub fn run(home: PathBuf, json: bool, args: WorkArgs) -> ExitCode {
    match execute(home, json, args.cmd) {
        Ok(code) => exit_code(code),
        Err(e) => {
            report_error(&e, json);
            exit_code(exit_for_error(&e))
        }
    }
}

/// `work order|plan` with `--tasks`/`--task`: `args` are the arguments after `work`. No daemon, no identity:
/// read the files (`-` = stdin), call `agora_sched::compat::run`, write its stdout/stderr, exit with its code.
pub fn given_tasks(args: &[String]) -> ExitCode {
    let out = given_tasks_with(args, &mut std::io::stdin().lock());
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    exit_code(out.exit_code)
}

fn execute(home: PathBuf, json: bool, cmd: WorkCmd) -> Result<i32, CliError> {
    let paths = Paths::new(&home);
    let (identity, label) = identity::load_identity(&paths)?;
    let ctx = Ctx {
        paths,
        identity,
        label,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(dispatch(&ctx, json, cmd))
}

async fn dispatch(ctx: &Ctx, json: bool, cmd: WorkCmd) -> Result<i32, CliError> {
    match cmd {
        WorkCmd::Run(a) => cmd_work_run::run(ctx, json, a).await,
        WorkCmd::Wait(a) => cmd_work_run::wait(ctx, json, a).await,
        other => {
            let (out, code) = verb(ctx, other).await?;
            print_out(&out, json);
            Ok(code)
        }
    }
}

/// One daemon-backed verb: its `Out` and the exit code it implies (0, or 1 for `next` with nothing claimable).
async fn verb(ctx: &Ctx, cmd: WorkCmd) -> Result<(Out, i32), CliError> {
    match cmd {
        WorkCmd::Scope(ScopeCmd::Set {
            id,
            kind,
            parent,
            task,
            ..
        }) => {
            check_id("scope", &id)?;
            if let Some(p) = parent.as_deref().filter(|p| !p.is_empty()) {
                check_id("parent scope", p)?;
            }
            let args = ScopeArgs {
                id,
                kind: scope_kind(kind.as_deref())?,
                parent,
                task,
            };
            plain(work::scope_set(ctx, args).await?)
        }
        WorkCmd::Scope(ScopeCmd::Show { id }) => {
            check_scope(id.as_deref())?;
            plain(work::scopes(ctx, scope_arg(id)).await?)
        }
        WorkCmd::Add(a) => plain(work::add(ctx, add_args(a)?).await?),
        WorkCmd::Order(a) => {
            check_scope(a.scope.as_deref())?;
            plain(work::plan(ctx, scope_arg(a.scope), a.policy.into(), 1, false, None).await?)
        }
        WorkCmd::Plan(a) => {
            check_scope(a.scope.as_deref())?;
            check_agents(a.agents)?;
            plain(
                work::plan(
                    ctx,
                    scope_arg(a.scope),
                    a.policy.into(),
                    a.agents,
                    a.assign,
                    a.conflict_cost,
                )
                .await?,
            )
        }
        WorkCmd::Next(a) => {
            check_scope(a.scope.as_deref())?;
            let args = NextArgs {
                scope: scope_arg(a.scope),
                policy: a.policy.into(),
            };
            let out = work::next(ctx, args).await?;
            let code = if claimed_none(&out) { EXIT_REFUSED } else { 0 };
            Ok((out, code))
        }
        WorkCmd::Claim(a) => {
            check_id("task", &a.id)?;
            plain(work::claim(ctx, &a.id).await?)
        }
        WorkCmd::Accept(a) => {
            check_id("task", &a.id)?;
            plain(work::accept(ctx, &a.id, &normalize_digest(&a.digest)).await?)
        }
        WorkCmd::Show { id } => {
            check_id("task", &id)?;
            plain(work::show(ctx, &id).await?)
        }
        WorkCmd::Done(a) => finish(ctx, a, TaskState::Done).await,
        WorkCmd::Fail(a) => finish(ctx, a, TaskState::Failed).await,
        WorkCmd::Cancel(a) => {
            check_id("task", &a.id)?;
            plain(work::cancel(ctx, &a.id, &a.note).await?)
        }
        WorkCmd::Update(a) => {
            check_id("task", &a.id)?;
            let args = update_args(&a)?;
            plain(work::update(ctx, &a.id, args).await?)
        }
        WorkCmd::Reclaim { subject, note } => plain(work::reclaim(ctx, &subject, &note).await?),
        WorkCmd::Settings(a) => plain(work::settings(ctx, settings_request(&a)).await?),
        WorkCmd::Status { scope } => {
            check_scope(scope.as_deref())?;
            plain(status_out(work::status(ctx, scope_arg(scope)).await?))
        }
        WorkCmd::Log(a) => {
            if let Some(task) = &a.task {
                check_id("task", task)?;
            }
            check_scope(a.scope.as_deref())?;
            plain(work::log(ctx, a.task, a.actor, scope_arg(a.scope), a.limit).await?)
        }
        WorkCmd::Schedule(ScheduleCmd::Add(a)) => {
            plain(sched::timer_create(ctx, timer_args(a)?).await?)
        }
        WorkCmd::Schedule(ScheduleCmd::List { owner }) => plain(sched::timers(ctx, owner).await?),
        WorkCmd::Schedule(ScheduleCmd::Rm { id }) => {
            check_id("timer", &id)?;
            plain(sched::timer_delete(ctx, &id).await?)
        }
        WorkCmd::Run(_) | WorkCmd::Wait(_) => Err(CliError::Usage(
            "work run and work wait are handled by cmd_work_run".into(),
        )),
    }
}

fn plain(out: Out) -> Result<(Out, i32), CliError> {
    Ok((out, 0))
}

async fn finish(ctx: &Ctx, a: WorkCloseArgs, state: TaskState) -> Result<(Out, i32), CliError> {
    check_id("task", &a.id)?;
    let args = FinishArgs {
        state,
        note: a.note,
    };
    plain(work::finish(ctx, &a.id, args).await?)
}

/// Usage errors (exit 2) for names and numbers that work.py checks at argument parsing.
fn check_id(what: &str, value: &str) -> Result<(), CliError> {
    validate_name(what, value).map_err(CliError::Usage)
}

/// A `--scope` filter: the empty value means every scope, so only a non-empty one is a name.
fn check_scope(scope: Option<&str>) -> Result<(), CliError> {
    match scope {
        Some(s) if !s.is_empty() => check_id("scope", s),
        _ => Ok(()),
    }
}

/// `plan --agents N` on the queue path: the same 1..=256 bound the daemon applies (a usage error, exit 2).
fn check_agents(agents: u32) -> Result<(), CliError> {
    plan::check_agents(usize::try_from(agents).unwrap_or(usize::MAX))
        .map(|_| ())
        .map_err(CliError::Usage)
}

fn scope_arg(scope: Option<String>) -> Option<String> {
    scope.filter(|s| !s.is_empty())
}

fn check_positive(flag: &str, value: Option<f64>) -> Result<(), CliError> {
    match value {
        Some(v) if !(v.is_finite() && v > 0.0) => Err(CliError::Usage(format!(
            "{flag} must be a positive number, not {v}"
        ))),
        _ => Ok(()),
    }
}

fn scope_kind(kind: Option<&str>) -> Result<Option<ScopeKind>, CliError> {
    kind.map(|k| {
        ScopeKind::parse(k).ok_or_else(|| {
            CliError::Usage(format!(
                "argument --kind: invalid choice: {k:?} (choose from 'project', 'conversation', 'agent')"
            ))
        })
    })
    .transpose()
}

fn current_dir_string() -> Result<String, CliError> {
    std::env::current_dir()?
        .into_os_string()
        .into_string()
        .map_err(|_| CliError::Usage("the current directory is not valid UTF-8".into()))
}

/// `work add` flags -> the signed `AddArgs`. work.py defaults apply (kind other, size/value/weight 1); the cwd
/// defaults to the current directory; the command is the argv after `--`, never a shell line.
fn add_args(a: WorkAddArgs) -> Result<AddArgs, CliError> {
    if let Some(id) = &a.id {
        check_id("task", id)?;
    }
    if let Some(kind) = &a.kind {
        validate_kind(kind).map_err(CliError::Usage)?;
    }
    check_scope(a.scope.as_deref())?;
    for after in &a.after {
        check_id("--after", after)?;
    }
    check_positive("--size", a.size)?;
    check_positive("--value", a.value)?;
    check_positive("--weight", a.weight)?;
    if !a.command.is_empty() {
        check_command(&a.command).map_err(CliError::Usage)?;
    }
    let defaults = AddArgs::default();
    let cwd = match a.cwd {
        Some(cwd) => cwd,
        None => current_dir_string()?,
    };
    Ok(AddArgs {
        id: a.id,
        title: a.title.unwrap_or_default(),
        kind: a.kind.unwrap_or(defaults.kind),
        scope: scope_arg(a.scope),
        after: a.after,
        size: a.size.unwrap_or(defaults.size),
        value: a.value.unwrap_or(defaults.value),
        files: a.files,
        weight: a.weight.unwrap_or(defaults.weight),
        command: (!a.command.is_empty()).then_some(a.command),
        cwd: Some(cwd),
        card: a.card,
    })
}

/// `schedule add`: the timer id is `--id` and the task template carries no id (tasks get `<timer>-<n>`).
fn timer_args(a: ScheduleAddArgs) -> Result<TimerArgs, CliError> {
    let spec = timer_spec(&a.when)?;
    let mut task = a.task;
    let id = task.id.take();
    if let Some(id) = &id {
        check_id("timer", id)?;
    }
    Ok(TimerArgs {
        id,
        spec,
        template: add_args(task)?,
    })
}

/// Exactly one of `--at` (UTC ms or RFC 3339), `--every` (seconds or Nm/Nh/Nd) and `--cron` (five UTC fields).
fn timer_spec(when: &ScheduleWhen) -> Result<TimerSpec, CliError> {
    let spec = if let Some(at) = &when.at {
        TimerSpec::At {
            at_ms: timer::parse_at(at).map_err(CliError::Usage)?,
        }
    } else if let Some(every) = &when.every {
        timer::parse_spec(&format!("every:{every}")).map_err(CliError::Usage)?
    } else if let Some(cron) = &when.cron {
        timer::parse_spec(&format!("cron:{cron}")).map_err(CliError::Usage)?
    } else {
        return Err(CliError::Usage(
            "give one of --at, --every or --cron".into(),
        ));
    };
    timer::validate(&spec).map_err(CliError::Usage)?;
    Ok(spec)
}

/// `work update` flags -> `UpdateArgs`. `--unassign` is `Some(None)`, `--assign PK` is `Some(Some(PK))`.
fn update_args(a: &WorkUpdateArgs) -> Result<UpdateArgs, CliError> {
    if let Some(kind) = &a.kind {
        validate_kind(kind).map_err(CliError::Usage)?;
    }
    if let Some(after) = &a.after {
        for item in after {
            check_id("--after", item)?;
        }
    }
    check_positive("--size", a.size)?;
    check_positive("--value", a.value)?;
    let agent = if a.unassign {
        Some(None)
    } else {
        a.assign.clone().map(Some)
    };
    Ok(UpdateArgs {
        title: a.title.clone(),
        kind: a.kind.clone(),
        size: a.size,
        value: a.value,
        after: a.after.clone(),
        agent,
    })
}

/// `work settings` with no value flag only reads; otherwise the patch is signed.
fn settings_request(a: &WorkSettingsArgs) -> Option<SettingsPatch> {
    let patch = a.patch();
    (!patch.is_empty()).then_some(patch)
}

/// `accept --digest` is compared the way work.py does: surrounding space and case do not matter.
fn normalize_digest(digest: &str) -> String {
    digest.trim().to_ascii_lowercase()
}

/// `status --json` prints the StatusReport at top level, the work.py shape (`machine`, `running`, `waiting`,
/// `counts`); the client wraps it as `{"report": ...}`, which is unwrapped here.
fn status_out(out: Out) -> Out {
    Out {
        json: status_report(out.json),
        text: out.text,
    }
}

/// The inner report of a `{"report": …}` object (and only that shape); any other value is returned unchanged.
fn status_report(json: serde_json::Value) -> serde_json::Value {
    match json {
        serde_json::Value::Object(mut map) if map.len() == 1 && map.contains_key("report") => {
            map.remove("report").unwrap_or(serde_json::Value::Null)
        }
        other => other,
    }
}

/// `next` with nothing claimable answers `{"claimed": null, "reason": ...}`.
fn claimed_none(out: &Out) -> bool {
    out.json
        .get("claimed")
        .is_some_and(serde_json::Value::is_null)
}

/// Exit code of a failed verb from its `ClientError::code()` and message. Usage errors and an unknown task id
/// (the daemon's `NotFound("task <id>")`) are input errors; every other refusal, and an unreachable daemon, is 1.
fn exit_for(code: &str, message: &str) -> i32 {
    if code == "usage" || (code == "not_found" && message.starts_with("task ")) {
        EXIT_INPUT
    } else {
        EXIT_REFUSED
    }
}

fn exit_for_error(e: &CliError) -> i32 {
    exit_for(&e.code(), &e.to_string())
}

fn report_error(e: &CliError, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({"error": {"code": e.code(), "message": e.to_string()}})
        );
    } else {
        eprintln!("work: {e}");
    }
}

fn print_out(out: &Out, json: bool) {
    if json {
        println!("{}", out.json);
    } else if !out.text.is_empty() {
        println!("{}", out.text);
    }
}

/// The exit status byte: the work semantics' i32 masked to 0..=255.
fn exit_byte(code: i32) -> u8 {
    u8::try_from(code & 0xff).unwrap_or(1)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(exit_byte(code))
}

fn given_tasks_with(args: &[String], stdin: &mut dyn Read) -> CompatOutput {
    let mut stdin_used = false;
    let mut read = |path: &str| read_source(path, &mut stdin_used, &mut *stdin);
    compat::run(args, &mut read)
}

/// One `--tasks` source: a file, or stdin. Stdin is consumed once; a later `-` reads an empty input.
fn read_source(path: &str, stdin_used: &mut bool, stdin: &mut dyn Read) -> Result<String, String> {
    if path == "-" {
        if std::mem::replace(stdin_used, true) {
            return Ok(String::new());
        }
        let mut text = String::new();
        stdin
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))
    }
}

#[cfg(test)]
mod tests;
