//! Installer for Antigravity (`agy`): the `mcp_config.json` MCP entry, the `hooks.json` named hook
//! `agora`, and the AGENTS.md block. Targets per scope: `ClientPaths::agy` (SPEC 4.3); hook commands
//! via `Ctx::hook_cmd`. Order: validate and compute every new file in memory first (any failure ->
//! `Err` before any write), then write MCP, hooks, instructions; each write is one `Action`.
//! DOCS-ONLY (SPEC 2.4): agy is not installed on the build machine, so the shapes written here follow
//! antigravity.google/docs/{mcp,hooks,rules} and were not run against the binary.

use super::claude::{self, Action};
use super::instructions;
use super::json_merge::{self, ServerChange};
use super::markers;
use super::multi::{ClientReport, Ctx};
use super::safe_write;
use super::text_write;
use crate::CliError;
use agora_client::capabilities::{CapabilitiesFile, HarnessCapabilities};
use agora_deliver::Harness;
use serde_json::{Value, json};
use std::fmt::Display;
use std::path::{Path, PathBuf};

/// The key under `mcpServers` and the named hook in `hooks.json`; also the owner name.
const SERVER: &str = "agora";
/// The `mcp_config.json` container object holding MCP servers.
const MCP_CONTAINER: &str = "mcpServers";
/// Hook handler timeout in seconds (SPEC 4.3).
const HOOK_TIMEOUT_S: u64 = 10;
/// Mode of AGENTS.md: a shared, readable instructions file.
const INSTRUCTIONS_MODE: u32 = 0o644;
/// Note on every agy install: the hook shapes are documentation-derived only.
const DOCS_ONLY_NOTE: &str = "Antigravity support is DOCS-ONLY (agy not installed on the build machine): hooks inject via PreInvocation injectSteps and continue via Stop decision:continue";
/// Detail line when an existing `mcpServers.agora` entry that was not ours got replaced.
const MCP_REPLACED: &str = "replaced an existing mcpServers.agora entry (backup kept)";
/// Detail line when an existing `hooks.json` `agora` entry that was not ours got replaced.
const HOOKS_REPLACED: &str = "replaced an existing hooks.json agora entry (backup kept)";

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`, `context_hook`, `stop_hook`; `hooks_trusted` is not applicable to agy;
/// `owned_session` stays false: it comes from the probe), `scopes = [c.scope_label()]` and `files` =
/// every path written or edited.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let (mcp_path, hooks_path, instr_path) = scope_files(c)?;
    let dry_run = c.o.dry_run;

    // 1. Validate and compute every new text in memory. Nothing is written before all checks pass.
    let mut mcp_doc = safe_write::read_json(&mcp_path)?;
    json_merge::check_object_member(&mcp_doc, MCP_CONTAINER, &mcp_path)?;
    let mcp_entry = json!({"command": c.exe_str()?, "args": c.mcp_args()?});
    let mcp_change = json_merge::set_server(&mut mcp_doc, MCP_CONTAINER, SERVER, mcp_entry);

    // A foreign `agora` hook is replaced, with a warning; the backup keeps the old file.
    let mut hooks_doc = safe_write::read_json(&hooks_path)?;
    let hooks_change = json_merge::set_named(&mut hooks_doc, SERVER, hook_def(c)?);

    let instr_text = text_write::read_text(&instr_path)?.unwrap_or_default();
    let instr_new = markers::upsert_block(&instr_text, instructions::snippet())
        .map_err(|e| invalid(&instr_path, e))?;

    // 2. Writes: MCP, hooks, then the AGENTS.md block.
    let mut actions = Vec::new();
    let mcp_lines = replaced_line(mcp_change, MCP_REPLACED);
    let mcp_res = safe_write::write_json(&mcp_path, &mcp_doc, dry_run)?;
    actions.push(claude::from_write(&mcp_path, mcp_res, mcp_lines));

    let hooks_lines = replaced_line(hooks_change, HOOKS_REPLACED);
    let hooks_res = safe_write::write_json(&hooks_path, &hooks_doc, dry_run)?;
    actions.push(claude::from_write(&hooks_path, hooks_res, hooks_lines));

    let instr_res = text_write::write_text(&instr_path, &instr_new, INSTRUCTIONS_MODE, dry_run)?;
    actions.push(claude::from_write(&instr_path, instr_res, Vec::new()));

    let caps = HarnessCapabilities {
        scopes: vec![c.scope_label()],
        mcp_configured: true,
        context_hook: true,
        stop_hook: true,
        hooks_trusted: None,
        owned_session: false,
        files: vec![
            path_str(&mcp_path),
            path_str(&hooks_path),
            path_str(&instr_path),
        ],
        ..Default::default()
    };
    Ok(ClientReport {
        actions,
        caps,
        notes: vec![DOCS_ONLY_NOTE.to_owned()],
    })
}

/// Remove only what `install` added, in this scope. The AGENTS.md block is removed only when
/// `!caps.file_shared(path, Harness::Antigravity)`; otherwise the action is `kept` with detail
/// "still used by <other client>". Files that hold nothing else are kept as `{}` (never deleted).
/// Idempotent: a second run reports `unchanged` / `absent`.
pub fn uninstall(c: &Ctx, caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let (mcp_path, hooks_path, instr_path) = scope_files(c)?;
    let dry_run = c.o.dry_run;
    let instr_str = path_str(&instr_path);
    let shared = caps.file_shared(&instr_str, Harness::Antigravity);

    // 1. Validate and compute every new text in memory, before the first write.
    let mcp_exists = exists(&mcp_path);
    let mut mcp_doc = safe_write::read_json(&mcp_path)?;
    json_merge::check_object_member(&mcp_doc, MCP_CONTAINER, &mcp_path)?;
    let mcp_removed = json_merge::remove_server(&mut mcp_doc, MCP_CONTAINER, SERVER);

    let hooks_exists = exists(&hooks_path);
    let mut hooks_doc = safe_write::read_json(&hooks_path)?;
    let hooks_removed = json_merge::remove_named(&mut hooks_doc, SERVER);

    let instr_text = text_write::read_text(&instr_path)?;
    let instr_new = match &instr_text {
        Some(text) if !shared => {
            markers::remove_block(text).map_err(|e| invalid(&instr_path, e))?
        }
        _ => None,
    };

    // 2. Writes: MCP, hooks, then the AGENTS.md block.
    let mut actions = Vec::new();
    actions.push(if mcp_removed {
        let res = safe_write::write_json(&mcp_path, &mcp_doc, dry_run)?;
        claude::from_write(&mcp_path, res, vec!["removed mcpServers.agora".to_owned()])
    } else {
        nothing_to_remove(&mcp_path, mcp_exists, "no agora entry")
    });

    actions.push(if hooks_removed {
        let res = safe_write::write_json(&hooks_path, &hooks_doc, dry_run)?;
        claude::from_write(
            &hooks_path,
            res,
            vec!["removed hooks.json agora".to_owned()],
        )
    } else {
        nothing_to_remove(&hooks_path, hooks_exists, "no agora hook")
    });

    actions.push(if instr_text.is_none() {
        claude::action(&instr_path, "absent", String::new())
    } else if shared {
        let other = other_user(caps, &instr_str);
        claude::action(&instr_path, "kept", format!("still used by {other}"))
    } else {
        match instr_new {
            Some(text) => {
                let res = text_write::write_text(&instr_path, &text, INSTRUCTIONS_MODE, dry_run)?;
                claude::from_write(&instr_path, res, Vec::new())
            }
            None => nothing_to_remove(&instr_path, true, "no agora block"),
        }
    });

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files: vec![path_str(&mcp_path), path_str(&hooks_path), instr_str],
            ..Default::default()
        },
        notes: Vec::new(),
    })
}

/// The agy files of this scope: `(mcp_config.json, hooks.json, AGENTS.md)`.
fn scope_files(c: &Ctx) -> Result<(PathBuf, PathBuf, PathBuf), CliError> {
    let targets = c.paths.agy(c.o.scope);
    let hooks = targets
        .hooks
        .ok_or_else(|| CliError::Usage("agy has no hooks file".into()))?;
    let instructions = targets
        .instructions
        .ok_or_else(|| CliError::Usage("agy has no instructions file".into()))?;
    Ok((targets.mcp, hooks, instructions))
}

/// The named `agora` hook definition: `PreInvocation` injects context, `Stop` continues the loop.
/// The event is passed on the command line because agy's payload does not reliably carry it.
fn hook_def(c: &Ctx) -> Result<Value, CliError> {
    let context = c.hook_cmd("context", "agy", Some("PreInvocation"))?;
    let stop = c.hook_cmd("stop", "agy", Some("Stop"))?;
    Ok(json!({
        "enabled": true,
        "PreInvocation": [{"type": "command", "command": context, "timeout": HOOK_TIMEOUT_S}],
        "Stop": [{"type": "command", "command": stop, "timeout": HOOK_TIMEOUT_S}],
    }))
}

/// The detail line for a replaced foreign entry; empty for every other change.
fn replaced_line(change: ServerChange, text: &str) -> Vec<String> {
    if change == ServerChange::ReplacedForeign {
        vec![text.to_owned()]
    } else {
        Vec::new()
    }
}

/// `unchanged` (with `nothing`) when the file exists but holds nothing of ours, `absent` when it
/// does not exist.
fn nothing_to_remove(path: &Path, existed: bool, nothing: &str) -> Action {
    if existed {
        claude::action(path, "unchanged", nothing.to_owned())
    } else {
        claude::action(path, "absent", String::new())
    }
}

/// The first other harness whose capabilities entry lists `path` (for the `kept` detail).
fn other_user(caps: &CapabilitiesFile, path: &str) -> &'static str {
    Harness::ALL
        .into_iter()
        .filter(|h| *h != Harness::Antigravity)
        .find(|h| {
            caps.harness(*h)
                .is_some_and(|hc| hc.files.iter().any(|f| f == path))
        })
        .map_or("another client", Harness::as_str)
}

/// A refusal names the file and says nothing was written.
fn invalid(path: &Path, what: impl Display) -> CliError {
    CliError::Usage(format!("{}: {what}; nothing was written", path.display()))
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

/// True when something (file, dangling or not) is at `path`; symlinks are not followed.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

#[cfg(test)]
mod tests;
