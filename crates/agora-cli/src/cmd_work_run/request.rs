//! Argument checks for `agora work run` and `agora work wait`: `--timeout`, `--scope` and the new-job flags, and
//! the mapping of the inline command to a `RunArgs` with its paths resolved.

use crate::WorkRunArgs;
use agora_sched::model::{AddArgs, RunArgs};
use std::path::Path;
use std::time::Duration;

/// The longest `--timeout` honoured; larger values are clamped (about 31 years).
pub(super) const MAX_TIMEOUT: Duration = Duration::from_secs(1_000_000_000);
pub(super) fn env_scope() -> Option<String> {
    std::env::var("WORK_SCOPE").ok()
}

/// `--scope` counts as given only when it differs from `WORK_SCOPE`: clap fills the flag from that variable.
pub(super) fn scope_given(scope: Option<&str>, env_scope: Option<&str>) -> bool {
    scope.is_some_and(|scope| Some(scope) != env_scope)
}

/// `--timeout` in seconds: finite and zero or more. Values beyond `MAX_TIMEOUT` are clamped.
pub(super) fn timeout(seconds: f64) -> Result<Duration, String> {
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
pub(super) fn run_request(
    a: &WorkRunArgs,
    env_scope: Option<&str>,
    base: &Path,
) -> Result<RunArgs, String> {
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
