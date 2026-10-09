//! Format-preserving edits of the Codex `config.toml` with `toml_edit`: comments, key order and
//! unrelated tables are preserved byte for byte. Codex itself writes `[mcp_servers.<name>]` through
//! the same TOML document (`codex mcp add`), so the MCP entry and the `[hooks.state."<key>"]` trust
//! entries (VERIFIED-LOCAL against Codex 0.157.1, see `codex_trust`) live in the user config next to
//! the user's own tables.

use super::json_merge::{ServerChange, is_agora_exe};
use crate::CliError;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

/// Name used in error messages, so the caller's report points at the right file.
const CONFIG: &str = "config.toml";

/// Set `[mcp_servers.<name>]` `command` and `args`, and `default_tools_approval_mode = "approve"` when
/// `approve` is Some (otherwise leave that key as it is). An existing agora entry keeps the user's own
/// keys (`startup_timeout_sec`, ...). An existing table-like entry that is not agora's is removed whole
/// (its `url`, `env` subtable, ... included) and replaced: `ReplacedForeign`. A scalar `name = ...`
/// value is not ours to remove and stays an error. Returns the new document text and what changed.
/// A TOML parse error or a non-table `mcp_servers` -> `CliError::Usage`; the caller names the file.
pub fn set_mcp_server(
    doc: &str,
    name: &str,
    command: &str,
    args: &[String],
    approve: Option<&str>,
) -> Result<(String, ServerChange), CliError> {
    let mut parsed = parse(doc)?;
    let servers = child(parsed.as_table_mut(), "mcp_servers", true)?;
    let previous = servers.get(name).map(server_is_ours);
    let foreign_table = previous == Some(false)
        && servers
            .get(name)
            .is_some_and(|item| item.as_table_like().is_some());
    if foreign_table {
        servers.remove(name);
    }
    let entry = child(servers, name, false)?;
    put(entry, "command", Value::from(command));
    let mut list = Array::new();
    for arg in args {
        list.push(arg.as_str());
    }
    put(entry, "args", Value::Array(list));
    if let Some(mode) = approve {
        put(entry, "default_tools_approval_mode", Value::from(mode));
    }
    let text = parsed.to_string();
    let change = match previous {
        None => ServerChange::Added,
        Some(false) => ServerChange::ReplacedForeign,
        Some(true) if text == doc => ServerChange::Unchanged,
        Some(true) => ServerChange::Updated,
    };
    Ok((text, change))
}

/// Remove `[mcp_servers.<name>]` only if its `command` names an agora executable (see `server_is_ours`);
/// drop an emptied `mcp_servers` table. `Ok(None)`: nothing to remove.
pub fn remove_mcp_server(doc: &str, name: &str) -> Result<Option<String>, CliError> {
    let mut parsed = parse(doc)?;
    let root = parsed.as_table_mut();
    let removed = match root
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    {
        Some(servers) if servers.get(name).is_some_and(server_is_ours) => {
            servers.remove(name);
            true
        }
        _ => false,
    };
    if !removed {
        return Ok(None);
    }
    prune_implicit(root, "mcp_servers");
    Ok(Some(parsed.to_string()))
}

/// Write `[hooks.state."<key>"] trusted_hash = "<hash>"` for each `(key, hash)`, keeping the other keys
/// of that table. Returns the new text (identical to `doc` when nothing changes).
pub fn set_trust(doc: &str, entries: &[(String, String)]) -> Result<String, CliError> {
    let mut parsed = parse(doc)?;
    let root = parsed.as_table_mut();
    for (key, hash) in entries {
        let hooks = child(root, "hooks", true)?;
        let state = child(hooks, "state", true)?;
        let entry = child(state, key, false)?;
        put(entry, "trusted_hash", Value::from(hash.as_str()));
    }
    Ok(parsed.to_string())
}

