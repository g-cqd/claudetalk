//! Installer for Codex CLI (config.toml MCP entry, hooks.json, optional hook trust, AGENTS.md block).
//! Targets per scope: `ClientPaths::codex` (SPEC 4.3); hook commands via `Ctx::hook_cmd`.
//! Order: validate and parse every existing file first (any failure -> `Err` before any write), then
//! write hooks.json, config.toml (MCP entry, and trust when the user config is that file), the user
//! config's trust (project scope only), and the instructions file; each write is one `Action`.

use super::claude::{self, Action, Scope};
use super::codex_toml;
use super::codex_trust;
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

/// The `[mcp_servers.<name>]` key, also the owner name in the capabilities entry.
const SERVER: &str = "agora";
/// Codex hook handler timeout in seconds (SPEC 4.3, the `"timeout": 10` of every agora handler).
const HOOK_TIMEOUT_S: u64 = 10;
/// Mode of a new `config.toml`: the user's Codex config can hold secrets.
const CONFIG_MODE: u32 = 0o600;
/// Mode of a new AGENTS.md: a shared, readable instructions file.
const INSTRUCTIONS_MODE: u32 = 0o644;
/// Detail line when an existing `mcp_servers.agora` entry that was not ours got replaced.
const MCP_REPLACED: &str = "replaced an existing mcp_servers.agora entry (backup kept)";
/// Note when the hooks are written but not trusted (Codex skips untrusted hooks silently).
const TRUST_NOTE: &str = "Codex skips new hooks until trusted: run `codex`, open `/hooks` and trust the 4 agora hooks, or re-run with --trust-hooks";

/// The four handlers agora writes, as `(event, hook kind)`. The Codex dialect reads the event from
/// the payload, so no `--event` is passed.
const HOOK_EVENTS: [(&str, &str); 4] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "context"),
    ("PostToolUse", "context"),
    ("Stop", "stop"),
];

/// Install this client for `c.o.scope`. The report's `caps` carries this run's flags
/// (`mcp_configured`, `context_hook`, `stop_hook`, `hooks_trusted`, `owned_session` stays false: it
/// comes from the probe), `scopes = [c.scope_label()]` and `files` = every path written or edited.
pub fn install(c: &Ctx) -> Result<ClientReport, CliError> {
    let (mcp_path, hooks_path, instr_path) = scope_files(c)?;
    let user_cfg = c.paths.codex_user_config();
    let same_file = user_cfg == mcp_path;
    let dry_run = c.o.dry_run;

    // 1. Validate and compute every new text in memory. Nothing is written before all checks pass.
    let config_text = text_write::read_text(&mcp_path)?.unwrap_or_default();
    let args = c.mcp_args()?;
    let (mcp_text, mcp_change) =
        codex_toml::set_mcp_server(&config_text, SERVER, c.exe_str()?, &args, None)
            .map_err(|e| invalid(&mcp_path, e))?;
    let user_text = if same_file {
        config_text.clone()
    } else {
        text_write::read_text(&user_cfg)?.unwrap_or_default()
    };
    let mut hooks_doc = safe_write::read_json(&hooks_path)?;
    json_merge::check_object_member(&hooks_doc, "hooks", &hooks_path)?;
    let events = HOOK_EVENTS.map(|(event, _)| event);
    json_merge::check_event_arrays(&hooks_doc, &events, &hooks_path)?;
    let written = hook_entries(c)?;
    let pairs = exact_pairs(&written);
    json_merge::merge_grouped_hooks(&mut hooks_doc, &written);
    let instr_text = text_write::read_text(&instr_path)?.unwrap_or_default();
    let instr_new = markers::upsert_block(&instr_text, instructions::snippet())
        .map_err(|e| invalid(&instr_path, e))?;
    // Dry parse of the trust entries: the final keys need hooks.json on disk (step 3 recomputes them).
    codex_toml::set_trust(
        &user_text,
        &codex_trust::our_trust_entries(&hooks_path, &hooks_doc, &pairs),
    )
    .map_err(|e| invalid(&user_cfg, e))?;

    // 2. hooks.json first: the trust keys name its canonical path, which exists only after this write.
    let mut actions = Vec::new();
    let mut files = Vec::new();
    let hooks_res = safe_write::write_json(&hooks_path, &hooks_doc, dry_run)?;
    actions.push(claude::from_write(&hooks_path, hooks_res, Vec::new()));
    files.push(path_str(&hooks_path));

    // 3. Trust: the final keys, read from the hooks.json as it now stands.
    let entries = codex_trust::our_trust_entries(&hooks_path, &hooks_doc, &pairs);
    let trust_text =
        codex_toml::set_trust(&user_text, &entries).map_err(|e| invalid(&user_cfg, e))?;
    let already_trusted = !entries.is_empty() && trust_text == user_text;
    let write_trust = c.trust_hooks && !entries.is_empty();
    let trusted = !entries.is_empty() && (c.trust_hooks || already_trusted);
    let trust_line = format!("wrote {} hook trust entries", entries.len());

    // 4. config.toml: the MCP entry, plus the trust entries when the user config is this same file.
    let mut mcp_lines = Vec::new();
    if mcp_change == ServerChange::ReplacedForeign {
        mcp_lines.push(MCP_REPLACED.to_owned());
    }
    let mut mcp_final = mcp_text;
    if same_file && write_trust {
        mcp_final =
            codex_toml::set_trust(&mcp_final, &entries).map_err(|e| invalid(&mcp_path, e))?;
        mcp_lines.push(trust_line.clone());
    }
    let mcp_res = text_write::write_text(&mcp_path, &mcp_final, CONFIG_MODE, dry_run)?;
    actions.push(claude::from_write(&mcp_path, mcp_res, mcp_lines));
    files.push(path_str(&mcp_path));

    // 5. Project scope only: trust still lives in the user config.
    if !same_file && write_trust {
        let res = text_write::write_text(&user_cfg, &trust_text, CONFIG_MODE, dry_run)?;
        actions.push(claude::from_write(&user_cfg, res, vec![trust_line]));
        files.push(path_str(&user_cfg));
    }

    // 6. The AGENTS.md block.
    let instr_res = text_write::write_text(&instr_path, &instr_new, INSTRUCTIONS_MODE, dry_run)?;
    actions.push(claude::from_write(&instr_path, instr_res, Vec::new()));
    files.push(path_str(&instr_path));

    let mut notes = Vec::new();
    if c.o.scope == Scope::Project {
        notes.push(format!(
            "Codex loads {proj}/.codex/ only for trusted projects (`[projects.\"{proj}\"] trust_level = \"trusted\"` in {cfg})",
            proj = c.o.project_dir.display(),
            cfg = user_cfg.display()
        ));
    }
    if dry_run && c.trust_hooks && !hooks_path.exists() {
        notes.push(
            "dry run: the trust keys use the hooks.json path as given, because the file does not exist yet"
                .to_owned(),
        );
    }
    if !trusted {
        notes.push(TRUST_NOTE.to_owned());
    }
    let caps = HarnessCapabilities {
        scopes: vec![c.scope_label()],
        mcp_configured: true,
        context_hook: true,
        stop_hook: true,
        hooks_trusted: Some(trusted),
        owned_session: false,
        files,
        ..Default::default()
    };
    Ok(ClientReport {
        actions,
        caps,
        notes,
    })
}

