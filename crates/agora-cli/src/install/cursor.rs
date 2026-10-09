//! Installer for Cursor: `mcp.json` (`mcpServers.agora`), the flat `hooks.json`, and in project scope
//! the owned rule file `.cursor/rules/agora.mdc`. Targets: `ClientPaths::cursor` (SPEC 4.3); hook
//! commands via `Ctx::hook_cmd`. DOCS-ONLY (SPEC 2.6): Cursor is not installed on the build machine,
//! so these shapes follow cursor.com/docs/{hooks,context/mcp,context/rules} and were not run against
//! Cursor. Order: compute every new text in memory first (any failure -> `Err` before any write),
//! then write MCP, hooks, rule; each write is one `Action`.

use super::claude::{self, Action};
use super::instructions;
use super::json_merge::{self, ServerChange};
use super::markers;
use super::multi::{ClientReport, Ctx};
use super::safe_write;
use super::text_write;
use crate::CliError;
use agora_client::capabilities::{CapabilitiesFile, HarnessCapabilities};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The server name under `mcpServers`; also the owner name for `remove_server`.
const SERVER: &str = "agora";
/// The `mcp.json` container object of MCP servers.
const MCP_CONTAINER: &str = "mcpServers";
/// The `hooks.json` container of flat handler arrays.
const HOOKS: &str = "hooks";
/// The hook dialect in hook commands (`agora hook <kind> --dialect cursor`).
const DIALECT: &str = "cursor";
/// Handler timeout in seconds (SPEC 4.3).
const HOOK_TIMEOUT_S: u64 = 10;
/// Mode of the owned rule file: a readable project file.
const RULE_MODE: u32 = 0o644;
/// Note on every Cursor install.
const DOCS_ONLY_NOTE: &str =
    "Cursor support is DOCS-ONLY (Cursor not installed on the build machine)";
/// Hook events agora writes into `hooks.json`; each must be an array when present.
const HOOK_EVENTS: [&str; 4] = ["sessionStart", "postToolUse", "afterMCPExecution", "stop"];
/// Note on a user-scope install, which writes no rule file.
const USER_RULE_NOTE: &str = "Cursor user rules live in Cursor Settings → Rules; paste the Agora rule text from `agora install --client cursor --scope project --dry-run` output or the project file .cursor/rules/agora.mdc";
/// Detail line when an existing `mcpServers.agora` entry that was not ours got replaced.
const MCP_REPLACED: &str = "replaced an existing mcpServers.agora entry (backup kept)";

/// The files of one scope.
struct Files {
    mcp: PathBuf,
    hooks: PathBuf,
    /// The owned rule file; `None` in user scope.
    rule: Option<PathBuf>,
}

