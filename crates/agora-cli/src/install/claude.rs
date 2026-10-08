//! Installing Agora into Claude Code: the plugin (default) or plain settings entries.
//!
//! Plugin mode: the embedded plugin (`assets::PLUGIN_FILES`) is materialized into
//! `<agora home>/claude/marketplace/` (`.claude-plugin/marketplace.json` with the plugin at
//! `agora/`), with the absolute exe and home substituted: in `hooks.json` every
//! `"command": "agora hook X"` becomes `'<exe>' --home '<home>' hook X` (POSIX single-quote
//! escaping); in `.mcp.json` `"command":"agora"` -> the exe, `args` -> `["--home","<home>","mcp"]`,
//! env `AGORA_HOME` -> the home; in `register.js` the line `const AGORA = ["agora"];` ->
//! `const AGORA = <JSON array [exe,"--home",home]>;`. Then the scope's settings.json gets
//! `extraKnownMarketplaces.agora = {"source":{"source":"directory","path":"<marketplace dir>"}}`
//! and `enabledPlugins["agora@agora"] = true`.
//!
//! Entries mode: `mcpServers.agora = {"type":"stdio","command":"<exe>","args":["--home",
//! "<home>","mcp"],"env":{"MCP_PROTOCOL_NEGOTIATION":"legacy"}}` in the scope's MCP file, plus
//! the hooks of hooks.json (no modules) with absolute commands merged into the scope's
//! settings.json `hooks`. No mod in this mode.
//!
//! Hook ownership: a settings hook handler is OURS iff its `command` (trimmed) contains `agora`
//! and ends with one of ` hook session-start`, ` hook context`, ` hook stop`, ` hook rewake`,
//! ` hook rewake --quick`. User groups and handlers are never modified or dropped.

use super::assets::PLUGIN_FILES;
use super::safe_write::{WriteResult, read_json, write_bytes, write_json};
use crate::CliError;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// user: `~/.claude/settings.json` and `~/.claude.json`; project: `<project>/.claude/settings.json`
/// and `<project>/.mcp.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plugin,
    Entries,
}

/// Everything `install` / `uninstall` need, resolved by the caller (testable with temp dirs).
#[derive(Debug, Clone)]
pub struct Options {
    pub scope: Scope,
    pub mode: Mode,
    pub dry_run: bool,
    /// Absolute agora home.
    pub agora_home: PathBuf,
    /// The user's home (`$HOME`).
    pub user_home: PathBuf,
    /// The project directory (the process cwd) for `Scope::Project`.
    pub project_dir: PathBuf,
    /// Absolute path of the `agora` executable.
    pub exe: PathBuf,
}

/// One file the installer touched (or would touch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    pub path: String,
    /// `wrote` | `unchanged` | `dry_run` | `removed` | `absent`.
    pub result: String,
    pub detail: String,
}

pub const MARKETPLACE: &str = "agora";
pub const PLUGIN: &str = "agora";
pub const SERVER: &str = "agora";

/// The key of the plugin in `enabledPlugins`.
const PLUGIN_KEY: &str = "agora@agora";
/// Mode of the materialized plugin files.
const PLUGIN_FILE_MODE: u32 = 0o644;
/// The `<sub>` of every hook command this installer writes (`... hook <sub>`).
const OUR_SUBS: [&str; 5] = [
    "session-start",
    "context",
    "stop",
    "rewake",
    "rewake --quick",
];
/// The one line of `register.js` that the installer rewrites.
const REGISTER_LINE: &str = r#"const AGORA = ["agora"];"#;
const ENTRIES_WARNING: &str = "warning: entries-mode hooks are also installed; run `agora uninstall --client claude --mode entries`";
const PLUGIN_PRESENT_WARNING: &str = "warning: plugin-mode install also present; run `agora uninstall --client claude --mode plugin`";
const FOREIGN_MARKETPLACE_WARNING: &str = "warning: replaced an existing extraKnownMarketplaces.agora entry that was not created by agora (backup kept)";
const FOREIGN_SERVER_WARNING: &str = "warning: replaced an existing mcpServers.agora entry that was not created by agora (backup kept)";

/// The scope's `settings.json`.
pub fn settings_path(o: &Options) -> PathBuf {
    match o.scope {
        Scope::User => o.user_home.join(".claude").join("settings.json"),
        Scope::Project => o.project_dir.join(".claude").join("settings.json"),
    }
}

/// The scope's MCP file (`~/.claude.json` or `<project>/.mcp.json`).
pub fn mcp_path(o: &Options) -> PathBuf {
    match o.scope {
        Scope::User => o.user_home.join(".claude.json"),
        Scope::Project => o.project_dir.join(".mcp.json"),
    }
}

/// `<agora home>/claude/marketplace`.
pub fn marketplace_dir(o: &Options) -> PathBuf {
    o.agora_home.join("claude").join("marketplace")
}

/// POSIX shell single-quoting: `'` + s with every `'` written as `'\''` + `'`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `'<exe>' --home '<home>' hook <sub>` (exe and home shell-quoted).
pub fn hook_command(o: &Options, sub: &str) -> String {
    format!(
        "{} --home {} hook {sub}",
        shell_quote(&o.exe.to_string_lossy()),
        shell_quote(&o.agora_home.to_string_lossy())
    )
}

/// Ownership test of the module doc.
pub fn is_our_hook_command(cmd: &str) -> bool {
    our_sub(cmd).is_some()
}

/// The `<sub>` of one of our hook commands, or `None` when the command is not ours.
fn our_sub(cmd: &str) -> Option<&'static str> {
    let cmd = cmd.trim();
    if !cmd.contains("agora") {
        return None;
    }
    OUR_SUBS.iter().copied().find(|sub| {
        cmd.strip_suffix(*sub)
            .and_then(|rest| rest.strip_suffix(" hook "))
            .is_some()
    })
}

fn our_sub_of(handler: &Value) -> Option<&'static str> {
    our_sub(command_of(handler)?)
}