/// Remove only what `install` added, in this scope. Shared files (the AGENTS.md block, the Copilot
/// hooks file) are removed only when `!caps.file_shared(path, <this harness>)`; otherwise the action
/// is `kept` with detail "still used by <other client>". Idempotent: a second run reports `absent` /
/// `unchanged`. Writes run config.toml, the user config, the AGENTS.md block, and hooks.json last: a
/// failed write leaves hooks.json in place, so a rerun still finds the handlers that name the trust keys.
pub fn uninstall(c: &Ctx, caps: &CapabilitiesFile) -> Result<ClientReport, CliError> {
    let (mcp_path, hooks_path, instr_path) = scope_files(c)?;
    let user_cfg = c.paths.codex_user_config();
    let same_file = user_cfg == mcp_path;
    let dry_run = c.o.dry_run;
    let instr_str = path_str(&instr_path);
    let shared = caps.file_shared(&instr_str, Harness::Codex);

    // 1. Validate and compute every new text in memory, before the first write.
    let mut hooks_doc = safe_write::read_json(&hooks_path)?;
    json_merge::check_object_member(&hooks_doc, "hooks", &hooks_path)?;
    // Our trust keys name the hooks.json as it stands, so they are read before our handlers go. Only
    // the exact handlers this scope's install writes have keys; a looser match is removed without one.
    let written = hook_entries(c)?;
    let pairs = exact_pairs(&written);
    let keys: Vec<String> = codex_trust::our_trust_entries(&hooks_path, &hooks_doc, &pairs)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let removed = json_merge::remove_grouped_hooks(&mut hooks_doc);

    let config_text = text_write::read_text(&mcp_path)?;
    let user_text = if same_file {
        config_text.clone()
    } else {
        text_write::read_text(&user_cfg)?
    };
    let trust_new = match &user_text {
        Some(text) if !keys.is_empty() => {
            codex_toml::remove_trust(text, &keys).map_err(|e| invalid(&user_cfg, e))?
        }
        _ => None,
    };
    let mcp_base = if same_file {
        trust_new.clone().or_else(|| user_text.clone())
    } else {
        config_text.clone()
    };
    let mcp_new = match &mcp_base {
        Some(text) => {
            codex_toml::remove_mcp_server(text, SERVER).map_err(|e| invalid(&mcp_path, e))?
        }
        None => None,
    };
    let instr_text = text_write::read_text(&instr_path)?;
    let instr_new = match &instr_text {
        Some(text) if !shared => {
            markers::remove_block(text).map_err(|e| invalid(&instr_path, e))?
        }
        _ => None,
    };

    // 2. config.toml: the MCP entry, and the trust entries too when the user config is this file.
    let mut actions = Vec::new();
    let mut files = vec![
        path_str(&mcp_path),
        path_str(&hooks_path),
        instr_str.clone(),
    ];
    if !same_file && !keys.is_empty() {
        files.push(path_str(&user_cfg));
    }
    let mut mcp_lines = Vec::new();
    if mcp_new.is_some() {
        mcp_lines.push("removed mcp_servers.agora".to_owned());
    }
    if same_file && trust_new.is_some() {
        mcp_lines.push("removed agora hook trust".to_owned());
    }
    let mcp_final = if same_file {
        mcp_new.or(trust_new.clone())
    } else {
        mcp_new
    };
    actions.push(edit_action(
        &mcp_path,
        config_text.is_some(),
        mcp_final,
        mcp_lines,
        CONFIG_MODE,
        dry_run,
        "no agora entry",
    )?);

    // 3. Project scope: the trust entries live in the user config.
    if !same_file && !keys.is_empty() {
        actions.push(edit_action(
            &user_cfg,
            user_text.is_some(),
            trust_new,
            vec!["removed agora hook trust".to_owned()],
            CONFIG_MODE,
            dry_run,
            "no agora trust",
        )?);
    }

    // 4. The AGENTS.md block, unless another client's entry still lists the file.
    actions.push(if instr_text.is_none() {
        claude::action(&instr_path, "absent", String::new())
    } else if shared {
        let other = other_user(caps, &instr_str);
        claude::action(&instr_path, "kept", format!("still used by {other}"))
    } else {
        edit_action(
            &instr_path,
            true,
            instr_new,
            Vec::new(),
            INSTRUCTIONS_MODE,
            dry_run,
            "no agora block",
        )?
    });

    // 5. hooks.json last, kept (as `{}` when nothing else is in it) and never deleted. It is reported
    // first, as before.
    let hooks_action = if removed > 0 {
        let res = safe_write::write_json(&hooks_path, &hooks_doc, dry_run)?;
        claude::from_write(&hooks_path, res, vec!["agora hooks removed".to_owned()])
    } else if exists(&hooks_path) {
        claude::action(&hooks_path, "unchanged", "no agora hooks".to_owned())
    } else {
        claude::action(&hooks_path, "absent", String::new())
    };
    actions.insert(0, hooks_action);

    Ok(ClientReport {
        actions,
        caps: HarnessCapabilities {
            files,
            ..Default::default()
        },
        notes: Vec::new(),
    })
}

