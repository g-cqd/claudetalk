//! Installer for opencode: the `mcp.agora` entry in `opencode.json` and the AGENTS.md block. opencode
//! has no command hooks (SPEC 2.5), so no hook file is written and delivery is `agora run-agent
//! opencode` or the `inbox_wait` floor. Targets per scope: `ClientPaths::opencode` (SPEC 4.3).
//! Order: validate and compute every new text in memory first (any failure -> `Err` before any
//! write), then write the MCP file, then the AGENTS.md block; each write is one `Action`.
//! DOCS-ONLY (SPEC 2.5): the shapes follow opencode.ai/docs/{config,mcp-servers,rules}; not run
//! against the binary on the build machine.

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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The top-level object of `opencode.json` that holds MCP servers, and the server name.
const MCP_CONTAINER: &str = "mcp";
const SERVER: &str = "agora";
/// `$schema` written only when this install creates `opencode.json`.
const SCHEMA: &str = "https://opencode.ai/config.json";
/// The JSONC sibling we refuse to edit when `opencode.json` is absent.
const JSONC: &str = "opencode.jsonc";
/// Mode of AGENTS.md: a shared, readable instructions file.
const INSTRUCTIONS_MODE: u32 = 0o644;
/// Note on every install: opencode has no command hooks.
const NO_HOOKS_NOTE: &str = "opencode has no command hooks; delivery = `agora run-agent opencode` (owned session, DOCS-ONLY) or the inbox_wait floor";
/// Detail line when an existing `mcp.agora` entry that was not ours got replaced.
const MCP_REPLACED: &str = "replaced an existing mcp.agora entry that was not ours (backup kept)";

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`; `context_hook`, `stop_hook` and `hooks_trusted` do not apply to opencode;
/// `owned_session` stays false: it comes from the probe), `scopes = [c.scope_label()]` and `files` =
/// the MCP file and the AGENTS.md file.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let (mcp_path, instr_path) = scope_files(c)?;
    let dry_run = c.o.dry_run;
    let entry = mcp_entry(c)?;

    // 1. Validate and compute every new text in memory. Nothing is written before all checks pass.
    let mcp_existed = present(&mcp_path)?;
    let jsonc_path = mcp_path.with_file_name(JSONC);
    if !mcp_existed && present(&jsonc_path)? {
        return Err(CliError::Usage(format!(
            "{} is not edited automatically; add this entry by hand under \"mcp\": {entry}; nothing was written",
            jsonc_path.display()
        )));
    }
    let mut root = load_root(
        &mcp_path,
        &format!("add this entry by hand under \"mcp\": {entry}"),
    )?;
    json_merge::check_object_member(&root, MCP_CONTAINER, &mcp_path)?;
    let change = json_merge::set_server(&mut root, MCP_CONTAINER, SERVER, entry);
    if !mcp_existed && let Some(obj) = root.as_object_mut() {
        obj.insert("$schema".to_owned(), json!(SCHEMA));
    }

    let instr_text = text_write::read_text(&instr_path)?.unwrap_or_default();
    let instr_new = markers::upsert_block(&instr_text, instructions::snippet())
        .map_err(|e| invalid(&instr_path, e))?;

    // 2. Writes: the MCP entry, then the AGENTS.md block.
    let mut actions = Vec::new();
    let mcp_res = safe_write::write_json(&mcp_path, &root, dry_run)?;
    actions.push(claude::from_write(
        &mcp_path,
        mcp_res,
        replaced_line(change),
    ));
    let instr_res = text_write::write_text(&instr_path, &instr_new, INSTRUCTIONS_MODE, dry_run)?;
    actions.push(claude::from_write(&instr_path, instr_res, Vec::new()));

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            scopes: vec![c.scope_label()],
            mcp_configured: true,
            files: vec![path_str(&mcp_path), path_str(&instr_path)],
            ..Default::default()
        },
        notes: vec![NO_HOOKS_NOTE.to_owned()],
    })
}

/// Remove only what `install` added, in this scope. The AGENTS.md block is removed only when
/// `!caps.file_shared(path, Harness::OpenCode)`; otherwise the action is `kept` with detail "still
/// used by <other client>". The `$schema` of a created `opencode.json` stays (the file is never
/// deleted). Idempotent: a second run reports `absent` for both files. The report's `caps.files` are
/// this scope's MCP and AGENTS.md paths, so the capabilities record forgets exactly these.
pub fn uninstall(c: &Ctx, caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let (mcp_path, instr_path) = scope_files(c)?;
    let dry_run = c.o.dry_run;
    let instr_str = path_str(&instr_path);
    let shared = caps.file_shared(&instr_str, Harness::OpenCode);

    // 1. Validate and compute every new text in memory, before the first write.
    let mut root = load_root(&mcp_path, "remove \"mcp\".\"agora\" by hand")?;
    json_merge::check_object_member(&root, MCP_CONTAINER, &mcp_path)?;
    let mcp_removed = json_merge::remove_server(&mut root, MCP_CONTAINER, SERVER);
    let instr_text = text_write::read_text(&instr_path)?;
    let instr_new = match &instr_text {
        Some(text) => markers::remove_block(text).map_err(|e| invalid(&instr_path, e))?,
        None => None,
    };

    // 2. Writes: the MCP entry, then the AGENTS.md block.
    let mut actions = Vec::new();
    actions.push(if mcp_removed {
        let res = safe_write::write_json(&mcp_path, &root, dry_run)?;
        claude::from_write(&mcp_path, res, vec!["removed mcp.agora".to_owned()])
    } else {
        nothing_to_remove(&mcp_path, "no agora entry")
    });
    actions.push(match (instr_text.is_some(), instr_new) {
        (false, _) | (true, None) => nothing_to_remove(&instr_path, "no agora block"),
        (true, Some(_)) if shared => claude::action(
            &instr_path,
            "kept",
            format!("still used by {}", other_user(caps, &instr_str)),
        ),
        (true, Some(text)) => {
            let res = text_write::write_text(&instr_path, &text, INSTRUCTIONS_MODE, dry_run)?;
            claude::from_write(&instr_path, res, vec!["removed agora block".to_owned()])
        }
    });

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files: vec![path_str(&mcp_path), instr_str],
            ..Default::default()
        },
        notes: Vec::new(),
    })
}

/// The opencode files of this scope: `(opencode.json, AGENTS.md)`.
fn scope_files(c: &Ctx) -> Result<(PathBuf, PathBuf), CliError> {
    let targets = c.paths.opencode(c.o.scope);
    let instructions = targets
        .instructions
        .ok_or_else(|| CliError::Usage("opencode has no instructions file".into()))?;
    Ok((targets.mcp, instructions))
}

/// The `mcp.agora` entry: `{"type":"local","command":[<exe>, "--home", <home>, "mcp"],"enabled":true}`.
fn mcp_entry(c: &Ctx) -> Result<Value, CliError> {
    let mut command = vec![c.exe_str()?.to_owned()];
    command.extend(c.mcp_args()?);
    Ok(json!({"type": "local", "command": command, "enabled": true}))
}

/// `opencode.json` as a JSON object (`{}` when missing). A file that does not parse (JSONC comments
/// included) -> `Err` carrying the parse error and `hint`, the by-hand edit for the user.
fn load_root(path: &Path, hint: &str) -> Result<Value, CliError> {
    safe_write::read_json(path).map_err(|e| {
        CliError::Usage(format!(
            "{e}\nopencode.json is not edited automatically; {hint}"
        ))
    })
}

/// True when something (a file, a dangling symlink included) is at `path`; symlinks are not followed.
fn present(path: &Path) -> Result<bool, CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The detail line for a replaced foreign entry; empty for every other change.
fn replaced_line(change: ServerChange) -> Vec<String> {
    if change == ServerChange::ReplacedForeign {
        vec![MCP_REPLACED.to_owned()]
    } else {
        Vec::new()
    }
}

/// `absent` when nothing of ours is in the file (or there is no file); `detail` names what was looked for.
fn nothing_to_remove(path: &Path, detail: &str) -> Action {
    claude::action(path, "absent", detail.to_owned())
}

/// The first other harness whose capabilities entry lists `path` (for the `kept` detail).
fn other_user(caps: &CapabilitiesFile, path: &str) -> &'static str {
    Harness::ALL
        .into_iter()
        .filter(|h| *h != Harness::OpenCode)
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

#[cfg(test)]
mod tests;
