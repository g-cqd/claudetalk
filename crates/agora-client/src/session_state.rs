//! Per-Claude-session state, shared by the hook rungs (`agora-hooks`) and the MCP push task
//! (`agora-mcp`) of one session.
//!
//! One JSON file `<home>/hooks/sessions/<key>.json` (0600) per session, guarded by an exclusive
//! lock on `<key>.lock` so that two hooks of one session never deliver the same message twice.
//! Directories are 0700. `key` is `session_key(session_id)`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest state file `load` will read; bigger files are treated as garbage.
const MAX_STATE_BYTES: u64 = 64 * 1024;

/// The persisted state of one session (missing fields default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// Highest inbox seq already delivered to this Claude session.
    pub cursor: i64,
    /// Consecutive Stop blocks since the last UserPromptSubmit / empty Stop.
    pub stop_blocks: u32,
    /// `"<pid>-<start ms>"` of the rewake that owns the long-poll (newest wins).
    pub rewake_owner: Option<String>,
    pub updated_ms: i64,
}

/// File-name stem of a session id: strip a trailing ".jsonl"; keep the id if it is 1..=64 chars
/// of `[A-Za-z0-9_-]`; empty -> "default"; anything else -> "h" + the first 32 hex chars of its
/// sha256.
pub fn session_key(session_id: &str) -> String {
    use std::fmt::Write as _;

    let rest = session_id.strip_suffix(".jsonl").unwrap_or(session_id);
    if rest.is_empty() {
        return "default".to_string();
    }
    let safe = rest.len() <= 64
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if safe {
        return rest.to_string();
    }
    let digest = Sha256::digest(rest.as_bytes());
    let mut key = String::from("h");
    for b in digest.iter().take(16) {
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// `home/hooks/sessions`.
pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("hooks").join("sessions")
}

/// Read the session's state without locking. Missing, garbage or larger than 64 KiB -> default.
pub fn load(home: &Path, session_id: &str) -> SessionState {
    let path = sessions_dir(home).join(format!("{}.json", session_key(session_id)));
    read_state(&path).unwrap_or_default()
}

/// Read and parse a state file; `None` for anything that is not a small regular file holding
/// valid JSON. `O_NOFOLLOW` makes a symlink fail the open (ELOOP) instead of being followed.
fn read_state(path: &Path) -> Option<SessionState> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    // Checked on the open descriptor, so the type and size cannot change between check and read.
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_STATE_BYTES {
        return None;
    }
    // Bounded read: the file may have grown since the metadata check.
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_STATE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// An exclusive lock on `sessions_dir/<key>.lock` (0600) plus the state loaded under it.
/// Directories (0700) are created only when `home` is a directory, else `acquire` fails. The
/// lock is taken with `File::lock()` inside `tokio::task::spawn_blocking` and is released when
/// the value is dropped.
pub struct SessionLock {
    _file: File,
    dir: PathBuf,
    key: String,
    state: SessionState,
}

impl SessionLock {
    /// Take the lock, then load the state like `load`.
    pub async fn acquire(home: &Path, session_id: &str) -> io::Result<SessionLock> {
        let dir = session_dir(home)?;
        let key = session_key(session_id);
        let file = open_lock(&dir, &key)?;

        let file = tokio::task::spawn_blocking(move || -> io::Result<File> {
            file.lock()?;
            // Keeps an active session's lock file young for `gc`; a failure here is harmless.
            let _ = file.set_modified(SystemTime::now());
            Ok(file)
        })
        .await
        .map_err(io::Error::other)??;

        let state = load(home, session_id);
        Ok(SessionLock {
            _file: file,
            dir,
            key,
            state,
        })
    }

    /// The state loaded under the lock; edit it, then `save`.
    pub fn state(&mut self) -> &mut SessionState {
        &mut self.state
    }

    /// Atomic save (temp file + rename, 0600) of the state with `updated_ms` = now; keeps the
    /// lock until drop.
    pub fn save(&mut self) -> io::Result<()> {
        self.state.updated_ms = crate::ops::now_ms();
        let bytes = serde_json::to_vec(&self.state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tmp = self.dir.join(format!(
            ".{}.json.tmp-{}-{nanos}",
            self.key,
            std::process::id()
        ));
        let dest = self.dir.join(format!("{}.json", self.key));
        write_atomic(&tmp, &dest, &bytes)?;
        Ok(())
    }
}

/// Non-blocking cursor advance for the MCP push task. Takes the session lock with `try_lock`:
/// a lock held by another holder is `Ok(false)`, never a wait. Under the lock, if `seq` is above
/// the stored cursor the cursor becomes `seq` and the state is saved atomically (`Ok(true)`);
/// otherwise `Ok(false)`. `home` must be an existing directory (`NotFound` otherwise) and is
/// never created; `hooks/sessions` is created (0700) like `acquire` does.
pub fn try_advance(home: &Path, session_id: &str, seq: i64) -> io::Result<bool> {
    let dir = session_dir(home)?;
    let key = session_key(session_id);
    let file = open_lock(&dir, &key)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    // Same as `acquire`: keeps an active session's lock file young for `gc`.
    let _ = file.set_modified(SystemTime::now());
    let state = load(home, session_id);
    let mut lock = SessionLock {
        _file: file,
        dir,
        key,
        state,
    };
    if seq <= lock.state.cursor {
        return Ok(false);
    }
    lock.state.cursor = seq;
    lock.save()?;
    Ok(true)
}

/// `home/hooks/sessions` after checking that `home` is a directory and creating (0700) the
/// `hooks` and `sessions` directories below it. `NotFound` when `home` is missing.
fn session_dir(home: &Path) -> io::Result<PathBuf> {
    if !home.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let dir = sessions_dir(home);
    make_private_dir(&home.join("hooks"))?;
    make_private_dir(&dir)?;
    Ok(dir)
}

/// Open (creating it 0600) `dir/<key>.lock` with `O_NOFOLLOW`, then require a regular file: a
/// symlink fails the open with ELOOP, and a FIFO or directory is refused before it can be locked.
fn open_lock(dir: &Path, key: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{key}.lock")))?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}

/// `Ok` only for a regular file; anything else (FIFO, directory, device) is an `InvalidInput` error.
fn require_regular(meta: &fs::Metadata) -> io::Result<()> {
    if meta.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session lock is not a regular file",
        ))
    }
}