impl Files {
    /// Every path of this scope, as stored in `HarnessCapabilities::files`.
    fn listed(&self) -> Vec<String> {
        let mut out = vec![path_str(&self.mcp), path_str(&self.hooks)];
        out.extend(self.rule.as_deref().map(path_str));
        out
    }
}

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`, `context_hook`, `stop_hook`; `hooks_trusted` does not apply to Cursor;
/// `owned_session` stays false), `scopes = [c.scope_label()]` and `files` = every path written or edited.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let files = scope_files(c)?;
    let dry_run = c.o.dry_run;

    // 1. Validate and compute every new text in memory. Nothing is written before all checks pass.
    let mut mcp_doc = safe_write::read_json(&files.mcp)?;
    json_merge::check_object_member(&mcp_doc, MCP_CONTAINER, &files.mcp)?;
    let mcp_change = json_merge::set_server(&mut mcp_doc, MCP_CONTAINER, SERVER, mcp_entry(c)?);

    let mut hooks_doc = safe_write::read_json(&files.hooks)?;
    json_merge::check_object_member(&hooks_doc, HOOKS, &files.hooks)?;
    json_merge::check_event_arrays(&hooks_doc, &HOOK_EVENTS, &files.hooks)?;
    json_merge::merge_flat_hooks(&mut hooks_doc, &hook_entries(c)?);

    if let Some(rule) = &files.rule {
        check_rule_owned(rule)?;
    }

    // 2. Writes: MCP, hooks, then the owned rule file.
    let mut actions = Vec::new();
    let mcp_res = safe_write::write_json(&files.mcp, &mcp_doc, dry_run)?;
    actions.push(claude::from_write(
        &files.mcp,
        mcp_res,
        mcp_replaced_line(mcp_change),
    ));

    let hooks_res = safe_write::write_json(&files.hooks, &hooks_doc, dry_run)?;
    actions.push(claude::from_write(&files.hooks, hooks_res, Vec::new()));

    let mut notes = vec![DOCS_ONLY_NOTE.to_owned()];
    match &files.rule {
        Some(rule) => {
            let res =
                text_write::write_text(rule, &instructions::cursor_rule(), RULE_MODE, dry_run)?;
            actions.push(claude::from_write(rule, res, Vec::new()));
        }
        None => notes.push(USER_RULE_NOTE.to_owned()),
    }

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            scopes: vec![c.scope_label()],
            mcp_configured: true,
            context_hook: true,
            stop_hook: true,
            files: files.listed(),
            ..Default::default()
        },
        notes,
    })
}

/// Remove only what `install` added, in this scope. The owned rule file is removed only when it
/// carries the agora marker; a file without the marker is `kept`. A JSON file that ends up empty
/// stays as `{}`. Idempotent: a second run reports `unchanged` / `absent`. Cursor has no shared
/// file, so `_caps` is not consulted.
pub fn uninstall(c: &Ctx, _caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let files = scope_files(c)?;
    let dry_run = c.o.dry_run;

    // 1. Validate and compute every new text in memory, before the first write.
    let mcp_exists = exists(&files.mcp);
    let mut mcp_doc = safe_write::read_json(&files.mcp)?;
    json_merge::check_object_member(&mcp_doc, MCP_CONTAINER, &files.mcp)?;
    let mcp_removed = json_merge::remove_server(&mut mcp_doc, MCP_CONTAINER, SERVER);

    let hooks_exists = exists(&files.hooks);
    let mut hooks_doc = safe_write::read_json(&files.hooks)?;
    json_merge::check_object_member(&hooks_doc, HOOKS, &files.hooks)?;
    let hooks_removed = json_merge::remove_flat_hooks(&mut hooks_doc) > 0;

    let rule_text = match &files.rule {
        Some(rule) => text_write::read_text(rule)?,
        None => None,
    };

    // 2. Writes: MCP, hooks, then the owned rule file.
    let mut actions = Vec::new();
    actions.push(if mcp_removed {
        let res = safe_write::write_json(&files.mcp, &mcp_doc, dry_run)?;
        claude::from_write(&files.mcp, res, vec!["removed mcpServers.agora".to_owned()])
    } else {
        nothing_to_remove(&files.mcp, mcp_exists, "no agora entry")
    });

    actions.push(if hooks_removed {
        let res = safe_write::write_json(&files.hooks, &hooks_doc, dry_run)?;
        claude::from_write(
            &files.hooks,
            res,
            vec!["removed hooks.json agora handlers".to_owned()],
        )
    } else {
        nothing_to_remove(&files.hooks, hooks_exists, "no agora hook")
    });

    if let Some(rule) = &files.rule {
        actions.push(match &rule_text {
            None => claude::action(rule, "absent", String::new()),
            Some(text) if markers::has_block(text) => text_write::remove_file(rule, dry_run)?,
            Some(_) => claude::action(rule, "kept", "not managed by agora".to_owned()),
        });
    }

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files: files.listed(),
            ..Default::default()
        },
        notes: Vec::new(),
    })
}

/// The Cursor files of this scope. The rule file is the instructions target (project scope only).
fn scope_files(c: &Ctx) -> Result<Files, CliError> {
    let targets = c.paths.cursor(c.o.scope);
    let hooks = targets
        .hooks
        .ok_or_else(|| CliError::Usage("cursor has no hooks file".into()))?;
    Ok(Files {
        mcp: targets.mcp,
        hooks,
        rule: targets.instructions,
    })
}

/// The owned rule file may be replaced only when it is absent or carries the agora marker.
fn check_rule_owned(path: &Path) -> Result<(), CliError> {
    match text_write::read_text(path)? {
        Some(text) if !markers::has_block(&text) => Err(CliError::Usage(format!(
            "{}: exists and is not managed by agora; nothing was written",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// The `mcpServers.agora` entry. `${workspaceFolder}` is Cursor's interpolation, so the workspace
/// reaches agora through `AGORA_FOLDER`.
fn mcp_entry(c: &Ctx) -> Result<Value, CliError> {
    Ok(json!({
        "type": "stdio",
        "command": c.exe_str()?,
        "args": c.mcp_args()?,
        "env": {"AGORA_FOLDER": "${workspaceFolder}"},
    }))
}

/// The flat `hooks.json` entries `(event, handler)`: sessionStart -> session-start; postToolUse and
/// afterMCPExecution -> context; stop -> stop, with `loop_limit`.
fn hook_entries(c: &Ctx) -> Result<Vec<(&'static str, Value)>, CliError> {
    let mut stop = handler(c, "stop")?;
    stop["loop_limit"] = json!(agora_hooks::dialect::LOOP_LIMIT);
    Ok(vec![
        ("sessionStart", handler(c, "session-start")?),
        ("postToolUse", handler(c, "context")?),
        ("afterMCPExecution", handler(c, "context")?),
        ("stop", stop),
    ])
}

/// One command handler: `{"command": <hook command>, "timeout": 10}`.
fn handler(c: &Ctx, kind: &str) -> Result<Value, CliError> {
    Ok(json!({
        "command": c.hook_cmd(kind, DIALECT, None)?,
        "timeout": HOOK_TIMEOUT_S,
    }))
}

/// The detail line for a replaced foreign MCP entry; empty for every other change.
fn mcp_replaced_line(change: ServerChange) -> Vec<String> {
    if change == ServerChange::ReplacedForeign {
        vec![MCP_REPLACED.to_owned()]
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

/// True when something (file, dangling or not) is at `path`; symlinks are not followed.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests;
