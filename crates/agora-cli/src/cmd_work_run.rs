//! `agora work run` and `agora work wait`: loop `WorkWait` round trips until the job ends or the overall
//! `--timeout` passes, then print the work.py report. Ctrl-C exits 130 and the job continues.

use crate::{CliError, Ctx, WorkRunArgs, WorkWaitArgs};
use agora_client::api::{work, work_text};
use agora_sched::exit::{EXIT_INPUT, EXIT_INTERRUPTED, EXIT_TIMEOUT, wait_exit};
use agora_sched::model::{AddArgs, RunArgs, TaskRecord, TaskState, WaitResult, WaitSpec};
use agora_sched::pyfmt::printable;
use agora_sched::textfmt::{brief, enumerate_ids};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

const POLL_CAP_MS: u64 = 55_000;
const BRIEF_LIMIT: usize = 200;
/// The longest `--timeout` honoured; larger values are clamped (about 31 years).
const MAX_TIMEOUT: Duration = Duration::from_secs(1_000_000_000);
const WAIT_ONE: &str = "wait for one thing: task IDs, --all, --pid, --file, --gone, --log, --card, --channel or --notices";

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

fn env_scope() -> Option<String> {
    std::env::var("WORK_SCOPE").ok()
}

/// `--scope` counts as given only when it differs from `WORK_SCOPE`: clap fills the flag from that variable.
fn scope_given(scope: Option<&str>, env_scope: Option<&str>) -> bool {
    scope.is_some_and(|scope| Some(scope) != env_scope)
}

/// `--timeout` in seconds: finite and zero or more. Values beyond `MAX_TIMEOUT` are clamped.
fn timeout(seconds: f64) -> Result<Duration, String> {
    if !(seconds.is_finite() && seconds >= 0.0) {
        return Err(format!(
            "--timeout '{seconds}' is not a finite number, zero or more"
        ));
    }
    Ok(Duration::try_from_secs_f64(seconds)
        .unwrap_or(MAX_TIMEOUT)
        .min(MAX_TIMEOUT))
}

/// The new-job flags given with a task ID (`--scope` only when it is not the `WORK_SCOPE` default).
fn new_job_flags(a: &WorkRunArgs, env_scope: Option<&str>) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if a.id.is_some() {
        flags.push("--id");
    }
    if a.weight.is_some() {
        flags.push("--weight");
    }
    if !a.files.is_empty() {
        flags.push("--files");
    }
    if a.cwd.is_some() {
        flags.push("--cwd");
    }
    if a.kind.is_some() {
        flags.push("--kind");
    }
    if a.title.is_some() {
        flags.push("--title");
    }
    if scope_given(a.scope.as_deref(), env_scope) {
        flags.push("--scope");
    }
    if a.card.is_some() {
        flags.push("--card");
    }
    flags
}

/// Validates the `run` arguments (ref-queue-cli section 4) and maps them to a `work.run` request. `base` is the
/// directory relative `--cwd` and `--files` paths are resolved against.
fn run_request(a: &WorkRunArgs, env_scope: Option<&str>, base: &Path) -> Result<RunArgs, String> {
    match (a.task.as_deref(), a.command.is_empty()) {
        (None, true) => Err("name a task you claimed, or give a command after --".to_owned()),
        (Some(_), false) => Err("give a task ID or a command after --, not both".to_owned()),
        (Some(task), true) => {
            let flags = new_job_flags(a, env_scope);
            if flags.is_empty() {
                Ok(RunArgs {
                    task: Some(task.to_owned()),
                    add: None,
                })
            } else {
                Err(format!(
                    "{} describe a new job; give its command after --",
                    flags.join(", ")
                ))
            }
        }
        (None, false) => inline_request(a, base),
    }
}