/// The Codex files of this scope: `(config.toml, hooks.json, AGENTS.md)`.
fn scope_files(c: &Ctx) -> Result<(PathBuf, PathBuf, PathBuf), CliError> {
    let targets = c.paths.codex(c.o.scope);
    let hooks = targets
        .hooks
        .ok_or_else(|| CliError::Usage("codex has no hooks file".into()))?;
    let instructions = targets
        .instructions
        .ok_or_else(|| CliError::Usage("codex has no instructions file".into()))?;
    Ok((targets.mcp, hooks, instructions))
}

/// One grouped hook entry for `json_merge::merge_grouped_hooks`: `(event, matcher, handler)`.
type GroupedHook = (&'static str, Option<&'static str>, Value);

/// The grouped hook entries agora writes: no matcher, `"timeout": 10`.
fn hook_entries(c: &Ctx) -> Result<Vec<GroupedHook>, CliError> {
    HOOK_EVENTS
        .iter()
        .map(|(event, kind)| {
            let command = c.hook_cmd(kind, "codex", None)?;
            let handler = json!({"type": "command", "command": command, "timeout": HOOK_TIMEOUT_S});
            Ok((*event, None, handler))
        })
        .collect()
}

/// The `(event, command)` pair of each grouped entry: the exact handlers this run writes, which are the
/// only ones whose trust is written or removed.
fn exact_pairs(entries: &[GroupedHook]) -> Vec<(&str, &str)> {
    entries
        .iter()
        .filter_map(|(event, _, handler)| Some((*event, handler.get("command")?.as_str()?)))
        .collect()
}

/// The action for one file that removal edits: `Some(text)` is written (or dry-run diffed), `None`
/// is `unchanged` when the file exists and `absent` when it does not.
fn edit_action(
    path: &Path,
    existed: bool,
    new: Option<String>,
    lines: Vec<String>,
    mode: u32,
    dry_run: bool,
    nothing: &str,
) -> Result<Action, CliError> {
    match new {
        Some(text) => {
            let res = text_write::write_text(path, &text, mode, dry_run)?;
            Ok(claude::from_write(path, res, lines))
        }
        None if existed => Ok(claude::action(path, "unchanged", nothing.to_owned())),
        None => Ok(claude::action(path, "absent", String::new())),
    }
}

/// The first other harness whose capabilities entry lists `path` (for the `kept` detail).
fn other_user(caps: &CapabilitiesFile, path: &str) -> &'static str {
    Harness::ALL
        .into_iter()
        .filter(|h| *h != Harness::Codex)
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
