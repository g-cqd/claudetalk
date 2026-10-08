//! `$AGORA_HOME/capabilities.json` (mode 0600): what `agora install` found and set up for each
//! harness, read by the MCP shim to decide which delivery rungs belong in the ladder.
//!
//! Shape: `{"schema":1,"claude":{"probed_at_ms":..,"claude_bin":"..","claude_version":"2.1.293",
//! "mods":true,"channels_flag":true,"async_rewake":true,"stop_hook":true,"context_hook":true,
//! "mcp_negotiation_legacy":true,"install_mode":"plugin","scope":"user"}, ...}`. Unknown
//! top-level keys are preserved on a load/save round trip (`CapabilitiesFile::other`).

use agora_daemon::Paths;
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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        (dir, paths)
    }

    fn full() -> CapabilitiesFile {
        CapabilitiesFile {
            schema: 1,
            claude: Some(ClaudeCapabilities {
                probed_at_ms: 1_786_000_000_000,
                claude_bin: Some("/usr/local/bin/claude".into()),
                claude_version: Some("2.1.293".into()),
                mods: true,
                channels_flag: true,
                async_rewake: true,
                stop_hook: true,
                context_hook: true,
                mcp_negotiation_legacy: true,
                install_mode: Some("plugin".into()),
                scope: Some("user".into()),
            }),
            other: serde_json::Map::new(),
        }
    }

    #[test]
    fn path_is_under_home() {
        let (dir, paths) = home();
        assert_eq!(path(&paths), dir.path().join(FILE_NAME));
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_dir, paths) = home();
        save(&paths, &full()).expect("save");
        assert_eq!(load(&paths), Some(full()));
    }

    #[test]
    fn unknown_top_level_keys_survive_load_and_save() {
        let (_dir, paths) = home();
        let codex = serde_json::json!({"version": "0.9", "flags": [1, 2]});
        let original = serde_json::json!({
            "schema": 1,
            "claude": {"mods": true, "claude_version": "2.1.293"},
            "codex": codex,
        });
        std::fs::write(path(&paths), original.to_string()).expect("write");

        let loaded = load(&paths).expect("loads");
        assert_eq!(loaded.other.get("codex"), Some(&codex));
        save(&paths, &loaded).expect("save");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(&paths)).expect("read")).expect("json");
        assert_eq!(saved.get("codex"), Some(&codex));
        assert_eq!(saved["claude"]["mods"], serde_json::json!(true));
        assert_eq!(
            saved["claude"]["claude_version"],
            serde_json::json!("2.1.293")
        );
        assert_eq!(saved.get("schema"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn missing_file_loads_as_none() {
        let (_dir, paths) = home();
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn garbage_json_loads_as_none() {
        let (_dir, paths) = home();
        std::fs::write(path(&paths), "not json {").expect("write");
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn oversized_file_loads_as_none() {
        let (_dir, paths) = home();
        let pad = "x".repeat(64 * 1024);
        std::fs::write(path(&paths), format!(r#"{{"schema":1,"pad":"{pad}"}}"#)).expect("write");
        assert_eq!(load(&paths), None);
    }

    #[test]
    fn symlink_loads_as_none_and_save_refuses() {
        let (dir, paths) = home();
        let real = dir.path().join("real.json");
        std::fs::write(&real, r#"{"schema":1}"#).expect("write");
        std::os::unix::fs::symlink(&real, path(&paths)).expect("symlink");

        assert_eq!(load(&paths), None);
        let err = save(&paths, &full()).expect_err("save must refuse a symlink");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            r#"{"schema":1}"#
        );
    }

    #[test]
    fn saved_file_is_mode_0600_and_leaves_no_temp_file() {
        let (dir, paths) = home();
        save(&paths, &full()).expect("save");
        let mode = std::fs::metadata(path(&paths))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()]);
    }

    #[test]
    fn save_without_home_dir_is_not_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(&dir.path().join("missing"));
        let err = save(&paths, &full()).expect_err("no home dir");
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn default_file_has_schema_1() {
        let file = CapabilitiesFile::default();
        assert_eq!(file.schema, 1);
        assert!(file.claude.is_none());
        assert!(file.other.is_empty());
    }

    #[test]
    fn schema_only_file_loads_without_claude() {
        let (_dir, paths) = home();
        std::fs::write(path(&paths), r#"{"schema":1}"#).expect("write");
        let loaded = load(&paths).expect("loads");
        assert_eq!(loaded.schema, 1);
        assert!(loaded.claude.is_none());
    }

    #[test]
    fn partial_claude_object_loads_with_defaults() {
        let (_dir, paths) = home();
        std::fs::write(path(&paths), r#"{"schema":1,"claude":{"mods":true}}"#).expect("write");
        let loaded = load(&paths).expect("loads");
        assert_eq!(
            loaded.claude,
            Some(ClaudeCapabilities {
                mods: true,
                ..ClaudeCapabilities::default()
            })
        );
    }
}