/// The inline form: a new claimed job typed with `run -- CMD`. Paths are made absolute and canonical when they
/// exist; the daemon checks the rest (scope, weight against capacity, the directory).
fn inline_request(a: &WorkRunArgs, base: &Path) -> Result<RunArgs, String> {
    let weight = a.weight.unwrap_or(1.0);
    if !(weight.is_finite() && weight > 0.0) {
        return Err(format!(
            "--weight '{weight}' is not a finite number above zero"
        ));
    }
    let cwd = resolve(base, a.cwd.as_deref());
    let cwd_path = Path::new(&cwd);
    let files = a
        .files
        .iter()
        .map(|file| resolve(cwd_path, Some(file)))
        .collect();
    Ok(RunArgs {
        task: None,
        add: Some(AddArgs {
            id: a.id.clone(),
            title: a.title.clone().unwrap_or_default(),
            kind: a.kind.clone().unwrap_or_else(|| "other".to_owned()),
            scope: a.scope.clone().filter(|scope| !scope.is_empty()),
            files,
            weight,
            command: Some(a.command.clone()),
            cwd: Some(cwd),
            card: a.card.clone(),
            ..AddArgs::default()
        }),
    })
}

/// `base/path` (or `base` alone), canonical when the path exists, as a string.
fn resolve(base: &Path, path: Option<&str>) -> String {
    let joined = match path {
        Some(path) => base.join(path),
        None => base.to_path_buf(),
    };
    std::fs::canonicalize(&joined)
        .unwrap_or(joined)
        .to_string_lossy()
        .into_owned()
}

/// The `--pid`, `--file`, `--gone`, `--log`, `--card`, `--channel`, `--notices` and task-ID modes that were given.
fn wait_modes(a: &WorkWaitArgs) -> Vec<String> {
    let mut modes = Vec::new();
    if !a.ids.is_empty() {
        modes.push("task IDs".to_owned());
    }
    if a.all {
        modes.push("--all".to_owned());
    }
    if !a.pid.is_empty() {
        modes.push("--pid".to_owned());
    }
    if !a.file.is_empty() {
        modes.push("--file".to_owned());
    }
    if !a.gone.is_empty() {
        modes.push("--gone".to_owned());
    }
    if a.log.is_some() {
        modes.push("--log".to_owned());
    }
    if !a.card.is_empty() {
        modes.push("--card".to_owned());
    }
    if a.channel.is_some() {
        modes.push("--channel".to_owned());
    }
    if a.notices {
        modes.push("--notices".to_owned());
    }
    modes
}