fn command_of(handler: &Value) -> Option<&str> {
    handler.get("command").and_then(Value::as_str)
}

fn group_matcher(group: &Value) -> Option<&str> {
    group.get("matcher").and_then(Value::as_str)
}

fn handlers(group: &Value) -> &[Value] {
    match group.get("hooks").and_then(Value::as_array) {
        Some(list) => list.as_slice(),
        None => &[],
    }
}

fn utf8_path(p: &Path) -> Result<&str, CliError> {
    p.to_str()
        .ok_or_else(|| CliError::Usage(format!("{} is not a UTF-8 path", p.display())))
}

/// `(exe, home)` as strings: the substitutions need UTF-8.
fn exe_and_home(o: &Options) -> Result<(&str, &str), CliError> {
    Ok((utf8_path(&o.exe)?, utf8_path(&o.agora_home)?))
}

fn embedded(rel: &str) -> Result<&'static str, CliError> {
    PLUGIN_FILES
        .iter()
        .find(|(path, _)| *path == rel)
        .map(|(_, content)| *content)
        .ok_or_else(|| CliError::Usage(format!("plugin file {rel} is not embedded")))
}

fn parse_embedded(rel: &str) -> Result<Value, CliError> {
    serde_json::from_str(embedded(rel)?)
        .map_err(|e| CliError::Usage(format!("embedded {rel} is not valid JSON: {e}")))
}

fn pretty_bytes(value: &Value) -> Result<Vec<u8>, CliError> {
    let mut text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    text.push('\n');
    Ok(text.into_bytes())
}

fn root_mut(value: &mut Value) -> Result<&mut Map<String, Value>, CliError> {
    value
        .as_object_mut()
        .ok_or_else(|| CliError::Usage("expected a JSON object".to_owned()))
}

/// The object under `parent[key]`, created when missing; a non-object there is an error.
fn object_member<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, CliError> {
    parent
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            CliError::Usage(format!(
                "settings key \"{key}\" is not an object; fix it, nothing was written"
            ))
        })
}

/// The hook handlers of the embedded `hooks/hooks.json` with the absolute commands applied.
fn plugin_hooks(o: &Options) -> Result<Value, CliError> {
    exe_and_home(o)?;
    let mut doc = parse_embedded("hooks/hooks.json")?;
    let events = doc
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            CliError::Usage("embedded hooks/hooks.json has no hooks object".to_owned())
        })?;
    for groups in events.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for handler in list.iter_mut() {
                let Some(command) = command_of(handler) else {
                    continue;
                };
                let Some(sub) = command.strip_prefix("agora hook ") else {
                    return Err(CliError::Usage(format!(
                        "embedded hooks.json has an unexpected command: {command}"
                    )));
                };
                let rewritten = hook_command(o, sub);
                if let Some(obj) = handler.as_object_mut() {
                    obj.insert("command".to_owned(), Value::String(rewritten));
                }
            }
        }
    }
    Ok(doc)
}

/// The hook handlers of the embedded `hooks/hooks.json` with absolute commands:
/// `(event, matcher, handler JSON)` in file order, the handler being
/// `{"type":"command","command":..,"timeout":..,"asyncRewake":..}` as in the file.
pub fn hook_entries(
    o: &Options,
) -> Result<Vec<(String, Option<String>, serde_json::Value)>, CliError> {
    let doc = plugin_hooks(o)?;
    let mut out = Vec::new();
    for (event, groups) in doc
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group_matcher(group).map(str::to_owned);
            for handler in handlers(group) {
                if handler.get("type").and_then(Value::as_str) == Some("command") {
                    out.push((event.clone(), matcher.clone(), handler.clone()));
                }
            }
        }
    }
    Ok(out)
}

fn has_command(groups: &[Value], command: &str) -> bool {
    groups
        .iter()
        .any(|g| handlers(g).iter().any(|h| command_of(h) == Some(command)))
}

/// Replace the first OURS handler with sub `sub` in a group with `matcher`; true when replaced.
fn replace_ours(groups: &mut [Value], matcher: Option<&str>, sub: &str, handler: &Value) -> bool {
    for group in groups.iter_mut() {
        if group_matcher(group) != matcher {
            continue;
        }
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(slot) = list.iter_mut().find(|h| our_sub_of(h) == Some(sub)) {
            *slot = handler.clone();
            return true;
        }
    }
    false
}

fn new_group(matcher: Option<&str>, handler: &Value) -> Value {
    let mut group = Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".to_owned(), Value::String(matcher.to_owned()));
    }
    group.insert("hooks".to_owned(), Value::Array(vec![handler.clone()]));
    Value::Object(group)
}

/// Merge `entries` into `settings["hooks"]`; returns `(added, already_present, migrated)`. For
/// each `(event, matcher, handler)` add a group `{matcher?, hooks:[handler]}` unless an OURS
/// handler with the identical command already exists in that event; an OURS handler with a
/// different command (an older exe path) for the same hook is replaced in place (migrated).
/// Never modifies or drops user groups.
pub fn merge_hooks(
    settings: &mut serde_json::Value,
    entries: &[(String, Option<String>, serde_json::Value)],
) -> (usize, usize, usize) {
    let Some(root) = settings.as_object_mut() else {
        return (0, 0, 0);
    };
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
    else {
        return (0, 0, 0);
    };
    let (mut added, mut present, mut migrated) = (0, 0, 0);
    for (event, matcher, handler) in entries {
        let Some(command) = command_of(handler) else {
            continue;
        };
        let Some(groups) = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
        else {
            continue;
        };
        if has_command(groups, command) {
            present += 1;
        } else if our_sub(command)
            .is_some_and(|sub| replace_ours(groups, matcher.as_deref(), sub, handler))
        {
            migrated += 1;
        } else {
            groups.push(new_group(matcher.as_deref(), handler));
            added += 1;
        }
    }
    (added, present, migrated)
}

