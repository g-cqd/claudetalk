//! Pure `serde_json::Value` merging for the JSON client configs (MCP entries and hook groups):
//! replace what is ours in place, never touch anything else, prune what we emptied.

use crate::CliError;
use serde_json::{Map, Value, json};
use std::path::Path;

/// What `set_server` / `set_named` (and `codex_toml::set_mcp_server`) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerChange {
    Added,
    Updated,
    Unchanged,
    /// An existing entry that was not ours (its command does not name an agora executable, see
    /// `is_agora_exe`) was overwritten; the caller adds a warning (the file backup keeps the old entry).
    ReplacedForeign,
}

const HOOKS: &str = "hooks";

/// The hook invocations agora writes; a command must contain one of them to be ours.
const OUR_HOOK_INVOCATIONS: [&str; 3] = [
    " hook session-start --dialect ",
    " hook context --dialect ",
    " hook stop --dialect ",
];

/// True for a hook command line agora wrote: the trimmed command contains ` --home ` AND one of
/// `OUR_HOOK_INVOCATIONS`. A user hook that merely mentions agora is not ours.
pub fn is_our_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    cmd.contains(" --home ") && OUR_HOOK_INVOCATIONS.iter().any(|inv| cmd.contains(inv))
}

/// True when the executable of a server `command` is an agora binary: its file name, with the
/// directory and any surrounding single quotes removed, starts with `agora`. So `/opt/agora/bin/agora`
/// is ours and `/Users/agora/bin/my-mcp` is not.
pub fn is_agora_exe(cmd: &str) -> bool {
    let cmd = cmd.trim();
    let cmd = cmd
        .strip_prefix('\'')
        .and_then(|c| c.strip_suffix('\''))
        .unwrap_or(cmd);
    Path::new(cmd)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("agora"))
}