/// Validates the `wait` arguments (ref-queue-cli section 5) and maps the one given mode to its `WaitSpec`. File,
/// gone and log paths go out as given: `api::work::wait` makes them absolute against the current directory.
fn wait_spec(a: &WorkWaitArgs, env_scope: Option<&str>) -> Result<WaitSpec, String> {
    if a.after.is_some() && a.channel.is_none() && !a.notices {
        return Err("--after goes with --channel or --notices".to_owned());
    }
    let modes = wait_modes(a);
    match modes.len() {
        0 => return Err(WAIT_ONE.to_owned()),
        1 => {}
        _ => return Err(format!("{WAIT_ONE}, not {}", enumerate_ids(&modes))),
    }
    if a.any && a.ids.is_empty() {
        return Err("--any goes with task IDs".to_owned());
    }
    if scope_given(a.scope.as_deref(), env_scope) && !a.all {
        return Err("--scope goes with --all".to_owned());
    }
    let scope = a.scope.clone().filter(|scope| !scope.is_empty());
    if !a.ids.is_empty() {
        return Ok(WaitSpec::Tasks {
            ids: a.ids.clone(),
            any: a.any,
        });
    }
    if a.all {
        return Ok(WaitSpec::All { scope });
    }
    if !a.pid.is_empty() {
        if let Some(pid) = a.pid.iter().find(|pid| **pid <= 0) {
            return Err(format!("--pid {pid} is not a process id"));
        }
        return Ok(WaitSpec::Pid {
            pids: a.pid.clone(),
        });
    }
    if !a.file.is_empty() {
        return Ok(WaitSpec::File {
            paths: a.file.clone(),
        });
    }
    if !a.gone.is_empty() {
        return Ok(WaitSpec::Gone {
            paths: a.gone.clone(),
        });
    }
    if let Some([path, pattern]) = a.log.as_deref() {
        return Ok(WaitSpec::Log {
            path: path.clone(),
            pattern: pattern.clone(),
        });
    }
    if !a.card.is_empty() {
        return Ok(WaitSpec::Cards {
            ids: a.card.clone(),
            states: a.state.clone(),
        });
    }
    if let Some(channel) = &a.channel {
        return Ok(WaitSpec::Messages {
            channel: channel.clone(),
            after: a.after.unwrap_or(0),
        });
    }
    if a.notices {
        return Ok(WaitSpec::Notices {
            after: a.after.unwrap_or(-1),
        });
    }
    Err(WAIT_ONE.to_owned())
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

/// Prints the task report (text or JSON) and returns the exit status for the task rows.
fn finish_tasks(result: &WaitResult, any: bool, lines: usize, json: bool) -> Result<i32, CliError> {
    if result.tasks.is_empty() {
        return Err(CliError::UnexpectedReply);
    }
    let timed_out = result.timed_out;
    let tails = tails_for(&result.tasks, timed_out, lines);
    let rows: Vec<(TaskState, Option<i32>, Option<i64>)> = result
        .tasks
        .iter()
        .map(|task| (task.state, task.status, task.ended_ms))
        .collect();
    let status = wait_exit(&rows, timed_out, any);
    if json {
        print_json(&json_document(status, timed_out, &result.tasks, &tails));
    } else {
        for line in work_text::report_text(
            &result.tasks,
            &tails,
            timed_out,
            agora_client::ops::now_ms(),
        ) {
            println!("{line}");
        }
    }
    Ok(status)
}

/// Prints a condition wait (text or the `WaitResult` JSON) and returns its exit status: 124 on timeout, else the
/// status the daemon gave (a file's number, or 0).
fn finish_condition(result: &mut WaitResult, lines: usize, json: bool) -> i32 {
    if result.condition == "log"
        && let Some(path) = result.targets.first()
    {
        result.tail = agora_run::jobdir::tail(Path::new(path), lines);
    }
    result.status = if result.timed_out {
        EXIT_TIMEOUT
    } else {
        result.status
    };
    if json {
        match serde_json::to_value(&*result) {
            Ok(value) => print_json(&value),
            Err(_) => println!("{{\"status\": {}}}", result.status),
        }
    } else {
        for line in work_text::wait_condition_text(result) {
            println!("{line}");
        }
    }
    result.status
}

/// The last `lines` log lines of each task that is final or whose wait timed out; empty for the others.
fn tails_for(tasks: &[TaskRecord], timed_out: bool, lines: usize) -> Vec<Vec<String>> {
    tasks
        .iter()
        .map(|task| {
            if !(timed_out || task.state.is_final()) {
                return Vec::new();
            }
            match task.log.as_deref() {
                Some(log) => agora_run::jobdir::tail(Path::new(log), lines),
                None => Vec::new(),
            }
        })
        .collect()
}

/// `{"status", "timed_out", "tasks": [task + "tail"]}` (ref-queue-cli section 4).
fn json_document(
    status: i32,
    timed_out: bool,
    tasks: &[TaskRecord],
    tails: &[Vec<String>],
) -> Value {
    let rows: Vec<Value> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let mut row = serde_json::to_value(task).unwrap_or_default();
            if let Some(object) = row.as_object_mut() {
                object.insert(
                    "tail".to_owned(),
                    json!(tails.get(index).cloned().unwrap_or_default()),
                );
            }
            row
        })
        .collect();
    json!({"status": status, "timed_out": timed_out, "tasks": rows})
}

fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{value}"),
    }
}

