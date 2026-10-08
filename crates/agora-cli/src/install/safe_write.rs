//! Safe JSON config writes (port of the legacy `src/safe-write.ts`): never corrupt or lose a
//! user's settings file.

use crate::CliError;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest JSON file `read_json` accepts.
pub const MAX_JSON_BYTES: u64 = 4_000_000;
/// Backups of one path that `write_json` / `write_bytes` keep (the newest).
pub const KEEP_BACKUPS: usize = 3;

/// Mode of a new JSON config file (owner read/write only).
const JSON_NEW_MODE: u32 = 0o600;
/// Longest rendered value in a `structural_diff` line; a longer one is cut and ends with "…".
const DIFF_VALUE_CHARS: usize = 120;
const BACKUP_TAG: &str = ".agora-bak-";
const TEMP_TAG: &str = ".agora-tmp-";

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteResult {
    /// Written; `backup` is the copy of the previous file, if there was one.
    Wrote { backup: Option<PathBuf> },
    /// The file already had exactly these bytes; nothing was touched.
    Unchanged,
    /// Dry run: the structural diff lines (`structural_diff`); nothing was written.
    DryRun { diff: Vec<String> },
}

/// Read a JSON config. A missing file -> `{}`. Larger than `MAX_JSON_BYTES`, a parse error or a
/// non-object top level -> `Err` (and the caller writes NOTHING). A symlink -> `Err`.
pub fn read_json(path: &Path) -> Result<Value, CliError> {
    let Some(meta) = lstat(path)? else {
        return Ok(Value::Object(Map::new()));
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::Usage(format!(
            "refusing to read through symlink {}",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let too_large = || {
        CliError::Usage(format!(
            "{} is larger than {MAX_JSON_BYTES} bytes",
            path.display()
        ))
    };
    if meta.len() > MAX_JSON_BYTES {
        return Err(too_large());
    }
    // Read at most one byte past the cap, so a file that grew after the stat is still refused.
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JSON_BYTES {
        return Err(too_large());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        CliError::Usage(format!(
            "cannot parse {}: {e}; fix or move it, nothing was written",
            path.display()
        ))
    })?;
    if !value.is_object() {
        return Err(CliError::Usage(format!(
            "{} does not hold a JSON object",
            path.display()
        )));
    }
    Ok(value)
}

/// Serialize `value` pretty (2-space indent) plus "\n" and `write_bytes` it with mode 0600 for
/// a new file (an existing file keeps its mode). `dry_run` returns `WriteResult::DryRun` with
/// `structural_diff(existing or {}, value)`. A symlink is refused first, dry run included.
pub fn write_json(path: &Path, value: &Value, dry_run: bool) -> Result<WriteResult, CliError> {
    let mut serialized = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    serialized.push('\n');
    let bytes = serialized.as_bytes();
    refuse_symlink(path)?;
    if same_content(path, bytes)? {
        return Ok(WriteResult::Unchanged);
    }
    if dry_run {
        let before = read_json(path).unwrap_or_else(|_| Value::Object(Map::new()));
        return Ok(WriteResult::DryRun {
            diff: structural_diff(&before, value),
        });
    }
    replace_file(path, bytes, JSON_NEW_MODE)
}

/// A symlink -> `Err`, dry run included, so a dry run refuses exactly where the real run would.
/// Otherwise byte-identical to the existing file -> `Unchanged` (no backup). `dry_run` ->
/// `DryRun`, nothing written. Create parent directories. Keep the existing
/// file's mode, else `mode`. Before replacing an existing file copy it to
/// `<path>.agora-bak-<ms>` (same mode) and keep only the newest `KEEP_BACKUPS`. Write a temp
/// file `.<name>.agora-tmp-<pid>-<nanos>` in the same directory (`create_new`, written,
/// fsync'd), rename it over the target, fsync the parent directory; remove the temp file on any
/// failure.
pub fn write_bytes(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    dry_run: bool,
) -> Result<WriteResult, CliError> {
    refuse_symlink(path)?;
    if same_content(path, bytes)? {
        return Ok(WriteResult::Unchanged);
    }
    if dry_run {
        return Ok(WriteResult::DryRun {
            diff: vec![format!("write {} ({} bytes)", path.display(), bytes.len())],
        });
    }
    replace_file(path, bytes, mode)
}

/// Lines `+ $.a.b: <json>` (added), `- $.a.b: <json>` (removed), `~ $.a.b: <old> -> <new>`
/// (changed), sorted by path; values are cut to 120 chars. Objects are compared key by key,
/// every other value (arrays included) as a whole.
pub fn structural_diff(before: &Value, after: &Value) -> Vec<String> {
    let mut lines: Vec<(String, String)> = Vec::new();
    collect_diff("$", Some(before), Some(after), &mut lines);
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines.into_iter().map(|(_, line)| line).collect()
}

fn collect_diff(
    path: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<(String, String)>,
) {
    match (before, after) {
        (None, None) => {}
        (None, Some(a)) => out.push((path.to_owned(), format!("+ {path}: {}", render_value(a)))),
        (Some(b), None) => out.push((path.to_owned(), format!("- {path}: {}", render_value(b)))),
        (Some(Value::Object(b)), Some(Value::Object(a))) => {
            let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
            for key in keys {
                collect_diff(&format!("{path}.{key}"), b.get(key), a.get(key), out);
            }
        }
        (Some(b), Some(a)) => {
            if b != a {
                out.push((
                    path.to_owned(),
                    format!("~ {path}: {} -> {}", render_value(b), render_value(a)),
                ));
            }
        }
    }
}

/// Compact JSON cut to `DIFF_VALUE_CHARS` characters (the last one is "…" when cut).
fn render_value(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() <= DIFF_VALUE_CHARS {
        return s;
    }
    let mut out: String = s.chars().take(DIFF_VALUE_CHARS - 1).collect();
    out.push('…');
    out
}

/// `lstat`: `None` when the path does not exist. Never follows a symlink.
fn lstat(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `Err` when `path` is a symlink. Runs before every write decision, dry runs included.
fn refuse_symlink(path: &Path) -> Result<(), CliError> {
    if lstat(path)?.is_some_and(|m| m.file_type().is_symlink()) {
        return Err(CliError::Usage(format!(
            "refusing to write through symlink {}",
            path.display()
        )));
    }
    Ok(())
}

/// True when `path` is a regular file holding exactly `bytes`. Never reads through a symlink.
fn same_content(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let Some(meta) = lstat(path)? else {
        return Ok(false);
    };
    if !meta.is_file() || meta.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
        return Ok(false);
    }
    Ok(fs::read(path)? == bytes)
}

/// The write itself, after the symlink, unchanged and dry-run checks: create the parent
/// directories, back up an existing file, then replace it atomically.
fn replace_file(path: &Path, bytes: &[u8], mode: u32) -> Result<WriteResult, CliError> {
    let meta = lstat(path)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("{} is not a UTF-8 file path", path.display())))?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;

    let (mode, backup) = match meta.filter(Metadata::is_file) {
        Some(existing) => {
            let existing_mode = existing.permissions().mode() & 0o777;
            (
                existing_mode,
                Some(backup_existing(path, dir, name, existing_mode)?),
            )
        }
        None => (mode & 0o777, None),
    };
    write_temp_and_rename(dir, path, name, bytes, mode)?;
    Ok(WriteResult::Wrote { backup })
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Copy the existing file to a fresh `<name>.agora-bak-<ms>` (same mode), then prune old backups.
fn backup_existing(path: &Path, dir: &Path, name: &str, mode: u32) -> io::Result<PathBuf> {
    let backup = free_backup_name(dir, name, now_ms())?;
    if let Err(e) = copy_with_mode(path, &backup, mode) {
        let _ = fs::remove_file(&backup);
        return Err(e);
    }
    prune_backups(dir, name);
    Ok(backup)
}

fn copy_with_mode(from: &Path, to: &Path, mode: u32) -> io::Result<()> {
    fs::copy(from, to)?;
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
}

/// `<name>.agora-bak-<ms>`, or `<name>.agora-bak-<ms>-<n>` (n from 1) when that name is taken.
fn free_backup_name(dir: &Path, name: &str, ms: u128) -> io::Result<PathBuf> {
    let mut candidate = dir.join(format!("{name}{BACKUP_TAG}{ms}"));
    let mut n: u32 = 1;
    while lstat(&candidate)?.is_some() {
        candidate = dir.join(format!("{name}{BACKUP_TAG}{ms}-{n}"));
        n += 1;
    }
    Ok(candidate)
}

/// The `(ms, n)` sort key of a backup of `name` (`<name>.agora-bak-<ms>[-<n>]`), or `None` for
/// any other file name.
fn backup_key(entry: &str, name: &str) -> Option<(u128, u32)> {
    let rest = entry.strip_prefix(name)?.strip_prefix(BACKUP_TAG)?;
    let (ms, n) = match rest.split_once('-') {
        Some((ms, n)) => (ms, n.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    Some((ms.parse::<u128>().ok()?, n))
}

/// Keep the newest `KEEP_BACKUPS` regular-file backups of `name` in `dir`; removal is best
/// effort, since a leftover backup is harmless.
fn prune_backups(dir: &Path, name: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<((u128, u32), PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let entry_name = entry.file_name().into_string().ok()?;
            let key = backup_key(&entry_name, name)?;
            let is_file = entry.file_type().ok()?.is_file();
            is_file.then(|| (key, entry.path()))
        })
        .collect();
    backups.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, old) in backups.into_iter().skip(KEEP_BACKUPS) {
        let _ = fs::remove_file(old);
    }
}

/// Write `bytes` to a new temp file next to `path`, fsync it, rename it over `path`, then fsync
/// the directory (its errors are ignored). The temp file is removed when a step fails.
fn write_temp_and_rename(
    dir: &Path,
    path: &Path,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> io::Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{name}{TEMP_TAG}{}-{nanos}", std::process::id()));
    // `create_new` failing means the file is not ours, so it is not removed below.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let synced = write_synced(&mut file, bytes, mode);
    drop(file);
    if let Err(e) = synced.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(parent) = File::open(dir) {
        let _ = parent.sync_all();
    }
    Ok(())
}

fn write_synced(file: &mut File, bytes: &[u8], mode: u32) -> io::Result<()> {
    // Set the mode explicitly: the create-time mode is masked by the umask.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn backups_in(dir: &Path, name: &str) -> Vec<PathBuf> {
        let prefix = format!("{name}{BACKUP_TAG}");
        names_in(dir)
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .map(|n| dir.join(n))
            .collect()
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[test]
    fn read_missing_file_is_empty_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let value = read_json(&dir.path().join("settings.json")).expect("read");
        assert_eq!(value, json!({}));
    }

    #[test]
    fn read_parse_error_is_refused_and_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{\"a\":").expect("write");
        let err = read_json(&path).expect_err("parse error");
        let msg = err.to_string();
        assert!(msg.contains("cannot parse"), "{msg}");
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(fs::read(&path).expect("read"), b"{\"a\":");
    }

    #[test]
    fn read_non_object_top_level_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"[1,2]").expect("write");
        let msg = read_json(&path).expect_err("non-object").to_string();
        assert!(msg.contains("does not hold a JSON object"), "{msg}");
    }

    #[test]
    fn read_oversize_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let size = usize::try_from(MAX_JSON_BYTES + 1).expect("fits in usize");
        fs::write(&path, vec![b' '; size]).expect("write");
        let msg = read_json(&path).expect_err("oversize").to_string();
        assert!(msg.contains("is larger than 4000000 bytes"), "{msg}");
    }

    #[test]
    fn symlink_is_refused_for_read_and_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.json");
        let link = dir.path().join("settings.json");
        fs::write(&real, b"{}\n").expect("write");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let msg = read_json(&link).expect_err("read through link").to_string();
        assert!(msg.contains("refusing to read through symlink"), "{msg}");
        let value = json!({"a": 1});
        assert!(write_json(&link, &value, false).is_err());
        assert!(write_bytes(&link, b"{}\n", 0o600, false).is_err());

        assert_eq!(fs::read(&real).expect("read"), b"{}\n");
        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn dry_run_on_symlink_is_refused_like_the_real_run_and_target_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.json");
        let link = dir.path().join("settings.json");
        fs::write(&real, b"{}\n").expect("write");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let value = json!({"a": 1});
        let real_run = write_json(&link, &value, false)
            .expect_err("real run refuses")
            .to_string();
        let dry_json = write_json(&link, &value, true)
            .expect_err("dry run refuses")
            .to_string();
        assert_eq!(dry_json, real_run);
        assert!(
            dry_json.contains("refusing to write through symlink"),
            "{dry_json}"
        );

        let dry_bytes = write_bytes(&link, b"{\"a\": 1}\n", 0o600, true)
            .expect_err("dry run refuses")
            .to_string();
        assert!(
            dry_bytes.contains("refusing to write through symlink"),
            "{dry_bytes}"
        );

        assert_eq!(fs::read(&real).expect("read"), b"{}\n");
        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
        assert!(backups_in(dir.path(), "settings.json").is_empty());
    }

    #[test]
    fn new_file_is_created_with_mode_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.json");
        let result = write_json(&path, &json!({"a": 1}), false).expect("write");
        assert_eq!(result, WriteResult::Wrote { backup: None });
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn existing_file_keeps_its_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_json(&path, &json!({"a": 1}), false).expect("first write");
        set_mode(&path, 0o644);
        let result = write_json(&path, &json!({"a": 2}), false).expect("second write");
        assert!(matches!(result, WriteResult::Wrote { backup: Some(_) }));
        assert_eq!(mode_of(&path), 0o644);
    }

    #[test]
    fn identical_content_is_unchanged_without_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_json(&path, &json!({"a": 1}), false).expect("first write");
        let again = write_json(&path, &json!({"a": 1}), false).expect("second write");
        assert_eq!(again, WriteResult::Unchanged);
        let bytes = b"{\n  \"a\": 1\n}\n";
        assert_eq!(
            write_bytes(&path, bytes, 0o600, true).expect("dry"),
            WriteResult::Unchanged
        );
        assert!(backups_in(dir.path(), "settings.json").is_empty());
    }

    #[test]
    fn change_creates_backup_with_old_bytes_and_same_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{\"old\": true}\n").expect("write");
        set_mode(&path, 0o640);
        let result = write_json(&path, &json!({"new": true}), false).expect("write");
        let WriteResult::Wrote {
            backup: Some(backup),
        } = result
        else {
            panic!("expected a backup, got {result:?}");
        };
        assert_eq!(
            fs::read(&backup).expect("read backup"),
            b"{\"old\": true}\n"
        );
        assert_eq!(mode_of(&backup), 0o640);
        assert_eq!(mode_of(&path), 0o640);
    }

    #[test]
    fn keeps_only_the_newest_three_backups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        for v in 1..=5 {
            write_json(&path, &json!({ "v": v }), false).expect("write");
        }
        let backups = backups_in(dir.path(), "settings.json");
        assert_eq!(backups.len(), KEEP_BACKUPS);
        let mut kept: Vec<i64> = backups
            .iter()
            .map(|b| {
                let value = read_json(b).expect("backup is JSON");
                value["v"].as_i64().expect("v")
            })
            .collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![2, 3, 4]);
    }

    #[test]
    fn dry_run_writes_nothing_and_returns_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let before = b"{\"a\": 1, \"b\": 2}\n";
        fs::write(&path, before).expect("write");
        let result = write_json(&path, &json!({"a": 2, "c": 3}), true).expect("dry run");
        assert_eq!(
            result,
            WriteResult::DryRun {
                diff: vec![
                    "~ $.a: 1 -> 2".to_owned(),
                    "- $.b: 2".to_owned(),
                    "+ $.c: 3".to_owned(),
                ],
            }
        );
        assert_eq!(fs::read(&path).expect("read"), before);
        assert_eq!(names_in(dir.path()), vec!["settings.json".to_owned()]);
    }

    #[test]
    fn dry_run_write_bytes_reports_the_write_without_creating_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new.txt");
        let result = write_bytes(&path, b"abc", 0o600, true).expect("dry run");
        assert_eq!(
            result,
            WriteResult::DryRun {
                diff: vec![format!("write {} (3 bytes)", path.display())],
            }
        );
        assert!(!path.exists());
    }

    #[test]
    fn no_temp_file_is_left_after_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        write_json(&path, &json!({"a": 1}), false).expect("first");
        write_json(&path, &json!({"a": 2}), false).expect("second");
        assert!(names_in(dir.path()).iter().all(|n| !n.contains(TEMP_TAG)));
    }

    #[test]
    fn no_temp_file_is_left_after_failed_rename() {
        // A directory at the target makes the final rename fail after the temp file exists.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::create_dir(&path).expect("mkdir");
        assert!(write_bytes(&path, b"{}\n", 0o600, false).is_err());
        assert_eq!(names_in(dir.path()), vec!["settings.json".to_owned()]);
        assert!(path.is_dir());
    }

    #[test]
    fn structural_diff_orders_by_path_and_covers_all_kinds() {
        let before = json!({"z": 1, "a": {"k": [1, 2], "gone": true}});
        let after = json!({"a": {"k": [1, 3], "new": "x"}, "m": null});
        assert_eq!(
            structural_diff(&before, &after),
            vec![
                "- $.a.gone: true".to_owned(),
                "~ $.a.k: [1,2] -> [1,3]".to_owned(),
                "+ $.a.new: \"x\"".to_owned(),
                "+ $.m: null".to_owned(),
                "- $.z: 1".to_owned(),
            ]
        );
    }

    #[test]
    fn structural_diff_cuts_long_values_at_120_chars() {
        let long = "x".repeat(300);
        let lines = structural_diff(&json!({"s": long}), &json!({}));
        assert_eq!(lines.len(), 1);
        let value = lines[0].strip_prefix("- $.s: ").expect("prefix");
        assert_eq!(value.chars().count(), DIFF_VALUE_CHARS);
        assert!(value.ends_with('…'));

        // Multi-byte characters are cut on a character boundary, never inside one.
        let accented = "é".repeat(300);
        let lines = structural_diff(&json!({}), &json!({"s": accented}));
        let value = lines[0].strip_prefix("+ $.s: ").expect("prefix");
        assert_eq!(value.chars().count(), DIFF_VALUE_CHARS);
        assert!(value.ends_with('…'));
    }
}