/// Drop OURS handlers from `groups`; a group is dropped when it held OURS handlers and has none
/// left. Returns the number of handlers removed.
fn remove_ours_from_groups(groups: &mut Vec<Value>) -> usize {
    let mut removed = 0;
    groups.retain_mut(|group| {
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        list.retain(|h| !command_of(h).is_some_and(is_our_hook_command));
        let gone = before - list.len();
        removed += gone;
        gone == 0 || !list.is_empty()
    });
    removed
}

/// Remove OURS handlers from `settings["hooks"]`, then groups whose `hooks` became empty (only
/// groups that contained an OURS handler), then empty event arrays, then an empty `hooks`.
/// Returns the number of handlers removed.
pub fn remove_hooks(settings: &mut serde_json::Value) -> usize {
    let Some(root) = settings.as_object_mut() else {
        return 0;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied = Vec::new();
    for (event, value) in hooks.iter_mut() {
        let Some(groups) = value.as_array_mut() else {
            continue;
        };
        let n = remove_ours_from_groups(groups);
        removed += n;
        if n > 0 && groups.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in &emptied {
        hooks.remove(event);
    }
    let now_empty = hooks.is_empty();
    if removed > 0 && now_empty {
        root.remove("hooks");
    }
    removed
}

/// `.mcp.json` with the server's command, args and AGORA_HOME substituted.
fn mcp_doc(exe: &str, home: &str) -> Result<Value, CliError> {
    let mut doc = parse_embedded(".mcp.json")?;
    let server = doc
        .get_mut("mcpServers")
        .and_then(|servers| servers.get_mut(SERVER))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| CliError::Usage(format!("embedded .mcp.json has no {SERVER} server")))?;
    server.insert("command".to_owned(), Value::String(exe.to_owned()));
    server.insert("args".to_owned(), json!(["--home", home, "mcp"]));
    let env = server
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| CliError::Usage("embedded .mcp.json env is not an object".to_owned()))?;
    env.insert("AGORA_HOME".to_owned(), Value::String(home.to_owned()));
    Ok(doc)
}

/// `register.js` with its one `const AGORA = ["agora"];` line replaced by a JSON array literal.
fn register_js(content: &str, exe: &str, home: &str) -> Result<String, CliError> {
    let replacement = format!("const AGORA = {};", json!([exe, "--home", home]));
    let mut found = false;
    let mut out = String::with_capacity(content.len() + replacement.len());
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if body == REGISTER_LINE {
            found = true;
            out.push_str(&replacement);
            out.push_str(line.strip_prefix(body).unwrap_or(""));
        } else {
            out.push_str(line);
        }
    }
    if !found {
        return Err(CliError::Usage(
            "register.js lacks the AGORA line".to_owned(),
        ));
    }
    Ok(out)
}

/// The plugin files with substitutions applied: `(relative path under marketplace_dir, bytes)`,
/// including `.claude-plugin/marketplace.json` and every `agora/<plugin file>`.
pub fn materialize(o: &Options) -> Result<Vec<(PathBuf, Vec<u8>)>, CliError> {
    let (exe, home) = exe_and_home(o)?;
    let mut out = Vec::with_capacity(PLUGIN_FILES.len() + 1);
    for (rel, content) in PLUGIN_FILES {
        let bytes = match *rel {
            "hooks/hooks.json" => pretty_bytes(&plugin_hooks(o)?)?,
            ".mcp.json" => pretty_bytes(&mcp_doc(exe, home)?)?,
            "hooks/register.js" => register_js(content, exe, home)?.into_bytes(),
            _ => content.as_bytes().to_vec(),
        };
        out.push((Path::new("agora").join(*rel), bytes));
    }
    let marketplace = json!({
        "name": MARKETPLACE,
        "owner": {"name": "Agora contributors"},
        "plugins": [{
            "name": PLUGIN,
            "source": "./agora",
            "description": "Agora MCP tools and Claude Code delivery hooks",
        }],
    });
    out.push((
        PathBuf::from(".claude-plugin/marketplace.json"),
        pretty_bytes(&marketplace)?,
    ));
    Ok(out)
}

fn action(path: &Path, result: &str, detail: String) -> Action {
    Action {
        path: path.display().to_string(),
        result: result.to_owned(),
        detail,
    }
}

/// An `Action` from a write: `lines` first, then the backup path or the dry-run diff.
fn from_write(path: &Path, res: WriteResult, mut lines: Vec<String>) -> Action {
    let result = match res {
        WriteResult::Wrote { backup } => {
            lines.extend(backup.map(|b| format!("backup {}", b.display())));
            "wrote"
        }
        WriteResult::Unchanged => "unchanged",
        WriteResult::DryRun { diff } => {
            lines.extend(diff);
            "dry_run"
        }
    };
    action(path, result, lines.join("\n"))
}

fn exists(path: &Path) -> Result<bool, CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// True when any handler of `settings["hooks"]` is OURS.
fn has_our_hooks(settings: &Value) -> bool {
    let Some(events) = settings.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    events
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .any(|group| {
            handlers(group)
                .iter()
                .any(|h| command_of(h).is_some_and(is_our_hook_command))
        })
}

/// `settings["hooks"]` must be absent or an object of arrays; checked before a merge.
fn check_hooks_shape(settings: &Value, path: &Path) -> Result<(), CliError> {
    match settings.get("hooks") {
        None => Ok(()),
        Some(Value::Object(events)) if events.values().all(Value::is_array) => Ok(()),
        Some(_) => Err(CliError::Usage(format!(
            "{}: \"hooks\" must be an object of arrays; fix it, nothing was written",
            path.display()
        ))),
    }
}

/// The `extraKnownMarketplaces.agora` value plugin mode writes.
fn marketplace_entry(market: &str) -> Value {
    json!({"source": {"source": "directory", "path": market}})
}

/// Plugin mode: `extraKnownMarketplaces.agora` and `enabledPlugins["agora@agora"]`.
fn enable_plugin(settings: &mut Value, market: &str) -> Result<(), CliError> {
    let root = root_mut(settings)?;
    object_member(root, "extraKnownMarketplaces")?
        .insert(MARKETPLACE.to_owned(), marketplace_entry(market));
    object_member(root, "enabledPlugins")?.insert(PLUGIN_KEY.to_owned(), Value::Bool(true));
    Ok(())
}