/// `work: waiting for a b`, with ` (the first to end)` for `--any` over several ids.
fn waiting_line(ids: &[String], any: bool) -> String {
    let names: Vec<String> = ids.iter().map(|id| printable(id)).collect();
    let suffix = if any && names.len() > 1 {
        " (the first to end)"
    } else {
        ""
    };
    format!("work: waiting for {}{suffix}", names.join(" "))
}

/// Prints each announcement of a run once: the process start, or the capacity wait before the process exists.
#[derive(Default)]
struct Announcer {
    started: bool,
    waiting: bool,
}

impl Announcer {
    fn observe(&mut self, task: &TaskRecord) -> Option<String> {
        match task.pid {
            Some(pid) if !self.started => {
                self.started = true;
                Some(started_line(task, pid))
            }
            Some(_) => None,
            None if !self.waiting && waits_for_capacity(task) => {
                self.waiting = true;
                Some(waits_line(task))
            }
            None => None,
        }
    }
}

fn waits_for_capacity(task: &TaskRecord) -> bool {
    task.state == TaskState::Claimed && task.requested_ms.is_some() && !task.note.is_empty()
}

fn started_line(task: &TaskRecord, pid: i64) -> String {
    let job = match task.command.as_deref() {
        Some(argv) => format!("{} ({})", printable(&task.id), brief(argv, BRIEF_LIMIT)),
        None => printable(&task.id),
    };
    let log = printable(task.log.as_deref().unwrap_or("-"));
    format!("work: started job {job} as process {pid}; output in {log}")
}

