//! Argument checks for `agora work wait`: maps the one given wait mode to its `WaitSpec`.

use super::request::scope_given;
use crate::WorkWaitArgs;
use agora_sched::model::WaitSpec;
use agora_sched::textfmt::enumerate_ids;

const WAIT_ONE: &str = "wait for one thing: task IDs, --all, --pid, --file, --gone, --log, --card, --channel or --notices";

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
pub(super) fn wait_spec(a: &WorkWaitArgs, env_scope: Option<&str>) -> Result<WaitSpec, String> {
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