/// N7: true when the write replaces `prior` with a different value that does not look like ours
/// (`looks_ours`). An older agora value is replaced silently.
fn replaces_foreign(
    prior: Option<&Value>,
    written: &Value,
    looks_ours: fn(&Value) -> bool,
) -> bool {
    prior.is_some_and(|old| old != written && !looks_ours(old))
}

/// A marketplace entry is agora's when its source path is an agora marketplace dir.
fn marketplace_entry_is_ours(entry: &Value) -> bool {
    entry
        .pointer("/source/path")
        .and_then(Value::as_str)
        .is_some_and(|p| p.ends_with("/claude/marketplace"))
}

/// An MCP server entry is agora's when its command mentions agora.
fn mcp_server_entry_is_ours(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("agora"))
}

/// Plugin mode uninstall: remove our enable flag and marketplace entry, and the containers this
/// removal emptied. True when something was removed.
fn unset_plugin(root: &mut Map<String, Value>, market: &str) -> bool {
    let mut changed = false;
    let mut drop_enabled = false;
    if let Some(enabled) = root
        .get_mut("enabledPlugins")
        .and_then(Value::as_object_mut)
        && enabled.remove(PLUGIN_KEY).is_some()
    {
        changed = true;
        drop_enabled = enabled.is_empty();
    }
    if drop_enabled {
        root.remove("enabledPlugins");
    }

    let mut drop_known = false;
    if let Some(known) = root
        .get_mut("extraKnownMarketplaces")
        .and_then(Value::as_object_mut)
    {
        let ours = known
            .get(MARKETPLACE)
            .and_then(|m| m.pointer("/source/path"))
            .and_then(Value::as_str)
            == Some(market);
        if ours {
            known.remove(MARKETPLACE);
            changed = true;
            drop_known = known.is_empty();
        }
    }
    if drop_known {
        root.remove("extraKnownMarketplaces");
    }
    changed
}

/// The `mcpServers.agora` value entries mode writes.
fn mcp_server_entry(exe: &str, home: &str) -> Value {
    json!({
        "type": "stdio",
        "command": exe,
        "args": ["--home", home, "mcp"],
        "env": {"MCP_PROTOCOL_NEGOTIATION": "legacy"},
    })
}

/// Entries mode: `mcpServers.agora` set to the exe, args and the legacy negotiation env.
fn add_mcp_server(mcp: &mut Value, exe: &str, home: &str) -> Result<(), CliError> {
    let root = root_mut(mcp)?;
    object_member(root, "mcpServers")?.insert(SERVER.to_owned(), mcp_server_entry(exe, home));
    Ok(())
}

/// Remove `mcpServers.agora` only when its command contains `agora`; drop an emptied
/// `mcpServers`. True when removed.
fn remove_mcp_server(root: &mut Map<String, Value>) -> bool {
    let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return false;
    };
    let ours = servers
        .get(SERVER)
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("agora"));
    if !ours || servers.remove(SERVER).is_none() {
        return false;
    }
    let now_empty = servers.is_empty();
    if now_empty {
        root.remove("mcpServers");
    }
    true
}

/// Read an existing JSON file (a missing one is `absent`), let `edit` change it, and write it
/// when `edit` reports a change (`Some(note)`); otherwise `unchanged`.
fn edit_existing(
    path: &Path,
    dry_run: bool,
    edit: impl FnOnce(&mut Value) -> Result<Option<String>, CliError>,
) -> Result<Action, CliError> {
    if !exists(path)? {
        return Ok(action(path, "absent", String::new()));
    }
    let mut doc = read_json(path)?;
    match edit(&mut doc)? {
        None => Ok(action(path, "unchanged", String::new())),
        Some(note) => Ok(from_write(
            path,
            write_json(path, &doc, dry_run)?,
            vec![note],
        )),
    }
}

