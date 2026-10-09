//! Installer for Copilot CLI: `mcp-config.json` (`mcpServers.agora`), the Copilot hooks file
//! `hooks/agora.json` (an owned file that VS Code shares, see `install_shared_hooks`), and the
//! `copilot-instructions.md` block. Targets per scope: `ClientPaths::copilot_cli` (SPEC 4.3).
//! Order: validate and parse every existing file first (any failure -> `Err` before any write), then
//! write MCP, hooks, instructions; each write is one `Action`.

#[cfg(test)]
mod tests;

use super::claude::{Action, Scope, action, from_write};
use super::instructions;
use super::json_merge::{self, ServerChange, check_object_member};
use super::markers::{self, MarkerError};
use super::multi::{ClientReport, Ctx};
use super::paths::Targets;
use super::safe_write::{self, WriteResult, read_json};
use super::text_write;
use crate::CliError;
use agora_client::capabilities::{CapabilitiesFile, HarnessCapabilities};
use agora_deliver::Harness;
use serde_json::{Map, Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Mode of a new instructions file (a text file, not a secret).
const INSTRUCTIONS_MODE: u32 = 0o644;
/// A hook command written by agora for the Copilot dialect carries this; a hooks file with no such
/// string is not ours.
const COPILOT_DIALECT: &str = "--dialect copilot";
/// Copilot hook events agora uses, with the agora hook kind for each, in SPEC 4.3 order.
const HOOK_EVENTS: [(&str, &str); 4] = [
    ("sessionStart", "session-start"),
    ("userPromptSubmitted", "context"),
    ("postToolUse", "context"),
    ("agentStop", "stop"),
];
/// `timeoutSec` of every Copilot hook handler.
const HOOK_TIMEOUT_SEC: u64 = 10;
/// Warning when an `mcpServers.agora` entry that was not ours got replaced.
const WARN_FOREIGN_MCP: &str = "warning: an existing mcpServers.agora entry was not ours and was replaced; the backup keeps the old one";
/// Copilot CLI 0.0.369 ignores hook output: context and stop need Copilot CLI >= 1.0.72 (DOCS-ONLY).
const NOTE_USER_HOOKS_IGNORED: &str = "Copilot CLI 0.0.369 ignores hook output (context/stop need Copilot CLI >= 1.0.72; DOCS-ONLY); delivery there is the inbox_wait floor";
/// The user-level hooks directory needs Copilot CLI >= 0.0.422 (DOCS-ONLY).
const NOTE_USER_HOOKS_DIR: &str = "user-level hooks dir needs Copilot CLI >= 0.0.422";

/// The shared Copilot hooks document (SPEC 4.3). Copilot CLI and VS Code write the same bytes to
/// the same file, so the second install is `unchanged`. Each handler runs
/// `hook <kind> --dialect copilot --event <event>`. Fails only when the agora executable or home
/// is not valid UTF-8.
pub(crate) fn copilot_hooks_doc(c: &Ctx) -> Result<Value, CliError> {
    let mut hooks = Map::new();
    for (event, kind) in HOOK_EVENTS {
        let bash = c.hook_cmd(kind, "copilot", Some(event))?;
        hooks.insert(
            event.to_owned(),
            json!([{"type": "command", "bash": bash, "cwd": ".", "timeoutSec": HOOK_TIMEOUT_SEC}]),
        );
    }
    Ok(json!({"version": 1, "hooks": hooks}))
}

/// Pre-validate the shared hooks file: absent, or holding one of our commands -> `Ok`. A symlink,
/// malformed JSON or a foreign file -> `Err` naming the file (nothing is written).
pub(crate) fn check_shared_hooks(path: &Path) -> Result<(), CliError> {
    if !exists(path)? {
        return Ok(());
    }
    if is_ours(&read_json(path)?) {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "{} exists and is not managed by agora; nothing was written",
        path.display()
    )))
}

