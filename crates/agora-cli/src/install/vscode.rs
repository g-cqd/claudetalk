//! Installer for VS Code agent mode: `servers.agora` in the VS Code `mcp.json`, the shared Copilot
//! hooks file (`copilot_cli::install_shared_hooks`), and the instructions: a block in
//! `.github/copilot-instructions.md` (project scope) or the owned `agora.instructions.md` (user
//! scope). Targets per scope: `ClientPaths::copilot_vscode` (SPEC 4.3).
//! Order: validate and parse every existing file first (any failure -> `Err` before any write), then
//! write MCP, hooks, instructions; each write is one `Action`.

#[cfg(test)]
mod tests;

use super::claude::{Action, action, from_write};
use super::copilot_cli::{
    check_shared_hooks, copilot_hooks_doc, install_shared_hooks, uninstall_shared_hooks,
};
use super::instructions;
use super::json_merge::{self, ServerChange, check_object_member};
use super::markers::{self, MarkerError};
use super::multi::{ClientReport, Ctx};
use super::paths::{InstructionsKind, Targets};
use super::safe_write::{self, WriteResult, read_json};
use super::text_write;
use crate::CliError;
use agora_client::capabilities::{CapabilitiesFile, HarnessCapabilities};
use agora_deliver::Harness;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Mode of a new instructions file (a text file, not a secret).
const INSTRUCTIONS_MODE: u32 = 0o644;
/// The container key of the servers in VS Code's `mcp.json` (VERIFIED-LOCAL, SPEC 2.3).
const MCP_CONTAINER: &str = "servers";
/// VS Code's `mcp.json` may hold an `inputs` array (prompted variables); it is kept as it is.
const INPUTS: &str = "inputs";
/// The server name agora uses in `mcp.json`.
const SERVER: &str = "agora";
/// Expanded by VS Code per workspace; agora ignores an unexpanded `${` (SPEC 2.3).
const AGORA_FOLDER_VALUE: &str = "${workspaceFolder}";
/// Warning when a `servers.agora` entry that was not ours got replaced.
const WARN_FOREIGN_MCP: &str = "warning: an existing servers.agora entry was not ours and was replaced; the backup keeps the old one";
/// VS Code agent hooks are a preview (VERIFIED-LOCAL on VS Code 1.141.0: setting `chat.useHooks`,
/// policy minimum 1.109); MCP tools are available in Agent mode only.
const NOTE_PREVIEW: &str = "VS Code agent hooks are a preview feature (setting chat.useHooks, VS Code >= 1.109); MCP tools are available in Agent mode only";
/// No MCP notification reaches the model and `code chat` opens a window, so VS Code never wakes an
/// idle agent (SPEC 2.3); delivery is the hooks plus the `inbox_wait` floor.
const NOTE_NO_WAKE: &str = "No idle wake in VS Code: delivery is hooks + the inbox_wait floor";

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`, `context_hook`, `stop_hook`, `hooks_trusted`, `owned_session` stays false: it
/// comes from the probe), `scopes = [c.scope_label()]` and `files` = every path written or edited.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let files = vscode_files(c)?;
    let entry = server_entry(c)?;

    // Validate and compute every new text before the first write.
    let mut root = read_mcp(&files.mcp, &entry)?;
    check_object_member(&root, MCP_CONTAINER, &files.mcp)?;
    check_inputs(&root, &files.mcp)?;
    check_shared_hooks(&files.hooks)?;
    copilot_hooks_doc(c)?;
    let instructions_text = instructions_text(&files)?;

    // Write MCP, hooks, instructions.
    let warnings = match json_merge::set_server(&mut root, MCP_CONTAINER, SERVER, entry) {
        ServerChange::ReplacedForeign => vec![WARN_FOREIGN_MCP.to_owned()],
        ServerChange::Added | ServerChange::Updated | ServerChange::Unchanged => Vec::new(),
    };
    let mcp_res = safe_write::write_json(&files.mcp, &root, c.o.dry_run)?;
    let mut actions = vec![from_write(&files.mcp, mcp_res, warnings)];
    actions.push(install_shared_hooks(c, &files.hooks)?);
    let instr_res = text_write::write_text(
        &files.instructions,
        &instructions_text,
        INSTRUCTIONS_MODE,
        c.o.dry_run,
    )?;
    actions.push(from_write(&files.instructions, instr_res, Vec::new()));

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            mcp_configured: true,
            context_hook: true,
            stop_hook: true,
            scopes: vec![c.scope_label()],
            files: scope_files(&files),
            ..HarnessCapabilities::default()
        },
        notes: vec![NOTE_PREVIEW.to_owned(), NOTE_NO_WAKE.to_owned()],
    })
}

/// Remove only what `install` added, in this scope. Shared files (the Copilot hooks file, and the
/// `.github/copilot-instructions.md` block in project scope) are removed only when
/// `!caps.file_shared(path, <this harness>)`; otherwise the action is `kept` with detail "still used
/// by <other client>". The report's `caps.files` is this scope's three paths, whether or not they
/// existed, so the capabilities entry forgets exactly this scope. Idempotent: a second run reports
/// `absent` / `unchanged`.
pub fn uninstall(c: &Ctx, caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let files = vscode_files(c)?;
    let me = Harness::CopilotVsCode;

    // Read everything first: an unreadable file stops the run before any removal. A missing hooks
    // file reads as `{}`, so it is only checked for readability here.
    let mut root = read_json(&files.mcp)?;
    read_json(&files.hooks)?;
    let existing = text_write::read_text(&files.instructions)?;

    let mut actions = Vec::new();
    if json_merge::remove_server(&mut root, MCP_CONTAINER, SERVER) {
        let res = safe_write::write_json(&files.mcp, &root, c.o.dry_run)?;
        actions.push(removal(&files.mcp, res));
    } else {
        actions.push(action(
            &files.mcp,
            "absent",
            "no agora server entry".to_owned(),
        ));
    }
    actions.push(uninstall_shared_hooks(c, caps, &files.hooks, me)?);
    let instructions_action = match files.kind {
        InstructionsKind::Block => {
            block_removal(c, caps, &files.instructions, existing.as_deref())?
        }
        InstructionsKind::OwnedFile => owned_removal(c, &files.instructions, existing.as_deref())?,
    };
    actions.push(instructions_action);

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files: scope_files(&files),
            ..HarnessCapabilities::default()
        },
        notes: Vec::new(),
    })
}

/// The three files of this client in this scope: `mcp.json`, the shared hooks file, instructions.
struct VsCodeFiles {
    mcp: PathBuf,
    hooks: PathBuf,
    instructions: PathBuf,
    kind: InstructionsKind,
}

/// The paths of `files` as the capabilities entry stores them (`caps.files`).
fn scope_files(files: &VsCodeFiles) -> Vec<String> {
    vec![
        files.mcp.display().to_string(),
        files.hooks.display().to_string(),
        files.instructions.display().to_string(),
    ]
}

/// The VS Code paths for `c.o.scope`. Both scopes have all three.
fn vscode_files(c: &Ctx) -> Result<VsCodeFiles, CliError> {
    let Targets {
        mcp,
        hooks,
        instructions,
        instructions_kind,
    } = c.paths.copilot_vscode(c.o.scope);
    match (hooks, instructions) {
        (Some(hooks), Some(instructions)) => Ok(VsCodeFiles {
            mcp,
            hooks,
            instructions,
            kind: instructions_kind,
        }),
        _ => Err(CliError::Usage(
            "internal: copilot-vscode has no hooks or instructions path".to_owned(),
        )),
    }
}

/// The `servers.agora` entry: a stdio server with `AGORA_FOLDER` set to the workspace folder.
fn server_entry(c: &Ctx) -> Result<Value, CliError> {
    let exe = c.exe_str()?;
    let args = c.mcp_args()?;
    Ok(json!({
        "type": "stdio",
        "command": exe,
        "args": args,
        "env": {"AGORA_FOLDER": AGORA_FOLDER_VALUE},
    }))
}

/// Read `mcp.json` for install. A file that does not parse also gets the hint to add `entry` by hand:
/// VS Code allows comments in `mcp.json`, which `read_json` refuses. Every other error is unchanged.
fn read_mcp(path: &Path, entry: &Value) -> Result<Value, CliError> {
    read_json(path).map_err(|e| {
        if is_unparsable_file(path) {
            CliError::Usage(format!(
                "{e}; VS Code mcp.json may contain comments; add this entry by hand under \"{MCP_CONTAINER}\": {entry}"
            ))
        } else {
            e
        }
    })
}

/// True when `path` is a regular file (a symlink is not) whose bytes are not JSON.
fn is_unparsable_file(path: &Path) -> bool {
    let regular = fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    regular && fs::read(path).is_ok_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_err())
}

/// `inputs`, when present, must be an array; it is preserved untouched. `Err` names the file.
fn check_inputs(root: &Value, path: &Path) -> Result<(), CliError> {
    match root.get(INPUTS) {
        Some(value) if !value.is_array() => Err(CliError::Usage(format!(
            "{}: \"{INPUTS}\" is not a JSON array; fix it, nothing was written",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// The instructions text to write, validated before any write. Project scope: the existing file
/// with its agora block upserted. User scope: the owned file; an existing file without an agora
/// block is refused.
fn instructions_text(files: &VsCodeFiles) -> Result<String, CliError> {
    let existing = text_write::read_text(&files.instructions)?;
    match files.kind {
        InstructionsKind::Block => {
            let existing = existing.unwrap_or_default();
            markers::upsert_block(&existing, instructions::snippet())
                .map_err(|e| marker_error(&files.instructions, &e))
        }
        InstructionsKind::OwnedFile => match existing {
            Some(text) if !markers::has_block(&text) => Err(CliError::Usage(format!(
                "{} exists and is not managed by agora; nothing was written",
                files.instructions.display()
            ))),
            _ => Ok(instructions::vscode_user_instructions()),
        },
    }
}

/// The action for the agora block in a shared instructions file (project scope). No block ->
/// `absent`; listed in `caps` for another harness -> `kept`; else the block is stripped and the file
/// written back.
fn block_removal(
    c: &Ctx,
    caps: &CapabilitiesFile,
    path: &Path,
    existing: Option<&str>,
) -> Result<Action, CliError> {
    let stripped = match existing {
        Some(text) => markers::remove_block(text).map_err(|e| marker_error(path, &e))?,
        None => None,
    };
    let Some(text) = stripped else {
        return Ok(action(path, "absent", "no agora block".to_owned()));
    };
    if let Some(other) = shared_with(caps, path, Harness::CopilotVsCode) {
        return Ok(action(path, "kept", format!("still used by {other}")));
    }
    let res = text_write::write_text(path, &text, INSTRUCTIONS_MODE, c.o.dry_run)?;
    Ok(removal(path, res))
}

/// The action for the owned user instructions file: missing -> `absent`; holds an agora block ->
/// removed (`text_write::remove_file`); otherwise a file that is not ours -> `kept`.
fn owned_removal(c: &Ctx, path: &Path, existing: Option<&str>) -> Result<Action, CliError> {
    match existing {
        Some(text) if !markers::has_block(text) => {
            Ok(action(path, "kept", "not managed by agora".to_owned()))
        }
        _ => text_write::remove_file(path, c.o.dry_run),
    }
}

/// The name of another harness whose capabilities entry lists `path`, when `file_shared` says so.
fn shared_with(caps: &CapabilitiesFile, path: &Path, me: Harness) -> Option<&'static str> {
    let key = path.display().to_string();
    if !caps.file_shared(&key, me) {
        return None;
    }
    Harness::ALL
        .into_iter()
        .filter(|h| *h != me)
        .find(|h| {
            caps.harness(*h)
                .is_some_and(|entry| entry.files.contains(&key))
        })
        .map(Harness::as_str)
}

/// The action for a removal write: `wrote` becomes `removed` (the backup path stays in the detail).
fn removal(path: &Path, res: WriteResult) -> Action {
    let mut act = from_write(path, res, Vec::new());
    if act.result == "wrote" {
        act.result = "removed".to_owned();
    }
    act
}

/// A marker error on an instructions file, as a usage error naming the file.
fn marker_error(path: &Path, e: &MarkerError) -> CliError {
    CliError::Usage(format!(
        "{}: {e}; fix it, nothing was written",
        path.display()
    ))
}