/// Remove the marketplace directory recursively; refuses a symlink or a non-directory.
fn remove_marketplace(dir: &Path, dry_run: bool) -> Result<Action, CliError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(action(dir, "absent", String::new()));
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::Usage(format!(
            "refusing to delete symlink {}",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(CliError::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if dry_run {
        return Ok(action(
            dir,
            "dry_run",
            "remove directory recursively".to_owned(),
        ));
    }
    fs::remove_dir_all(dir)?;
    Ok(action(dir, "removed", String::new()))
}

/// True when `settings.enabledPlugins["agora@agora"]` is `true`.
fn plugin_enabled(settings: &Value) -> bool {
    settings
        .get("enabledPlugins")
        .and_then(|p| p.get(PLUGIN_KEY))
        .and_then(Value::as_bool)
        == Some(true)
}

/// True when `settings.extraKnownMarketplaces.agora.source.path` is exactly `market`.
fn points_at(settings: &Value, market: &Path) -> bool {
    market.to_str().is_some_and(|m| {
        settings
            .pointer("/extraKnownMarketplaces/agora/source/path")
            .and_then(Value::as_str)
            == Some(m)
    })
}

/// True when `mcp.mcpServers.agora` exists and its command contains `agora`.
fn has_our_mcp_server(mcp: &Value) -> bool {
    mcp.pointer("/mcpServers/agora/command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("agora"))
}

/// A settings write failed. When this run already wrote the MCP file, the error names that file
/// (and its backup) so the user sees what is in place; otherwise the error is returned as is.
fn after_mcp_write(err: CliError, mcp_file: &Path, mcp_res: &WriteResult) -> CliError {
    match mcp_res {
        WriteResult::Wrote { backup } => {
            let backup = backup
                .as_ref()
                .map(|b| format!(" (backup {})", b.display()))
                .unwrap_or_default();
            CliError::Usage(format!(
                "settings write failed after {} was already written{backup}: {err}; undo with `agora uninstall --client claude --mode entries`",
                mcp_file.display()
            ))
        }
        WriteResult::Unchanged | WriteResult::DryRun { .. } => err,
    }
}

/// Install per `o.mode` and `o.scope` using `safe_write`; one `Action` per file touched.
pub fn install(o: &Options) -> Result<Vec<Action>, CliError> {
    match o.mode {
        Mode::Plugin => install_plugin(o),
        Mode::Entries => install_entries(o),
    }
}

fn install_plugin(o: &Options) -> Result<Vec<Action>, CliError> {
    let market = marketplace_dir(o);
    let market_str = utf8_path(&market)?;
    // Everything that can fail on the input is checked before the first write.
    let files = materialize(o)?;
    let settings_file = settings_path(o);
    let mut settings = read_json(&settings_file)?;
    let entries_installed = has_our_hooks(&settings);
    let prior_marketplace = settings.pointer("/extraKnownMarketplaces/agora").cloned();
    enable_plugin(&mut settings, market_str)?;

    let mut actions = Vec::with_capacity(files.len() + 1);
    for (rel, bytes) in &files {
        let path = market.join(rel);
        let res = write_bytes(&path, bytes, PLUGIN_FILE_MODE, o.dry_run)?;
        actions.push(from_write(&path, res, Vec::new()));
    }
    let mut lines = vec![
        format!("enabledPlugins.{PLUGIN_KEY} = true"),
        format!("extraKnownMarketplaces.{MARKETPLACE}.path = {market_str}"),
    ];
    if entries_installed {
        lines.push(ENTRIES_WARNING.to_owned());
    }
    if replaces_foreign(
        prior_marketplace.as_ref(),
        &marketplace_entry(market_str),
        marketplace_entry_is_ours,
    ) {
        lines.push(FOREIGN_MARKETPLACE_WARNING.to_owned());
    }
    let res = write_json(&settings_file, &settings, o.dry_run)?;
    actions.push(from_write(&settings_file, res, lines));
    Ok(actions)
}

fn install_entries(o: &Options) -> Result<Vec<Action>, CliError> {
    let (exe, home) = exe_and_home(o)?;
    let hooks = hook_entries(o)?;
    let mcp_file = mcp_path(o);
    let settings_file = settings_path(o);
    let mut mcp = read_json(&mcp_file)?;
    let mut settings = read_json(&settings_file)?;
    check_hooks_shape(&settings, &settings_file)?;
    let prior_server = mcp.pointer("/mcpServers/agora").cloned();
    add_mcp_server(&mut mcp, exe, home)?;
    let mut mcp_lines = vec![format!("mcpServers.{SERVER} set")];
    if replaces_foreign(
        prior_server.as_ref(),
        &mcp_server_entry(exe, home),
        mcp_server_entry_is_ours,
    ) {
        mcp_lines.push(FOREIGN_SERVER_WARNING.to_owned());
    }
    let (added, present, migrated) = merge_hooks(&mut settings, &hooks);
    let mut settings_lines = vec![format!(
        "hooks added={added} present={present} migrated={migrated}"
    )];
    if plugin_enabled(&settings) {
        settings_lines.push(PLUGIN_PRESENT_WARNING.to_owned());
    }

    let mcp_res = write_json(&mcp_file, &mcp, o.dry_run)?;
    let settings_res = write_json(&settings_file, &settings, o.dry_run)
        .map_err(|e| after_mcp_write(e, &mcp_file, &mcp_res))?;
    Ok(vec![
        from_write(&mcp_file, mcp_res, mcp_lines),
        from_write(&settings_file, settings_res, settings_lines),
    ])
}

/// The scope of the other half of the User / Project pair.
fn other_scope(scope: Scope) -> Scope {
    match scope {
        Scope::User => Scope::Project,
        Scope::Project => Scope::User,
    }
}

/// The name of a scope, as shown in messages.
fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
    }
}

/// True when the OTHER scope (same home, project and agora home) still uses this agora: its
/// settings point at our marketplace dir, enable our plugin, or hold OURS hooks, or its MCP file
/// has an `mcpServers.agora` whose command contains `agora`. A file that cannot be read counts as
/// not using it.
pub fn other_scope_uses_agora(o: &Options) -> bool {
    let other = Options {
        scope: other_scope(o.scope),
        ..o.clone()
    };
    let market = marketplace_dir(o);
    let settings_uses = read_json(&settings_path(&other))
        .is_ok_and(|s| points_at(&s, &market) || plugin_enabled(&s) || has_our_hooks(&s));
    let mcp_uses = read_json(&mcp_path(&other)).is_ok_and(|m| has_our_mcp_server(&m));
    settings_uses || mcp_uses
}

/// Undo `install`. Plugin mode: remove `enabledPlugins["agora@agora"]`, remove
/// `extraKnownMarketplaces.agora` only if its path is our marketplace dir, delete
/// `marketplace_dir` recursively (only that directory, and not while the other scope still uses
/// it, see `other_scope_uses_agora`). Entries mode: remove `mcpServers.agora`
/// only if its `command` contains `agora`, and OURS hooks. Idempotent: a second run reports
/// `unchanged` / `absent`.
pub fn uninstall(o: &Options) -> Result<Vec<Action>, CliError> {
    match o.mode {
        Mode::Plugin => uninstall_plugin(o),
        Mode::Entries => uninstall_entries(o),
    }
}

fn uninstall_plugin(o: &Options) -> Result<Vec<Action>, CliError> {
    let market = marketplace_dir(o);
    let market_str = utf8_path(&market)?;
    let settings = edit_existing(&settings_path(o), o.dry_run, |doc| {
        let changed = unset_plugin(root_mut(doc)?, market_str);
        Ok(changed.then(|| "enabledPlugins and extraKnownMarketplaces entries removed".to_owned()))
    })?;
    let dir = if other_scope_uses_agora(o) {
        action(
            &market,
            "kept",
            format!(
                "still used by the {} scope",
                scope_name(other_scope(o.scope))
            ),
        )
    } else {
        remove_marketplace(&market, o.dry_run)?
    };
    Ok(vec![settings, dir])
}

fn uninstall_entries(o: &Options) -> Result<Vec<Action>, CliError> {
    let mcp = edit_existing(&mcp_path(o), o.dry_run, |doc| {
        let removed = remove_mcp_server(root_mut(doc)?);
        Ok(removed.then(|| format!("mcpServers.{SERVER} removed")))
    })?;
    let settings = edit_existing(&settings_path(o), o.dry_run, |doc| {
        let n = remove_hooks(doc);
        Ok((n > 0).then(|| format!("hooks removed={n}")))
    })?;
    Ok(vec![mcp, settings])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_opts(root: &Path, scope: Scope, mode: Mode, dry_run: bool) -> Options {
        Options {
            scope,
            mode,
            dry_run,
            agora_home: root.join("x'y"),
            user_home: root.join("user"),
            project_dir: root.join("project"),
            exe: PathBuf::from("/opt/agora bin/agora"),
        }
    }

    fn write_file(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, value.to_string()).expect("write");
    }

    fn file<'a>(files: &'a [(PathBuf, Vec<u8>)], rel: &str) -> &'a [u8] {
        files
            .iter()
            .find(|(p, _)| p.as_path() == Path::new(rel))
            .map(|(_, bytes)| bytes.as_slice())
            .expect("file materialized")
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(
            shell_quote("/opt/agora bin/agora"),
            "'/opt/agora bin/agora'"
        );
        assert_eq!(shell_quote("/tmp/x'y"), r"'/tmp/x'\''y'");
    }

    #[test]
    fn hook_command_quotes_exe_and_home() {
        let o = Options {
            agora_home: PathBuf::from("/tmp/x'y"),
            ..test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false)
        };
        assert_eq!(
            hook_command(&o, "rewake --quick"),
            r"'/opt/agora bin/agora' --home '/tmp/x'\''y' hook rewake --quick"
        );
    }

    #[test]
    fn is_our_hook_command_accepts_only_our_hook_suffixes() {
        for cmd in [
            "'/opt/agora bin/agora' --home '/h' hook session-start",
            "agora hook context",
            "  agora hook stop  ",
            "agora hook rewake",
            "agora hook rewake --quick",
        ] {
            assert!(is_our_hook_command(cmd), "{cmd}");
        }
        for cmd in [
            "echo agora",
            "agora hook contextual",
            "other hook context",
            "agora hook session-start --x",
            "agora hook rewake --quickly",
        ] {
            assert!(!is_our_hook_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn hook_entries_are_the_seven_plugin_handlers_with_absolute_commands() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false);
        let entries = hook_entries(&o).expect("entries");
        assert_eq!(entries.len(), 7);
        for (_, _, handler) in &entries {
            let cmd = command_of(handler).expect("command");
            assert!(cmd.starts_with("'/opt/agora bin/agora' --home '"), "{cmd}");
            assert!(is_our_hook_command(cmd), "{cmd}");
            assert_eq!(handler["type"], json!("command"));
        }
        let post = entries
            .iter()
            .find(|(event, _, _)| event.as_str() == "PostToolUse")
            .expect("PostToolUse");
        assert_eq!(
            post.1.as_deref(),
            Some("mcp__plugin_agora_agora__.*|mcp__agora__.*")
        );
        let rewake = entries
            .iter()
            .find(|(_, _, h)| command_of(h).is_some_and(|c| c.ends_with(" hook rewake")))
            .expect("rewake");
        assert_eq!(rewake.2["asyncRewake"], json!(true));
        assert_eq!(rewake.2["timeout"], json!(3600));
    }

    #[test]
    fn merge_hooks_is_idempotent_and_keeps_user_hooks() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false);
        let entries = hook_entries(&o).expect("entries");
        let user_stop = json!({"hooks": [{"type": "command", "command": "echo done"}]});
        let mut settings = json!({"hooks": {"Stop": [user_stop.clone()]}});
        assert_eq!(merge_hooks(&mut settings, &entries), (7, 0, 0));
        assert_eq!(settings["hooks"]["Stop"][0], user_stop);
        assert_eq!(merge_hooks(&mut settings, &entries), (0, 7, 0));
        assert_eq!(settings["hooks"]["Stop"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn merge_hooks_migrates_an_old_exe_in_place() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false);
        let entries = hook_entries(&o).expect("entries");
        let old = "'/old/agora' --home '/old/home' hook stop";
        let mut settings =
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": old}]}]}});
        assert_eq!(merge_hooks(&mut settings, &entries), (6, 0, 1));
        let stop = settings["hooks"]["Stop"].as_array().expect("Stop");
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            json!(hook_command(&o, "stop"))
        );
    }

    #[test]
    fn remove_hooks_keeps_user_hooks_and_drops_only_our_groups() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false);
        let entries = hook_entries(&o).expect("entries");
        let user_stop = json!({"hooks": [{"type": "command", "command": "echo done"}]});
        let mixed = json!({
            "matcher": "Bash",
            "hooks": [
                {"type": "command", "command": "echo mixed"},
                {"type": "command", "command": hook_command(&o, "context"), "timeout": 10},
            ],
        });
        let mut settings =
            json!({"hooks": {"Stop": [user_stop.clone()], "UserPromptSubmit": [mixed]}});
        assert_eq!(merge_hooks(&mut settings, &entries), (6, 1, 0));
        assert_eq!(remove_hooks(&mut settings), 7);
        assert_eq!(
            settings,
            json!({"hooks": {
                "Stop": [user_stop],
                "UserPromptSubmit": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo mixed"}]}],
            }})
        );
    }

    #[test]
    fn remove_hooks_drops_containers_it_emptied() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Entries, false);
        let entries = hook_entries(&o).expect("entries");
        let mut settings = json!({"theme": "dark"});
        merge_hooks(&mut settings, &entries);
        assert_eq!(remove_hooks(&mut settings), 7);
        assert_eq!(settings, json!({"theme": "dark"}));
    }

    #[test]
    fn materialize_substitutes_all_three_files() {
        let o = test_opts(Path::new("/unused"), Scope::User, Mode::Plugin, false);
        let files = materialize(&o).expect("materialize");
        let exe = "/opt/agora bin/agora";
        let home = o.agora_home.to_str().expect("utf8").to_owned();

        let hooks: Value =
            serde_json::from_slice(file(&files, "agora/hooks/hooks.json")).expect("hooks.json");
        let mut commands = 0;
        for groups in hooks["hooks"].as_object().expect("hooks").values() {
            for group in groups.as_array().expect("groups") {
                for handler in group["hooks"].as_array().expect("handlers") {
                    let cmd = command_of(handler).expect("command");
                    assert!(cmd.starts_with("'/opt/agora bin/agora' --home '"), "{cmd}");
                    commands += 1;
                }
            }
        }
        assert_eq!(commands, 7);

        let mcp: Value =
            serde_json::from_slice(file(&files, "agora/.mcp.json")).expect(".mcp.json");
        let server = &mcp["mcpServers"]["agora"];
        assert_eq!(server["command"], json!(exe));
        assert_eq!(server["args"], json!(["--home", home, "mcp"]));
        assert_eq!(server["env"]["AGORA_HOME"], json!(home));
        assert_eq!(server["env"]["MCP_PROTOCOL_NEGOTIATION"], json!("legacy"));
        assert_eq!(
            server["env"]["AGORA_FOLDER"],
            json!("${CLAUDE_PROJECT_DIR}")
        );

        let js = String::from_utf8(file(&files, "agora/hooks/register.js").to_vec()).expect("utf8");
        let line = js
            .lines()
            .find(|l| l.starts_with("const AGORA = "))
            .expect("AGORA line");
        let literal = line
            .strip_prefix("const AGORA = ")
            .and_then(|rest| rest.strip_suffix(';'))
            .expect("array literal");
        assert_eq!(
            serde_json::from_str::<Value>(literal).expect("valid JSON array"),
            json!([exe, "--home", home])
        );

        let market: Value = serde_json::from_slice(file(&files, ".claude-plugin/marketplace.json"))
            .expect("marketplace.json");
        assert_eq!(market["plugins"][0]["source"], json!("./agora"));
        assert!(
            files
                .iter()
                .any(|(p, _)| p.as_path() == Path::new("agora/.claude-plugin/plugin.json"))
        );
    }

    #[test]
    fn register_js_without_the_agora_line_is_refused() {
        let err = register_js("export function register() {}\n", "/a/agora", "/h")
            .expect_err("missing line");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn install_plugin_twice_second_run_is_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        let first = install(&o).expect("first install");
        assert!(first.iter().all(|a| a.result == "wrote"), "{first:?}");
        let second = install(&o).expect("second install");
        assert!(second.iter().all(|a| a.result == "unchanged"), "{second:?}");
    }

    #[test]
    fn uninstall_plugin_restores_settings_and_removes_marketplace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        let original = json!({"theme": "dark", "enabledPlugins": {"other@x": true}});
        write_file(&settings_path(&o), &original);

        install(&o).expect("install");
        let installed = read_json(&settings_path(&o)).expect("settings");
        assert_eq!(installed["enabledPlugins"]["agora@agora"], json!(true));
        assert!(marketplace_dir(&o).is_dir());

        let actions = uninstall(&o).expect("uninstall");
        assert!(actions.iter().any(|a| a.result == "removed"), "{actions:?}");
        assert!(!marketplace_dir(&o).exists());
        assert_eq!(read_json(&settings_path(&o)).expect("settings"), original);

        let again = uninstall(&o).expect("second uninstall");
        assert!(
            again
                .iter()
                .all(|a| a.result == "unchanged" || a.result == "absent"),
            "{again:?}"
        );
    }

    #[test]
    fn entries_round_trip_keeps_user_mcp_server_and_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Entries, false);
        let user_mcp = json!({
            "numStartups": 3,
            "mcpServers": {"user": {"type": "stdio", "command": "node", "args": ["s.js"]}},
        });
        let user_settings =
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}});
        write_file(&mcp_path(&o), &user_mcp);
        write_file(&settings_path(&o), &user_settings);

        install(&o).expect("install");
        let mcp = read_json(&mcp_path(&o)).expect("mcp");
        assert_eq!(
            mcp["mcpServers"]["agora"]["command"],
            json!("/opt/agora bin/agora")
        );
        assert_eq!(mcp["mcpServers"]["user"], user_mcp["mcpServers"]["user"]);
        let settings = read_json(&settings_path(&o)).expect("settings");
        assert_eq!(settings["hooks"]["Stop"].as_array().map(Vec::len), Some(3));

        uninstall(&o).expect("uninstall");
        assert_eq!(read_json(&mcp_path(&o)).expect("mcp"), user_mcp);
        assert_eq!(
            read_json(&settings_path(&o)).expect("settings"),
            user_settings
        );
    }

    #[test]
    fn plugin_install_warns_when_entries_hooks_are_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = test_opts(dir.path(), Scope::User, Mode::Entries, false);
        install(&entries).expect("entries install");
        let plugin = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        let actions = install(&plugin).expect("plugin install");
        let settings = settings_path(&plugin).display().to_string();
        let action = actions
            .iter()
            .find(|a| a.path == settings)
            .expect("settings action");
        assert!(
            action
                .detail
                .contains("entries-mode hooks are also installed")
        );
    }

    #[test]
    fn plugin_install_refuses_a_non_object_container_before_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        write_file(&settings_path(&o), &json!({"enabledPlugins": []}));
        let err = install(&o).expect_err("non-object enabledPlugins");
        assert!(err.to_string().contains("enabledPlugins"), "{err}");
        assert!(!marketplace_dir(&o).exists());
    }

    #[test]
    fn dry_run_writes_nothing() {
        for mode in [Mode::Plugin, Mode::Entries] {
            let dir = tempfile::tempdir().expect("tempdir");
            let o = test_opts(dir.path(), Scope::User, mode, true);
            let actions = install(&o).expect("dry-run install");
            assert!(!actions.is_empty());
            assert!(actions.iter().all(|a| a.result == "dry_run"), "{actions:?}");
            assert!(!settings_path(&o).exists());
            assert!(!mcp_path(&o).exists());
            assert!(!marketplace_dir(&o).exists());
        }
    }

    #[test]
    fn uninstall_keeps_marketplace_while_the_other_scope_uses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        let project = test_opts(dir.path(), Scope::Project, Mode::Plugin, false);
        install(&user).expect("user install");
        install(&project).expect("project install");
        let user_settings = read_json(&settings_path(&user)).expect("user settings");

        let actions = uninstall(&project).expect("project uninstall");
        let market = marketplace_dir(&project).display().to_string();
        let kept = actions
            .iter()
            .find(|a| a.path == market)
            .expect("marketplace action");
        assert_eq!(kept.result, "kept");
        assert_eq!(kept.detail, "still used by the user scope");
        assert!(marketplace_dir(&user).is_dir());
        assert_eq!(
            read_json(&settings_path(&user)).expect("user settings"),
            user_settings
        );

        uninstall(&user).expect("user uninstall");
        assert!(!marketplace_dir(&user).exists());
    }

    #[test]
    fn other_scope_uses_agora_sees_entries_mode_in_the_other_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = test_opts(dir.path(), Scope::Project, Mode::Entries, false);
        let user = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        install(&entries).expect("entries install");
        assert!(other_scope_uses_agora(&user));
        uninstall(&entries).expect("entries uninstall");
        assert!(!other_scope_uses_agora(&user));
    }

    #[test]
    fn entries_install_warns_when_plugin_mode_is_also_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = settings_path(&test_opts(dir.path(), Scope::User, Mode::Entries, false))
            .display()
            .to_string();
        let warning =
            "plugin-mode install also present; run `agora uninstall --client claude --mode plugin`";

        let alone = install(&test_opts(dir.path(), Scope::User, Mode::Entries, false))
            .expect("entries install");
        let detail = |actions: &[Action]| {
            actions
                .iter()
                .find(|a| a.path == settings)
                .expect("settings action")
                .detail
                .clone()
        };
        assert!(!detail(&alone).contains(warning), "{alone:?}");

        install(&test_opts(dir.path(), Scope::User, Mode::Plugin, false)).expect("plugin install");
        let both = install(&test_opts(dir.path(), Scope::User, Mode::Entries, false))
            .expect("entries install again");
        assert!(detail(&both).contains(warning), "{both:?}");
    }

    #[test]
    fn settings_failure_after_mcp_write_names_the_mcp_file_and_backup() {
        let mcp = PathBuf::from("/h/.claude.json");
        let original = CliError::Usage("disk full".to_owned()).to_string();

        let wrote = WriteResult::Wrote {
            backup: Some(PathBuf::from("/h/.claude.json.agora-bak-1")),
        };
        let text =
            after_mcp_write(CliError::Usage("disk full".to_owned()), &mcp, &wrote).to_string();
        assert!(
            text.contains("/h/.claude.json was already written"),
            "{text}"
        );
        assert!(
            text.contains("(backup /h/.claude.json.agora-bak-1)"),
            "{text}"
        );
        assert!(text.contains("disk full"), "{text}");

        let no_backup = WriteResult::Wrote { backup: None };
        let text =
            after_mcp_write(CliError::Usage("disk full".to_owned()), &mcp, &no_backup).to_string();
        assert!(
            text.contains("/h/.claude.json was already written"),
            "{text}"
        );
        assert!(!text.contains("backup"), "{text}");

        let unchanged = after_mcp_write(
            CliError::Usage("disk full".to_owned()),
            &mcp,
            &WriteResult::Unchanged,
        );
        assert_eq!(unchanged.to_string(), original);
    }

    fn detail_at<'a>(actions: &'a [Action], path: &Path) -> &'a str {
        let path = path.display().to_string();
        actions
            .iter()
            .find(|a| a.path == path)
            .expect("action for file")
            .detail
            .as_str()
    }

    #[test]
    fn entries_install_warns_when_it_replaces_a_foreign_mcp_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Entries, false);
        write_file(
            &mcp_path(&o),
            &json!({"mcpServers": {"agora": {"command": "node", "args": ["x.js"]}}}),
        );
        let actions = install(&o).expect("entries install");
        let detail = detail_at(&actions, &mcp_path(&o));
        assert!(detail.contains(FOREIGN_SERVER_WARNING), "{detail}");
        assert!(detail.contains("backup "), "{detail}");
    }

    #[test]
    fn entries_install_is_silent_over_an_older_agora_mcp_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Entries, false);
        write_file(
            &mcp_path(&o),
            &json!({"mcpServers": {"agora": {"command": "/old/agora", "args": ["mcp"]}}}),
        );
        let actions = install(&o).expect("entries install");
        let detail = detail_at(&actions, &mcp_path(&o));
        assert!(!detail.contains("replaced an existing"), "{detail}");
    }

    #[test]
    fn plugin_install_warns_when_it_replaces_a_foreign_marketplace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        write_file(
            &settings_path(&o),
            &json!({"extraKnownMarketplaces": {"agora": {"source": {"source": "github", "repo": "someone/agora"}}}}),
        );
        let actions = install(&o).expect("plugin install");
        let detail = detail_at(&actions, &settings_path(&o));
        assert!(detail.contains(FOREIGN_MARKETPLACE_WARNING), "{detail}");
    }

    #[test]
    fn plugin_install_is_silent_over_an_older_agora_marketplace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = test_opts(dir.path(), Scope::User, Mode::Plugin, false);
        let older =
            json!({"source": {"source": "directory", "path": "/old/home/claude/marketplace"}});
        write_file(
            &settings_path(&o),
            &json!({"extraKnownMarketplaces": {"agora": older}}),
        );
        let actions = install(&o).expect("plugin install");
        let detail = detail_at(&actions, &settings_path(&o));
        assert!(!detail.contains("replaced an existing"), "{detail}");
    }
}