/// Write the shared Copilot hooks file. The file is owned whole, so its content is exactly
/// `copilot_hooks_doc` (`write_json`, pretty); byte-identical -> `unchanged`. Refuses a foreign file
/// (see `check_shared_hooks`). The VS Code installer calls this for the same file.
pub(crate) fn install_shared_hooks(c: &Ctx, path: &Path) -> Result<Action, CliError> {
    check_shared_hooks(path)?;
    let doc = copilot_hooks_doc(c)?;
    let res = safe_write::write_json(path, &doc, c.o.dry_run)?;
    Ok(from_write(path, res, Vec::new()))
}

/// Remove the shared Copilot hooks file, only when nothing else uses it: absent -> `absent`; not
/// ours -> `kept` ("not managed by agora"); listed in `caps` for another harness -> `kept` ("still
/// used by <harness>"); else `text_write::remove_file` (backup, then delete).
pub(crate) fn uninstall_shared_hooks(
    c: &Ctx,
    caps: &CapabilitiesFile,
    path: &Path,
    me: Harness,
) -> Result<Action, CliError> {
    if !exists(path)? {
        return Ok(action(path, "absent", "nothing to remove".to_owned()));
    }
    if !is_ours(&read_json(path)?) {
        return Ok(action(path, "kept", "not managed by agora".to_owned()));
    }
    if let Some(other) = shared_with(caps, path, me) {
        return Ok(action(path, "kept", format!("still used by {other}")));
    }
    text_write::remove_file(path, c.o.dry_run)
}

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`, `context_hook`, `stop_hook`, `hooks_trusted`, `owned_session` stays false: it
/// comes from the probe), `scopes = [c.scope_label()]` and `files` = every path written or edited.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let paths = copilot_files(c)?;

    // Validate everything before the first write.
    let mut root = read_json(&paths.mcp)?;
    check_object_member(&root, "mcpServers", &paths.mcp)?;
    check_shared_hooks(&paths.hooks)?;
    copilot_hooks_doc(c)?;
    let existing = text_write::read_text(&paths.instructions)?.unwrap_or_default();
    let instructions_text = markers::upsert_block(&existing, instructions::snippet())
        .map_err(|e| marker_error(&paths.instructions, &e))?;
    let exe = c.exe_str()?;
    let args = c.mcp_args()?;
    let entry = json!({"type": "local", "command": exe, "args": args, "tools": ["*"]});

    // Write MCP, hooks, instructions.
    let change = json_merge::set_server(&mut root, "mcpServers", "agora", entry);
    let warnings = match change {
        ServerChange::ReplacedForeign => vec![WARN_FOREIGN_MCP.to_owned()],
        ServerChange::Added | ServerChange::Updated | ServerChange::Unchanged => Vec::new(),
    };
    let mcp_res = safe_write::write_json(&paths.mcp, &root, c.o.dry_run)?;
    let mut actions = vec![from_write(&paths.mcp, mcp_res, warnings)];
    actions.push(install_shared_hooks(c, &paths.hooks)?);
    let instr_res = text_write::write_text(
        &paths.instructions,
        &instructions_text,
        INSTRUCTIONS_MODE,
        c.o.dry_run,
    )?;
    actions.push(from_write(&paths.instructions, instr_res, Vec::new()));

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            mcp_configured: true,
            scopes: vec![c.scope_label()],
            files: paths.display_paths(),
            ..HarnessCapabilities::default()
        },
        notes: notes(c.o.scope, &paths.mcp),
    })
}

/// Remove only what `install` added, in this scope. Shared files (the copilot-instructions block, the
/// Copilot hooks file) are removed only when `!caps.file_shared(path, <this harness>)`; otherwise the
/// action is `kept` with detail "still used by <other client>". The report's `caps.files` is this
/// scope's three paths (MCP config, hooks, instructions) whether each was removed, kept or absent.
/// Idempotent: a second run reports `absent` / `unchanged`.
pub fn uninstall(c: &Ctx, caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let paths = copilot_files(c)?;
    let me = Harness::CopilotCli;

    // Read everything first: an unreadable file stops the run before any removal.
    let mut root = read_json(&paths.mcp)?;
    if exists(&paths.hooks)? {
        read_json(&paths.hooks)?;
    }
    let existing = text_write::read_text(&paths.instructions)?;
    let stripped = match &existing {
        Some(text) => {
            markers::remove_block(text).map_err(|e| marker_error(&paths.instructions, &e))?
        }
        None => None,
    };

    let mut actions = Vec::new();
    if json_merge::remove_server(&mut root, "mcpServers", "agora") {
        let res = safe_write::write_json(&paths.mcp, &root, c.o.dry_run)?;
        actions.push(removal(&paths.mcp, res));
    } else {
        actions.push(action(
            &paths.mcp,
            "absent",
            "no agora server entry".to_owned(),
        ));
    }
    actions.push(uninstall_shared_hooks(c, caps, &paths.hooks, me)?);
    let shared = shared_with(caps, &paths.instructions, me);
    let instructions_action = match (stripped, shared) {
        (None, _) => action(&paths.instructions, "absent", "no agora block".to_owned()),
        (Some(_), Some(other)) => action(
            &paths.instructions,
            "kept",
            format!("still used by {other}"),
        ),
        (Some(text), None) => {
            let res =
                text_write::write_text(&paths.instructions, &text, INSTRUCTIONS_MODE, c.o.dry_run)?;
            removal(&paths.instructions, res)
        }
    };
    actions.push(instructions_action);

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files: paths.display_paths(),
            ..HarnessCapabilities::default()
        },
        notes: Vec::new(),
    })
}

/// The three files of this client in this scope: MCP config, shared hooks file, instructions.
struct CopilotFiles {
    mcp: PathBuf,
    hooks: PathBuf,
    instructions: PathBuf,
}

impl CopilotFiles {
    /// The three paths as the capabilities file records them, in the order MCP, hooks, instructions.
    fn display_paths(&self) -> Vec<String> {
        vec![
            self.mcp.display().to_string(),
            self.hooks.display().to_string(),
            self.instructions.display().to_string(),
        ]
    }
}

/// The Copilot CLI paths for `c.o.scope`. Both scopes have all three.
fn copilot_files(c: &Ctx) -> Result<CopilotFiles, CliError> {
    let Targets {
        mcp,
        hooks,
        instructions,
        ..
    } = c.paths.copilot_cli(c.o.scope);
    match (hooks, instructions) {
        (Some(hooks), Some(instructions)) => Ok(CopilotFiles {
            mcp,
            hooks,
            instructions,
        }),
        _ => Err(CliError::Usage(
            "internal: copilot-cli has no hooks or instructions path".to_owned(),
        )),
    }
}

/// The notes a user should read after an install in `scope`.
fn notes(scope: Scope, mcp: &Path) -> Vec<String> {
    match scope {
        Scope::User => vec![
            NOTE_USER_HOOKS_IGNORED.to_owned(),
            NOTE_USER_HOOKS_DIR.to_owned(),
        ],
        Scope::Project => vec![format!(
            "{} is read only by Copilot CLI >= 1.0.61 (after folder trust); older versions: use --scope user or `copilot --additional-mcp-config @<path>`",
            mcp.display()
        )],
    }
}

/// True when `doc` holds a string containing `--dialect copilot` (a hook command of ours) anywhere.
fn is_ours(doc: &Value) -> bool {
    match doc {
        Value::String(s) => s.contains(COPILOT_DIALECT),
        Value::Array(items) => items.iter().any(is_ours),
        Value::Object(map) => map.values().any(is_ours),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Whether `path` exists (a symlink counts: the readers refuse it afterwards).
fn exists(path: &Path) -> Result<bool, CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CliError::Io(e)),
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