fn waits_line(task: &TaskRecord) -> String {
    format!(
        "work: job {} waits for capacity: {}",
        printable(&task.id),
        printable(&task.note)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const BASE: &str = "/nonexistent-agora-base";

    fn base() -> &'static Path {
        Path::new(BASE)
    }

    fn run_args(task: Option<&str>, command: &[&str]) -> WorkRunArgs {
        WorkRunArgs {
            task: task.map(str::to_owned),
            id: None,
            weight: None,
            files: Vec::new(),
            cwd: None,
            kind: None,
            title: None,
            scope: None,
            card: None,
            timeout: 570.0,
            tail: 5,
            interval: None,
            agent: None,
            command: command.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn wait_args(ids: &[&str]) -> WorkWaitArgs {
        WorkWaitArgs {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            any: false,
            all: false,
            scope: None,
            pid: Vec::new(),
            file: Vec::new(),
            gone: Vec::new(),
            log: None,
            card: Vec::new(),
            state: Vec::new(),
            channel: None,
            after: None,
            notices: false,
            timeout: 570.0,
            tail: 5,
            interval: None,
        }
    }

    fn task(extra: Value) -> TaskRecord {
        let mut base = json!({
            "seq": 1, "id": "job-1", "title": "", "kind": "other", "tier": 1,
            "author": "pk", "author_name": "alice", "state": "running",
            "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0
        });
        if let (Some(object), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        serde_json::from_value(base).expect("a task record")
    }

    #[test]
    fn run_needs_a_task_or_a_command() {
        let err = run_request(&run_args(None, &[]), None, base()).expect_err("refused");
        assert_eq!(err, "name a task you claimed, or give a command after --");
    }

    #[test]
    fn run_refuses_a_task_and_a_command_together() {
        let err =
            run_request(&run_args(Some("job-1"), &["true"]), None, base()).expect_err("refused");
        assert_eq!(err, "give a task ID or a command after --, not both");
    }

    #[test]
    fn run_with_a_task_refuses_new_job_flags_in_order() {
        let mut a = run_args(Some("job-1"), &[]);
        a.weight = Some(2.0);
        a.kind = Some("bug".to_owned());
        a.title = Some("t".to_owned());
        let err = run_request(&a, None, base()).expect_err("refused");
        assert_eq!(
            err,
            "--weight, --kind, --title describe a new job; give its command after --"
        );
    }

    #[test]
    fn run_with_a_task_takes_scope_from_the_environment_as_the_default() {
        let mut a = run_args(Some("job-1"), &[]);
        a.scope = Some("proj".to_owned());
        assert!(run_request(&a, Some("proj"), base()).is_ok());
        let err = run_request(&a, Some("other"), base()).expect_err("refused");
        assert_eq!(err, "--scope describe a new job; give its command after --");
    }

    #[test]
    fn run_with_a_task_allows_the_wait_flags() {
        let mut a = run_args(Some("job-1"), &[]);
        a.timeout = 5.0;
        a.tail = 9;
        a.interval = Some(0.5);
        a.agent = Some("ignored".to_owned());
        let request = run_request(&a, None, base()).expect("accepted");
        assert_eq!(request.task.as_deref(), Some("job-1"));
        assert!(request.add.is_none());
    }

    #[test]
    fn run_inline_fills_the_defaults() {
        let request =
            run_request(&run_args(None, &["make", "test"]), None, base()).expect("accepted");
        assert!(request.task.is_none());
        let add = request.add.expect("an inline job");
        assert_eq!(add.kind, "other");
        assert_eq!(add.title, "");
        assert_eq!(add.weight, 1.0);
        assert_eq!(add.size, 1.0);
        assert_eq!(add.value, 1.0);
        assert_eq!(
            add.command,
            Some(vec!["make".to_owned(), "test".to_owned()])
        );
        assert_eq!(add.cwd.as_deref(), Some(BASE));
        assert!(add.scope.is_none());
    }

    #[test]
    fn run_inline_resolves_a_relative_cwd_and_files_against_the_base() {
        let mut a = run_args(None, &["true"]);
        a.cwd = Some("sub".to_owned());
        a.files = vec!["src/app".to_owned()];
        let add = run_request(&a, None, base())
            .expect("accepted")
            .add
            .expect("inline");
        assert_eq!(add.cwd.as_deref(), Some("/nonexistent-agora-base/sub"));
        assert_eq!(
            add.files,
            vec!["/nonexistent-agora-base/sub/src/app".to_owned()]
        );
    }

    #[test]
    fn run_inline_refuses_a_weight_that_is_not_a_positive_number() {
        for weight in [f64::NAN, 0.0, -1.0, f64::INFINITY] {
            let mut a = run_args(None, &["true"]);
            a.weight = Some(weight);
            let err = run_request(&a, None, base()).expect_err("refused");
            assert!(err.contains("is not a finite number above zero"), "{err}");
        }
    }

    #[test]
    fn timeout_accepts_zero_and_refuses_negative_or_nan() {
        assert_eq!(timeout(0.0), Ok(Duration::ZERO));
        assert_eq!(timeout(0.5), Ok(Duration::from_millis(500)));
        assert!(timeout(-1.0).is_err());
        assert!(timeout(f64::NAN).is_err());
        assert!(timeout(f64::INFINITY).is_err());
        assert_eq!(timeout(1e300), Ok(MAX_TIMEOUT));
    }

    #[test]
    fn wait_with_no_mode_names_the_choices() {
        let err = wait_spec(&wait_args(&[]), None).expect_err("refused");
        assert_eq!(
            err,
            "wait for one thing: task IDs, --all, --pid, --file, --gone, --log, --card, --channel or --notices"
        );
    }

    #[test]
    fn wait_with_two_modes_names_both() {
        let mut a = wait_args(&["job-1"]);
        a.pid = vec![42];
        let err = wait_spec(&a, None).expect_err("refused");
        assert!(err.ends_with(", not task IDs and --pid"), "{err}");
    }

    #[test]
    fn wait_any_needs_task_ids() {
        let mut a = wait_args(&[]);
        a.any = true;
        a.file = vec!["out.txt".to_owned()];
        let err = wait_spec(&a, None).expect_err("refused");
        assert_eq!(err, "--any goes with task IDs");
    }

    #[test]
    fn wait_scope_needs_all_unless_it_is_the_environment_default() {
        let mut a = wait_args(&["job-1"]);
        a.scope = Some("proj".to_owned());
        assert_eq!(
            wait_spec(&a, None).expect_err("refused"),
            "--scope goes with --all"
        );
        assert!(wait_spec(&a, Some("proj")).is_ok());
    }

    #[test]
    fn wait_ids_map_to_tasks_with_any() {
        let mut a = wait_args(&["a", "b"]);
        a.any = true;
        assert_eq!(
            wait_spec(&a, None),
            Ok(WaitSpec::Tasks {
                ids: vec!["a".to_owned(), "b".to_owned()],
                any: true,
            })
        );
    }

    #[test]
    fn wait_all_maps_an_empty_scope_to_every_scope() {
        let mut a = wait_args(&[]);
        a.all = true;
        a.scope = Some(String::new());
        assert_eq!(wait_spec(&a, None), Ok(WaitSpec::All { scope: None }));
        a.scope = Some("proj".to_owned());
        assert_eq!(
            wait_spec(&a, None),
            Ok(WaitSpec::All {
                scope: Some("proj".to_owned())
            })
        );
    }

    #[test]
    fn wait_pid_refuses_a_non_positive_process_id() {
        let mut a = wait_args(&[]);
        a.pid = vec![7, 0];
        assert_eq!(
            wait_spec(&a, None).expect_err("refused"),
            "--pid 0 is not a process id"
        );
    }

    /// The CLI maps relative paths unchanged; the client absolutises them before the request.
    #[test]
    fn wait_file_gone_and_log_map_their_targets() {
        let mut file = wait_args(&[]);
        file.file = vec!["done.txt".to_owned()];
        assert_eq!(
            wait_spec(&file, None),
            Ok(WaitSpec::File {
                paths: vec!["done.txt".to_owned()]
            })
        );

        let mut gone = wait_args(&[]);
        gone.gone = vec!["P1.lock".to_owned()];
        assert_eq!(
            wait_spec(&gone, None),
            Ok(WaitSpec::Gone {
                paths: vec!["P1.lock".to_owned()]
            })
        );

        let mut log = wait_args(&[]);
        log.log = Some(vec!["build.log".to_owned(), "Build complete".to_owned()]);
        assert_eq!(
            wait_spec(&log, None),
            Ok(WaitSpec::Log {
                path: "build.log".to_owned(),
                pattern: "Build complete".to_owned(),
            })
        );
    }

    #[test]
    fn wait_card_channel_and_notices_map_their_targets() {
        let mut card = wait_args(&[]);
        card.card = vec!["c1".to_owned()];
        card.state = vec!["review".to_owned()];
        assert_eq!(
            wait_spec(&card, None),
            Ok(WaitSpec::Cards {
                ids: vec!["c1".to_owned()],
                states: vec!["review".to_owned()],
            })
        );

        let mut channel = wait_args(&[]);
        channel.channel = Some("general".to_owned());
        channel.after = Some(12);
        assert_eq!(
            wait_spec(&channel, None),
            Ok(WaitSpec::Messages {
                channel: "general".to_owned(),
                after: 12,
            })
        );

        let mut notices = wait_args(&[]);
        notices.notices = true;
        assert_eq!(
            wait_spec(&notices, None),
            Ok(WaitSpec::Notices { after: -1 })
        );
    }

    #[derive(Parser, Debug)]
    struct WaitWrap {
        #[command(flatten)]
        a: WorkWaitArgs,
    }

    /// Parses the `wait` flags the way the binary does (`WORK_SCOPE` included) and maps them to a spec.
    fn parsed_spec(argv: &[&str]) -> Result<WaitSpec, String> {
        let w = WaitWrap::try_parse_from(std::iter::once("x").chain(argv.iter().copied()))
            .expect("parse");
        wait_spec(&w.a, env_scope().as_deref())
    }

    #[test]
    fn wait_notices_takes_after_as_the_sequence_number() {
        assert_eq!(
            parsed_spec(&["--notices", "--after", "5"]),
            Ok(WaitSpec::Notices { after: 5 })
        );
    }

    #[test]
    fn wait_after_without_channel_or_notices_is_refused() {
        assert_eq!(
            parsed_spec(&["--after", "5"]).expect_err("refused"),
            "--after goes with --channel or --notices"
        );
        assert_eq!(
            parsed_spec(&["--after", "5", "--pid", "7"]).expect_err("refused"),
            "--after goes with --channel or --notices"
        );
    }

    #[test]
    fn announcer_reports_the_start_once_when_the_pid_appears() {
        let mut announcer = Announcer::default();
        let running = task(json!({
            "state": "running", "pid": 4242, "log": "/logs/job-1.log",
            "command": ["touch", "/tmp/x"]
        }));
        assert_eq!(
            announcer.observe(&running).as_deref(),
            Some(
                "work: started job job-1 (touch /tmp/x) as process 4242; output in /logs/job-1.log"
            )
        );
        assert_eq!(announcer.observe(&running), None);
    }

    #[test]
    fn announcer_reports_the_capacity_wait_once_before_the_pid() {
        let mut announcer = Announcer::default();
        let waiting = task(json!({
            "state": "claimed", "requested_ms": 1, "note": "predicted 1 of 1 cores are taken",
            "command": ["true"]
        }));
        assert_eq!(
            announcer.observe(&waiting).as_deref(),
            Some("work: job job-1 waits for capacity: predicted 1 of 1 cores are taken")
        );
        assert_eq!(announcer.observe(&waiting), None);
        let running = task(json!({"state": "running", "pid": 9, "log": "/l"}));
        assert!(
            announcer
                .observe(&running)
                .is_some_and(|line| line.starts_with("work: started job"))
        );
    }

    #[test]
    fn announcer_says_nothing_for_a_claim_without_a_request_or_note() {
        let mut announcer = Announcer::default();
        assert_eq!(announcer.observe(&task(json!({"state": "claimed"}))), None);
        assert_eq!(
            announcer.observe(&task(
                json!({"state": "claimed", "requested_ms": 1, "note": ""})
            )),
            None
        );
    }

    #[test]
    fn waiting_line_lists_the_ids_and_the_any_suffix() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(waiting_line(&ids, false), "work: waiting for a b");
        assert_eq!(
            waiting_line(&ids, true),
            "work: waiting for a b (the first to end)"
        );
        assert_eq!(
            waiting_line(&["solo".to_owned()], true),
            "work: waiting for solo"
        );
    }

    #[test]
    fn json_document_has_the_status_flags_and_a_tail_per_task() {
        let tasks = vec![task(json!({"state": "failed", "status": 3, "id": "job-9"}))];
        let tails = vec![vec!["last line".to_owned()]];
        let doc = json_document(3, false, &tasks, &tails);
        assert_eq!(doc["status"], json!(3));
        assert_eq!(doc["timed_out"], json!(false));
        assert_eq!(doc["tasks"][0]["id"], json!("job-9"));
        assert_eq!(doc["tasks"][0]["status"], json!(3));
        assert_eq!(doc["tasks"][0]["tail"], json!(["last line"]));
    }

    #[test]
    fn json_document_gives_an_empty_tail_when_none_was_read() {
        let tasks = vec![task(json!({"state": "running"}))];
        let doc = json_document(124, true, &tasks, &[]);
        assert_eq!(doc["timed_out"], json!(true));
        assert_eq!(doc["tasks"][0]["tail"], json!([]));
    }

    #[test]
    fn tails_are_read_only_for_final_or_timed_out_tasks() {
        let running = task(json!({"state": "running", "log": "/nonexistent-agora-log"}));
        let done = task(json!({"state": "done", "log": "/nonexistent-agora-log"}));
        let none = task(json!({"state": "done"}));
        assert_eq!(
            tails_for(&[running.clone(), done, none], false, 5),
            vec![Vec::<String>::new(), Vec::new(), Vec::new()]
        );
        assert_eq!(tails_for(&[running], true, 5), vec![Vec::<String>::new()]);
    }
}