/// Create `path` with mode 0700. An existing path is accepted only if it is a real directory: a
/// symlink (even one to a directory) or a file is an error.
fn make_private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // `symlink_metadata` does not follow links, so a symlink is never `is_dir()` here.
            if fs::symlink_metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "existing session path is not a real directory",
                ))
            }
        }
        Err(e) => Err(e),
    }
}

/// Write `bytes` to a new 0600 file `tmp`, then rename it onto `dest`. `tmp` is removed when
/// the write or the rename fails.
fn write_atomic(tmp: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let result = file.write_all(bytes).and_then(|()| fs::rename(tmp, dest));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Modification time of `meta` as ms since the Unix epoch.
fn mtime_ms(meta: &fs::Metadata) -> Option<i64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Remove the `<key>.json` and `<key>.lock` files in `sessions_dir(home)` whose mtime is older
/// than `max_age_ms` before `now_ms`. Best effort: errors are ignored.
pub fn gc(home: &Path, now_ms: i64, max_age_ms: i64) {
    let cutoff = now_ms.saturating_sub(max_age_ms);
    let Ok(entries) = fs::read_dir(sessions_dir(home)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Session files, and leftover temp files (which start with '.').
        if !(name.ends_with(".json") || name.ends_with(".lock") || name.starts_with('.')) {
            continue;
        }
        // `DirEntry::metadata` does not follow symlinks, so a symlink is never a regular file.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() && mtime_ms(&meta).is_some_and(|t| t < cutoff) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests;