/// `Err` (naming `path`) when `root[key]` exists and is not a JSON object; used to validate every
/// existing file BEFORE any write.
pub fn check_object_member(root: &Value, key: &str, path: &Path) -> Result<(), CliError> {
    match root.get(key) {
        Some(value) if !value.is_object() => Err(CliError::Usage(format!(
            "{}: \"{key}\" is not a JSON object; fix it, nothing was written",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// `Err` (naming `path` and the event) when `root.hooks[e]` exists and is not a JSON array, for any
/// `e` in `events`; used to validate every existing file BEFORE any write, so a bad event is never
/// skipped silently. A `hooks` member that is not an object is left to `check_object_member`.
pub fn check_event_arrays(root: &Value, events: &[&str], path: &Path) -> Result<(), CliError> {
    let Some(hooks) = root.get(HOOKS).and_then(Value::as_object) else {
        return Ok(());
    };
    for e in events {
        if hooks.get(*e).is_some_and(|value| !value.is_array()) {
            return Err(CliError::Usage(format!(
                "{}: hooks.{e} is not an array; fix it, nothing was written",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Set `root[container_key][name] = entry` (creating the container object). Existing entry equal ->
/// `Unchanged`; ours (`is_server_ours`: the `command` string, or for opencode the first element of a
/// `command` array, names an agora executable) -> `Updated`; otherwise `ReplacedForeign`; absent ->
/// `Added`. `root` must be an object.
/// When `root` or `root[container_key]` is not an object, nothing is written and the result is
/// `ReplacedForeign`; the installers call `check_object_member` first, so this is a guard only.
pub fn set_server(root: &mut Value, container_key: &str, name: &str, entry: Value) -> ServerChange {
    let Some(container) = object_member_mut(root, container_key) else {
        return ServerChange::ReplacedForeign;
    };
    let change = match container.get(name) {
        None => ServerChange::Added,
        Some(existing) if *existing == entry => return ServerChange::Unchanged,
        Some(existing) if is_server_ours(existing) => ServerChange::Updated,
        Some(_) => ServerChange::ReplacedForeign,
    };
    container.insert(name.to_owned(), entry);
    change
}

/// Remove `root[container_key][name]` only if ours (same test as `set_server`); drops the container
/// when that leaves it empty. True when something was removed.
pub fn remove_server(root: &mut Value, container_key: &str, name: &str) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let Some(container) = obj.get_mut(container_key).and_then(Value::as_object_mut) else {
        return false;
    };
    if !container.get(name).is_some_and(is_server_ours) {
        return false;
    }
    container.remove(name);
    let now_empty = container.is_empty();
    if now_empty {
        obj.remove(container_key);
    }
    true
}

/// Claude-style grouped hooks (Codex `hooks.json`): for each `(event, matcher, handler)` put the
/// handler into `root.hooks[event]` (a group `{"matcher"?, "hooks":[handler]}`), replacing OUR
/// existing handler in place (`is_our_command`), never touching user groups. Returns the number of
/// handlers added or changed.
///
/// An entry whose command is already present (as ours, in that event) changes nothing. An our
/// handler in a group with the same matcher is replaced only when its command is not one this
/// call writes for the same event, so two of our handlers in one event never replace each other.
pub fn merge_grouped_hooks(root: &mut Value, entries: &[(&str, Option<&str>, Value)]) -> usize {
    let mut changed = 0;
    for (event, matcher, handler) in entries {
        let Some(command) = command_of(handler) else {
            continue;
        };
        let Some(hooks) = object_member_mut(root, HOOKS) else {
            return changed;
        };
        let wanted = wanted_commands(entries.iter().map(|(e, _, h)| (*e, h)), event);
        let Some(groups) = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
        else {
            continue;
        };
        if groups
            .iter()
            .any(|g| has_ours_command(handlers(g), command))
        {
            continue;
        }
        if !replace_in_groups(groups, *matcher, &wanted, handler) {
            groups.push(new_group(*matcher, handler));
        }
        changed += 1;
    }
    changed
}

/// Remove OUR handlers from every group under `root.hooks`; prune emptied groups, events and the
/// `hooks` object. Returns the number removed.
pub fn remove_grouped_hooks(root: &mut Value) -> usize {
    let Some(obj) = root.as_object_mut() else {
        return 0;
    };
    let Some(hooks) = obj.get_mut(HOOKS).and_then(Value::as_object_mut) else {
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
    if removed > 0 && hooks.is_empty() {
        obj.remove(HOOKS);
    }
    removed
}

/// Flat hooks (Cursor `hooks.json`): `root.hooks[event]` is an array of handlers; replace OUR
/// handler in place or append; ensure `root.version == 1` when anything is added. Returns the number
/// of handlers added or changed.
///
/// Same rules as `merge_grouped_hooks`, matched by handler `command`. An existing `version` is left
/// as it is.
pub fn merge_flat_hooks(root: &mut Value, entries: &[(&str, Value)]) -> usize {
    let mut changed = 0;
    for (event, handler) in entries {
        let Some(command) = command_of(handler) else {
            continue;
        };
        let Some(hooks) = object_member_mut(root, HOOKS) else {
            return changed;
        };
        let wanted = wanted_commands(entries.iter().map(|(e, h)| (*e, h)), event);
        let Some(list) = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
        else {
            continue;
        };
        if has_ours_command(list, command) {
            continue;
        }
        match list.iter_mut().find(|h| replaceable(h, &wanted)) {
            Some(slot) => *slot = handler.clone(),
            None => list.push(handler.clone()),
        }
        changed += 1;
    }
    if changed > 0
        && let Some(obj) = root.as_object_mut()
    {
        obj.entry("version").or_insert_with(|| json!(1));
    }
    changed
}

/// Remove OUR handlers from the flat arrays; prune emptied events and `hooks`. Returns the number removed.
///
/// When that leaves the object with only `version: 1` (the value we write), `version` is removed
/// too, so uninstalling from a file we created leaves `{}`. Other content keeps `version`.
pub fn remove_flat_hooks(root: &mut Value) -> usize {
    let Some(obj) = root.as_object_mut() else {
        return 0;
    };
    let Some(hooks) = obj.get_mut(HOOKS).and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied = Vec::new();
    for (event, value) in hooks.iter_mut() {
        let Some(list) = value.as_array_mut() else {
            continue;
        };
        let before = list.len();
        list.retain(|h| !is_ours_handler(h));
        let n = before - list.len();
        removed += n;
        if n > 0 && list.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in &emptied {
        hooks.remove(event);
    }
    if removed == 0 {
        return 0;
    }
    if hooks.is_empty() {
        obj.remove(HOOKS);
    }
    if obj.len() == 1 && obj.get("version") == Some(&json!(1)) {
        obj.remove("version");
    }
    removed
}

/// agy named hooks: `root[name] = def`. An existing equal value -> `Unchanged`; an existing value
/// whose handler commands are ours -> `Updated`; any other existing value -> `ReplacedForeign`;
/// absent -> `Added`. "Ours" here: some command string inside the value satisfies `is_our_command`.
/// `root` must be an object; otherwise nothing is written and the result is `ReplacedForeign`.
pub fn set_named(root: &mut Value, name: &str, def: Value) -> ServerChange {
    let Some(obj) = root.as_object_mut() else {
        return ServerChange::ReplacedForeign;
    };
    let change = match obj.get(name) {
        None => ServerChange::Added,
        Some(existing) if *existing == def => return ServerChange::Unchanged,
        Some(existing) if command_strings(existing).iter().any(|c| is_our_command(c)) => {
            ServerChange::Updated
        }
        Some(_) => ServerChange::ReplacedForeign,
    };
    obj.insert(name.to_owned(), def);
    change
}

/// Remove `root[name]` only if ours (every handler command is ours). True when removed.
/// A value with no command strings is never ours, so a user's own `name` entry is kept.
pub fn remove_named(root: &mut Value, name: &str) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    if !obj.get(name).is_some_and(all_commands_ours) {
        return false;
    }
    obj.remove(name).is_some()
}

/// `root[key]` as an object, created when missing. `None` when `root` or the member is not an
/// object, so nothing is overwritten.
fn object_member_mut<'a>(root: &'a mut Value, key: &str) -> Option<&'a mut Map<String, Value>> {
    root.as_object_mut()?
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

/// Ours by server rules: the `command` string, or the first element of a `command` array, names an
/// agora executable (`is_agora_exe`).
fn is_server_ours(entry: &Value) -> bool {
    match entry.get("command") {
        Some(Value::String(cmd)) => is_agora_exe(cmd),
        Some(Value::Array(argv)) => argv
            .first()
            .and_then(Value::as_str)
            .is_some_and(is_agora_exe),
        _ => false,
    }
}

fn command_of(handler: &Value) -> Option<&str> {
    handler.get("command").and_then(Value::as_str)
}

/// A flat or grouped handler whose command is one agora writes.
fn is_ours_handler(handler: &Value) -> bool {
    command_of(handler).is_some_and(is_our_command)
}

/// An our handler whose command is not in `wanted`, so it is safe to replace with a new one.
fn replaceable(handler: &Value, wanted: &[&str]) -> bool {
    command_of(handler).is_some_and(|c| is_our_command(c) && !wanted.contains(&c))
}

/// Commands this call writes for one event; our handlers with those commands are never replaced.
fn wanted_commands<'a>(
    entries: impl Iterator<Item = (&'a str, &'a Value)>,
    event: &str,
) -> Vec<&'a str> {
    entries
        .filter(|(e, _)| *e == event)
        .filter_map(|(_, h)| command_of(h))
        .collect()
}

fn has_ours_command(handlers: &[Value], command: &str) -> bool {
    handlers
        .iter()
        .any(|h| command_of(h) == Some(command) && is_ours_handler(h))
}

/// The handler list of a group, empty when the group has none.
fn handlers(group: &Value) -> &[Value] {
    match group.get("hooks").and_then(Value::as_array) {
        Some(list) => list.as_slice(),
        None => &[],
    }
}

fn group_matcher(group: &Value) -> Option<&str> {
    group.get("matcher").and_then(Value::as_str)
}

/// Replace the first replaceable our handler in a group with `matcher`; true when replaced.
fn replace_in_groups(
    groups: &mut [Value],
    matcher: Option<&str>,
    wanted: &[&str],
    handler: &Value,
) -> bool {
    for group in groups.iter_mut() {
        if group_matcher(group) != matcher {
            continue;
        }
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        if let Some(slot) = list.iter_mut().find(|h| replaceable(h, wanted)) {
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

/// Drop our handlers from `groups`; a group is dropped when it held ours and has none left.
/// Returns the number of handlers removed.
fn remove_ours_from_groups(groups: &mut Vec<Value>) -> usize {
    let mut removed = 0;
    groups.retain_mut(|group| {
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        list.retain(|h| !is_ours_handler(h));
        let gone = before - list.len();
        removed += gone;
        gone == 0 || !list.is_empty()
    });
    removed
}

/// Every command string inside `value` (the value of a `command` key at any depth).
fn command_strings(value: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    collect_commands(value, &mut out);
    out
}

fn collect_commands<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match child {
                    Value::String(cmd) if key == "command" => out.push(cmd.as_str()),
                    _ => collect_commands(child, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_commands(item, out)),
        _ => {}
    }
}

/// At least one command string, and every one of them is ours.
fn all_commands_ours(value: &Value) -> bool {
    let commands = command_strings(value);
    !commands.is_empty() && commands.iter().all(|c| is_our_command(c))
}

#[cfg(test)]
mod tests;
