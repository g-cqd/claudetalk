//! Merging one run into `capabilities.json` (SPEC 4.3): the per-client entries, the probe fields,
//! and the single write of the file after every client ran.

use super::Done;
use crate::CliError;
use crate::install::claude::{Action, Options};
use crate::install::client::Client;
use crate::install::probe_clients::{self, ClientProbe};
use crate::install::{backup_path, capabilities_plan, check_capabilities_target};
use agora_client::capabilities::{self, CapabilitiesFile, HarnessCapabilities};
use agora_daemon::Paths;
use std::fs;

/// Copy the probe into the run's capabilities: binary, version, time and owned-session support.
/// For Copilot CLI the two hook flags follow the version (SPEC 2.2, hooks take effect from 1.0.72);
/// `owned_session` stays the probe's own `--acp` detection and does not follow the version.
pub(super) fn apply_probe(
    client: Client,
    caps: &mut HarnessCapabilities,
    probe: &ClientProbe,
    now: i64,
) {
    caps.bin = probe.bin.clone();
    caps.version = probe.version.clone();
    caps.probed_at_ms = now;
    caps.owned_session = probe.owned_session;
    if client == Client::CopilotCli {
        let effective = probe_clients::copilot_hooks_effective(probe.version.as_deref());
        caps.context_hook = effective;
        caps.stop_hook = effective;
    }
}

/// Merge one installed client into its entry (SPEC 4.3): the scope and the written files are
/// added, the flags come from this run, and the probe fields are replaced only when `--probe` ran.
/// `hooks_trusted`: a run reporting `Some(false)` (hooks written, not trusted) keeps an existing
/// `Some(true)` only when it comes from a scope the entry does not have yet, so a later untrusted
/// install of another Codex scope cannot erase the trust. A same-scope reinstall is authoritative:
/// its hook command may have changed (a moved or upgraded binary), which makes the trust stale. Any
/// other reported value replaces the entry's.
pub(super) fn record_entry(file: &mut CapabilitiesFile, d: &Done) {
    let h = d.client.harness();
    let run = &d.report.caps;
    let mut entry = file.harness(h).unwrap_or_default();
    let same_scope = entry.scopes.contains(&d.scope);
    entry.scopes.push(d.scope.clone());
    entry.files.extend(run.files.iter().cloned());
    sort_dedup(&mut entry.scopes);
    sort_dedup(&mut entry.files);
    entry.mcp_configured |= run.mcp_configured;
    if d.probed || run.context_hook {
        entry.context_hook = run.context_hook;
    }
    if d.probed || run.stop_hook {
        entry.stop_hook = run.stop_hook;
    }
    match run.hooks_trusted {
        Some(false) if entry.hooks_trusted == Some(true) && !same_scope => {}
        Some(trusted) => entry.hooks_trusted = Some(trusted),
        None => {}
    }
    if d.probed || run.owned_session {
        entry.owned_session = run.owned_session;
    }
    if d.probed {
        entry.bin = run.bin.clone();
        entry.version = run.version.clone();
        entry.probed_at_ms = run.probed_at_ms;
    }
    entry.notes = d.report.notes.clone();
    file.set_harness(h, Some(entry));
}

/// Remove one uninstalled client's scope and files from its entry; the entry goes when no scope is
/// left. An entry this file does not have is left alone.
pub(super) fn forget_entry(file: &mut CapabilitiesFile, d: &Done) {
    let h = d.client.harness();
    let Some(mut entry) = file.harness(h) else {
        return;
    };
    entry.scopes.retain(|s| *s != d.scope);
    entry.files.retain(|f| !d.report.caps.files.contains(f));
    if entry.scopes.is_empty() {
        file.set_harness(h, None);
    } else {
        file.set_harness(h, Some(entry));
    }
}

fn sort_dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

/// `edit_capabilities` for a finished run: a failed write becomes an `error` action and `false`,
/// not an error, because the clients already ran and their results must still be reported.
pub(super) fn write_capabilities(
    o: &Options,
    now: i64,
    detail: String,
    edit: impl FnOnce(&mut CapabilitiesFile),
) -> (Vec<Action>, bool) {
    match edit_capabilities(o, now, detail, edit) {
        Ok(actions) => (actions, true),
        Err(e) => (
            vec![Action {
                path: capabilities_shown(o),
                result: "error".to_owned(),
                detail: e.to_string(),
            }],
            false,
        ),
    }
}

/// `capabilities.json` of this home, as shown in actions.
fn capabilities_shown(o: &Options) -> String {
    capabilities::path(&Paths::new(&o.agora_home))
        .display()
        .to_string()
}

/// Apply `edit` to `capabilities.json`, loaded here (after every client ran, so no earlier write is
/// lost) and written once, only when `edit` changed something. A file that exists but does not
/// load is moved aside first, as `record_capabilities` does, and only when there is something to
/// write. `detail` is the action detail of the write.
fn edit_capabilities(
    o: &Options,
    now: i64,
    detail: String,
    edit: impl FnOnce(&mut CapabilitiesFile),
) -> Result<Vec<Action>, CliError> {
    let paths = Paths::new(&o.agora_home);
    let target = capabilities::path(&paths);
    let shown = capabilities_shown(o);
    let exists = check_capabilities_target(&paths)?;
    let loaded = capabilities::load(&paths);
    let before = loaded.clone().unwrap_or_default();
    let mut file = before.clone();
    edit(&mut file);
    if file == before {
        return Ok(Vec::new());
    }
    let mut actions = Vec::with_capacity(2);
    if capabilities_plan(exists, loaded.is_some(), false) == "backed_up" {
        let bak = backup_path(&target, now);
        fs::rename(&target, &bak)?;
        actions.push(Action {
            path: shown.clone(),
            result: "backed_up".to_owned(),
            detail: format!(
                "unparsable capabilities.json moved aside to {}",
                bak.display()
            ),
        });
    }
    capabilities::save(&paths, &file)?;
    actions.push(Action {
        path: shown,
        result: "wrote".to_owned(),
        detail,
    });
    Ok(actions)
}