/// Remove the `hooks.state` tables named by `keys`; prune emptied `state` / `hooks` tables.
/// `Ok(None)`: none of the keys existed.
pub fn remove_trust(doc: &str, keys: &[String]) -> Result<Option<String>, CliError> {
    let mut parsed = parse(doc)?;
    let root = parsed.as_table_mut();
    let Some(hooks) = root.get_mut("hooks").and_then(Item::as_table_like_mut) else {
        return Ok(None);
    };
    let mut changed = false;
    if let Some(state) = hooks.get_mut("state").and_then(Item::as_table_like_mut) {
        for key in keys {
            changed |= drop_trust(state, key);
        }
    }
    if !changed {
        return Ok(None);
    }
    prune_implicit(hooks, "state");
    prune_implicit(root, "hooks");
    Ok(Some(parsed.to_string()))
}

/// Parse the Codex config; a syntax error names the file and the parser's message.
fn parse(doc: &str) -> Result<DocumentMut, CliError> {
    doc.parse::<DocumentMut>()
        .map_err(|e| CliError::Usage(format!("{CONFIG} is not valid TOML: {e}")))
}

/// `parent[key]` as a table, created when missing. `implicit` tables get no header of their own
/// (`[mcp_servers.agora]` is written, not an empty `[mcp_servers]`). An existing non-table is an error.
fn child<'a>(
    parent: &'a mut dyn TableLike,
    key: &str,
    implicit: bool,
) -> Result<&'a mut dyn TableLike, CliError> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(implicit);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| CliError::Usage(format!("{CONFIG}: `{key}` is not a table")))
}

/// Set `table[key]` to `value`, keeping the comment and whitespace around the old value
/// (`command = "x"  # note` keeps its note). An old value equal to `value` is left as it is, quoting
/// included (`command = '...'` stays single-quoted). The key itself and every other key are untouched.
fn put(table: &mut dyn TableLike, key: &str, mut value: Value) {
    if let Some(old) = table.get_mut(key).and_then(Item::as_value_mut) {
        if same_value(old, &value) {
            return;
        }
        *value.decor_mut() = old.decor().clone();
        *old = value;
        return;
    }
    table.insert(key, Item::Value(value));
}

/// True for the same string, or an array of the same strings in the same order (decor ignored).
fn same_value(old: &Value, new: &Value) -> bool {
    match (old, new) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| same_value(x, y))
        }
        _ => false,
    }
}

/// Drop `parent[key]` when it is an implicit table that is now empty (created by agora, not the user).
fn prune_implicit(parent: &mut dyn TableLike, key: &str) {
    let empty_implicit = parent
        .get(key)
        .and_then(Item::as_table)
        .is_some_and(|table| table.is_implicit() && table.is_empty());
    if empty_implicit {
        parent.remove(key);
    }
}

/// The `command` string of an MCP server entry, when the entry is a table with one.
fn command_of(item: &Item) -> Option<&str> {
    item.as_table_like()?.get("command")?.as_str()
}

/// True when the server entry's `command` names an agora executable (an entry agora wrote). The rule
/// is `json_merge::is_agora_exe`: the file name starts with `agora`, so a directory called `agora` does not count.
fn server_is_ours(item: &Item) -> bool {
    command_of(item).is_some_and(is_agora_exe)
}

/// Remove agora's trust for `key`: the whole `state."<key>"` table when `trusted_hash` is its only
/// key, else only the `trusted_hash` key (the user's other keys, e.g. `enabled`, stay). True when
/// something was removed.
fn drop_trust(state: &mut dyn TableLike, key: &str) -> bool {
    let Some(entry) = state.get_mut(key).and_then(Item::as_table_like_mut) else {
        return false;
    };
    if !entry.contains_key("trusted_hash") {
        return false;
    }
    if entry.len() == 1 {
        state.remove(key);
    } else {
        entry.remove("trusted_hash");
    }
    true
}

#[cfg(test)]
mod tests;
