//! `$AGORA_HOME/capabilities.json` (mode 0600): what `agora install` found and set up for each
//! harness, read by the MCP shim to decide which delivery rungs belong in the ladder.
//!
//! Shape: `{"schema":1,"claude":{"probed_at_ms":..,"claude_bin":"..","claude_version":"2.1.293",
//! "mods":true,"channels_flag":true,"async_rewake":true,"stop_hook":true,"context_hook":true,
//! "mcp_negotiation_legacy":true,"install_mode":"plugin","scope":"user"}, ...}`. Unknown
//! top-level keys are preserved on a load/save round trip (`CapabilitiesFile::other`). Per-harness
//! entries (`HarnessCapabilities`) live under top-level keys equal to `Harness::as_str()` (codex,
//! copilot_cli, copilot_vscode, antigravity, opencode, cursor).

use agora_daemon::Paths;
use agora_deliver::Harness;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest file `load` reads; bigger files are treated as absent.
const MAX_BYTES: u64 = 64 * 1024;

/// Capabilities of the Claude Code installation (all fields default when missing).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCapabilities {
    pub probed_at_ms: i64,
    pub claude_bin: Option<String>,
    pub claude_version: Option<String>,
    /// The plugin mod (`$.prompt.submit`) is installed.
    pub mods: bool,
    /// `--channels` / `--dangerously-load-development-channels` exist.
    pub channels_flag: bool,
    /// The `asyncRewake` hooks are installed.
    pub async_rewake: bool,
    /// The Stop hook is installed.
    pub stop_hook: bool,
    /// The context hooks (UserPromptSubmit, PostToolUse, PostToolBatch) are installed.
    pub context_hook: bool,
    pub mcp_negotiation_legacy: bool,
    /// `plugin` or `entries`.
    pub install_mode: Option<String>,
    /// `user` or `project`.
    pub scope: Option<String>,
}

/// What `agora install --client <c>` set up for one non-Claude harness. Stored in
/// `CapabilitiesFile::other` under the key `Harness::as_str()` (`codex`, `copilot_cli`,
/// `copilot_vscode`, `antigravity`, `opencode`, `cursor`). All fields default when missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessCapabilities {
    /// 0 without `--probe`.
    pub probed_at_ms: i64,
    pub bin: Option<String>,
    pub version: Option<String>,
    /// Installed scopes: "user" or "project:<absolute dir>", sorted, deduplicated.
    pub scopes: Vec<String>,
    pub mcp_configured: bool,
    pub context_hook: bool,
    pub stop_hook: bool,
    /// Codex only: Some(false) = hooks written but not trusted yet (codex skips them until `/hooks` review
    /// or `--trust-hooks`); Some(true) = trust entries written; None = not applicable.
    pub hooks_trusted: Option<bool>,
    /// The binary supports the owned-session bridge (`agora run-agent`): codex app-server, copilot --acp,
    /// opencode serve, agy -p --conversation.
    pub owned_session: bool,
    /// Absolute paths of every file this install wrote or edited (config, hooks, instruction files).
    /// Uninstall of another client consults this to keep shared files (AGENTS.md block, the Copilot hooks file).
    pub files: Vec<String>,
    pub notes: Vec<String>,
}

/// The whole file. `claude` is absent until `agora install --client claude` ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilitiesFile {
    pub schema: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeCapabilities>,
    /// Unknown top-level keys, preserved.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Default for CapabilitiesFile {
    /// `schema: 1`, no `claude`, no other keys.
    fn default() -> Self {
        Self {
            schema: 1,
            claude: None,
            other: serde_json::Map::new(),
        }
    }
}

/// Whether `h` keeps its install record under `other[h.as_str()]`. Claude has its own typed field,
/// and `Unknown` names no client, so neither does.
fn has_entry(h: Harness) -> bool {
    !matches!(h, Harness::ClaudeCode | Harness::Unknown)
}

impl CapabilitiesFile {
    /// `other[h.as_str()]` deserialized; None for ClaudeCode/Unknown, absent, or not parseable.
    /// The raw value stays in `other` either way, so an unparseable entry survives a save.
    pub fn harness(&self, h: Harness) -> Option<HarnessCapabilities> {
        if !has_entry(h) {
            return None;
        }
        let value = self.other.get(h.as_str())?;
        serde_json::from_value(value.clone()).ok()
    }

    /// Set (Some) or remove (None) `other[h.as_str()]`. No-op for ClaudeCode/Unknown.
    /// Replaces the whole entry: unknown keys nested inside an entry are not preserved.
    pub fn set_harness(&mut self, h: Harness, caps: Option<HarnessCapabilities>) {
        if !has_entry(h) {
            return;
        }
        match caps {
            Some(caps) => {
                // Serializing this struct cannot fail: every field is a plain scalar, string or
                // vector, so there is no map with non-string keys and no non-finite float.
                if let Ok(value) = serde_json::to_value(caps) {
                    self.other.insert(h.as_str().to_owned(), value);
                }
            }
            None => {
                self.other.remove(h.as_str());
            }
        }
    }

    /// True when any harness entry other than `except` lists `path` in `files`
    /// (Claude: never - Claude does not share files with other clients).
    pub fn file_shared(&self, path: &str, except: Harness) -> bool {
        Harness::ALL
            .into_iter()
            .filter(|h| *h != except)
            .filter_map(|h| self.harness(h))
            .any(|caps| caps.files.iter().any(|f| f == path))
    }
}

/// File name under `$AGORA_HOME`.
pub const FILE_NAME: &str = "capabilities.json";

/// `paths.home/capabilities.json`.
pub fn path(paths: &Paths) -> PathBuf {
    paths.home.join(FILE_NAME)
}

/// Read the file. Missing, unparsable, larger than 64 KiB, or a symlink -> `None`.
pub fn load(paths: &Paths) -> Option<CapabilitiesFile> {
    let target = path(paths);
    let meta = std::fs::symlink_metadata(&target).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    // O_NOFOLLOW: a symlink swapped in after the metadata check fails to open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&target)
        .ok()?;
    let mut bytes = Vec::new();
    // Bounded read: the file may have grown since the metadata check.
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Atomically write the file (temp file + rename in the same directory, mode 0600).
/// `paths.home` must be an existing directory (`NotFound` otherwise), and an existing
/// `capabilities.json` that is a symlink is refused (`InvalidInput`), never followed.
pub fn save(paths: &Paths, file: &CapabilitiesFile) -> std::io::Result<()> {
    if !paths.home.is_dir() {
        return Err(ErrorKind::NotFound.into());
    }
    let target = path(paths);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "refusing to write capabilities.json through a symlink",
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut bytes = serde_json::to_vec_pretty(file)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "capabilities.json would exceed 64 KiB",
        ));
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = paths
        .home
        .join(format!(".{FILE_NAME}.tmp-{}-{nanos}", std::process::id()));
    // create_new: if the temp name is somehow taken, fail without touching that file.
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = out.write_all(&bytes).and_then(|()| out.sync_all());
    drop(out);
    let result = written.and_then(|()| std::fs::rename(&tmp, &target));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests;
