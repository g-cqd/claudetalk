//! Safe text-file IO for the non-JSON client files (TOML, Markdown, MDC). Same safety as
//! `safe_write`: symlinks refused, backup `<path>.agora-bak-<ms>` (newest 3 kept), atomic temp +
//! rename, byte-identical -> `Unchanged`.

#[cfg(test)]
mod tests;

use super::claude::{Action, action};
use super::safe_write::{self, WriteResult};
use crate::CliError;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Largest text file `read_text` accepts (the same cap as `safe_write::MAX_JSON_BYTES`).
const MAX_TEXT_BYTES: u64 = 4_000_000;
/// Most lines `line_diff` returns, the "… N more lines" line included.
const MAX_DIFF_LINES: usize = 200;

/// Read a text file. `Ok(None)`: missing. Symlink, non-regular file, larger than 4 MB or not UTF-8 -> `Err`
/// naming the file.
pub fn read_text(path: &Path) -> Result<Option<String>, CliError> {
    let Some(meta) = safe_write::lstat(path)? else {
        return Ok(None);
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
    if meta.len() > MAX_TEXT_BYTES {
        return Err(too_large(path));
    }
    // Read at most one byte past the cap, so a file that grew after the stat is still refused.
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TEXT_BYTES {
        return Err(too_large(path));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| CliError::Usage(format!("{} is not UTF-8 text", path.display())))
}

/// `safe_write::write_bytes(path, text, new_mode, dry_run)`; for a dry run that would change the
/// file the `DryRun` diff is `line_diff(existing or "", text)`. A symlink is refused first, dry
/// run included. Byte-identical text -> `Unchanged` in both modes.
pub fn write_text(
    path: &Path,
    text: &str,
    new_mode: u32,
    dry_run: bool,
) -> Result<WriteResult, CliError> {
    if !dry_run {
        return safe_write::write_bytes(path, text.as_bytes(), new_mode, false);
    }
    safe_write::refuse_symlink(path)?;
    let old = read_text(path)?;
    if old.as_deref() == Some(text) {
        return Ok(WriteResult::Unchanged);
    }
    Ok(WriteResult::DryRun {
        diff: line_diff(old.as_deref().unwrap_or(""), text),
    })
}

/// Unified-ish diff: `- <old line>` / `+ <new line>` for lines that differ (common prefix and suffix
/// skipped), at most 200 lines (a final `… N more lines` line when cut, counted in the 200).
/// Lines keep their terminator for comparison, so a trailing-newline change shows as `- a` / `+ a`.
pub fn line_diff(old: &str, new: &str) -> Vec<String> {
    let old: Vec<&str> = old.split_inclusive('\n').collect();
    let new: Vec<&str> = new.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let removed = old[prefix..old.len() - suffix]
        .iter()
        .map(|l| format!("- {}", clean(l)));
    let added = new[prefix..new.len() - suffix]
        .iter()
        .map(|l| format!("+ {}", clean(l)));
    let mut out: Vec<String> = removed.chain(added).collect();
    if out.len() > MAX_DIFF_LINES {
        let more = out.len() - (MAX_DIFF_LINES - 1);
        out.truncate(MAX_DIFF_LINES - 1);
        out.push(format!("… {more} more lines"));
    }
    out
}

/// Delete `path` after copying it to `<path>.agora-bak-<ms>`. Missing -> `Action` result `absent`;
/// `dry_run` -> result `dry_run` (nothing touched); else `removed` (detail names the backup).
/// A symlink or non-regular file -> `Err`.
pub fn remove_file(path: &Path, dry_run: bool) -> Result<Action, CliError> {
    let Some(meta) = safe_write::lstat(path)? else {
        return Ok(action(path, "absent", "nothing to remove".to_owned()));
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::Usage(format!(
            "refusing to remove symlink {}",
            path.display()
        )));
    }
    if !meta.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not a regular file; refusing to remove it",
            path.display()
        )));
    }
    let name = file_name(path)?;
    if dry_run {
        return Ok(action(path, "dry_run", "would remove".to_owned()));
    }
    let backup = safe_write::backup_existing(
        path,
        parent_dir(path),
        name,
        meta.permissions().mode() & 0o777,
    )?;
    fs::remove_file(path)?;
    Ok(action(
        path,
        "removed",
        format!("backup {}", backup.display()),
    ))
}

/// The "too large" error for `read_text`.
fn too_large(path: &Path) -> CliError {
    CliError::Usage(format!(
        "{} is larger than {MAX_TEXT_BYTES} bytes",
        path.display()
    ))
}

/// A diff line without its line terminator.
fn clean(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

/// The UTF-8 file name of `path`, or `Err`.
fn file_name(path: &Path) -> Result<&str, CliError> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("{} is not a UTF-8 file path", path.display())))
}

/// The directory that holds `path` (`.` for a bare file name).
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}
