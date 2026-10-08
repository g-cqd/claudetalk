//! Per-Claude-session hook state, shared by the hook rungs of one session.
//!
//! One JSON file `<home>/hooks/sessions/<key>.json` (0600) per session, guarded by an exclusive
//! lock on `<key>.lock` so that two hooks of one session never deliver the same message twice.
//! Directories are 0700. `key` is `session_key(session_id)`.

use crate::HookError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
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
    pub async fn acquire(home: &Path, session_id: &str) -> Result<SessionLock, HookError> {
        if !home.is_dir() {
            return Err(HookError::Io(io::Error::from(io::ErrorKind::NotFound)));
        }
        let dir = sessions_dir(home);
        make_private_dir(&home.join("hooks"))?;
        make_private_dir(&dir)?;

        let key = session_key(session_id);
        let lock_path = dir.join(format!("{key}.lock"));
        // O_NOFOLLOW: a symlink at the lock path fails the open with ELOOP.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)?;
        // A FIFO or a directory planted at the lock path must not be locked or used as a lock.
        require_regular(&file.metadata()?)?;

        let file = tokio::task::spawn_blocking(move || -> io::Result<File> {
            file.lock()?;
            // Keeps an active session's lock file young for `gc`; a failure here is harmless.
            let _ = file.set_modified(SystemTime::now());
            Ok(file)
        })
        .await
        .map_err(|e| HookError::Io(io::Error::other(e)))??;

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
    pub fn save(&mut self) -> Result<(), HookError> {
        self.state.updated_ms = agora_client::ops::now_ms();
        let bytes = serde_json::to_vec(&self.state)?;
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

/// `Ok` only for a regular file; anything else (FIFO, directory, device) is an `InvalidInput` error.
fn require_regular(meta: &fs::Metadata) -> Result<(), HookError> {
    if meta.is_file() {
        Ok(())
    } else {
        Err(HookError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session lock is not a regular file",
        )))
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
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn session_key_keeps_safe_ids_and_strips_one_jsonl() {
        let uuid = "e773b56f-0000-4abc-8def-0123456789ab";
        assert_eq!(session_key(uuid), uuid);
        assert_eq!(session_key(&format!("{uuid}.jsonl")), uuid);
        assert_eq!(session_key("a_b-c"), "a_b-c");
        assert_eq!(session_key("x.jsonl.jsonl").len(), 33);
    }

    #[test]
    fn session_key_empty_is_default() {
        assert_eq!(session_key(""), "default");
        assert_eq!(session_key(".jsonl"), "default");
    }

    #[test]
    fn session_key_hashes_unsafe_and_long_ids() {
        let traversal = session_key("../../etc");
        assert!(traversal.starts_with('h'));
        assert_eq!(traversal.len(), 33);
        assert_eq!(session_key("../../etc"), traversal);

        let long = session_key(&"a".repeat(65));
        assert!(long.starts_with('h'));
        assert_eq!(long.len(), 33);
        assert_eq!(session_key(&"a".repeat(64)), "a".repeat(64));
    }

    #[tokio::test]
    async fn acquire_on_missing_home_fails_and_creates_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("missing-home");
        let err = SessionLock::acquire(&home, "s")
            .await
            .err()
            .expect("missing home must fail");
        assert!(matches!(err, HookError::Io(ref e) if e.kind() == io::ErrorKind::NotFound));
        assert!(!home.exists());
    }

    #[tokio::test]
    async fn save_then_load_sees_the_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut lock = SessionLock::acquire(tmp.path(), "sess-1")
            .await
            .expect("acquire");
        assert_eq!(lock.state().cursor, 0);
        lock.state().cursor = 42;
        lock.state().stop_blocks = 2;
        lock.save().expect("save");
        drop(lock);

        let loaded = load(tmp.path(), "sess-1");
        assert_eq!(loaded.cursor, 42);
        assert_eq!(loaded.stop_blocks, 2);
        assert!(loaded.updated_ms > 0);
    }

    #[tokio::test]
    async fn directories_are_0700_and_files_are_0600() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let mut lock = SessionLock::acquire(home, "perm").await.expect("acquire");
        let dir = sessions_dir(home);
        let key = session_key("perm");
        assert_eq!(mode(&home.join("hooks")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(format!("{key}.lock"))), 0o600);

        lock.save().expect("save");
        assert_eq!(mode(&dir.join(format!("{key}.json"))), 0o600);
    }

    #[test]
    fn garbage_state_loads_as_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(format!("{}.json", session_key("g"))), b"{not json").expect("write");
        assert_eq!(load(tmp.path(), "g"), SessionState::default());
    }

    #[test]
    fn oversized_state_loads_as_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        fs::create_dir_all(&dir).expect("mkdir");
        let big = format!("{{\"cursor\":5,\"pad\":\"{}\"}}", "x".repeat(70 * 1024));
        fs::write(dir.join(format!("{}.json", session_key("big"))), big).expect("write");
        assert_eq!(load(tmp.path(), "big"), SessionState::default());
    }

    #[tokio::test]
    async fn second_acquire_waits_until_first_lock_is_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().to_path_buf();
        let mut first = SessionLock::acquire(&home, "shared")
            .await
            .expect("first acquire");
        first.state().cursor = 7;

        let (started_tx, started_rx) = oneshot::channel();
        let b_home = home.clone();
        let mut second = tokio::spawn(async move {
            let _ = started_tx.send(());
            let mut lock = SessionLock::acquire(&b_home, "shared").await?;
            Ok::<i64, HookError>(lock.state().cursor)
        });
        started_rx.await.expect("second task started");

        // While the first lock is held, the second acquire must still be pending.
        let early = tokio::time::timeout(Duration::from_millis(300), &mut second).await;
        assert!(
            early.is_err(),
            "second acquire finished while the lock was held"
        );

        first.save().expect("save");
        drop(first);
        let cursor = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("second acquire did not finish after release")
            .expect("join")
            .expect("second acquire");
        assert_eq!(cursor, 7);
    }

    #[tokio::test]
    async fn gc_removes_old_files_and_keeps_fresh_ones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        for id in ["old-session", "fresh-session"] {
            let mut lock = SessionLock::acquire(home, id).await.expect("acquire");
            lock.save().expect("save");
        }
        let dir = sessions_dir(home);
        let old_key = session_key("old-session");
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        for name in [format!("{old_key}.json"), format!("{old_key}.lock")] {
            let file = OpenOptions::new()
                .write(true)
                .open(dir.join(name))
                .expect("open");
            file.set_modified(ten_days_ago).expect("set mtime");
        }

        let now = agora_client::ops::now_ms();
        gc(home, now, 7 * 24 * 3600 * 1000);

        assert!(!dir.join(format!("{old_key}.json")).exists());
        assert!(!dir.join(format!("{old_key}.lock")).exists());
        let fresh_key = session_key("fresh-session");
        assert!(dir.join(format!("{fresh_key}.json")).exists());
        assert!(dir.join(format!("{fresh_key}.lock")).exists());
    }

    #[tokio::test]
    async fn symlinked_lock_path_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        // Creates the directories.
        let _ = SessionLock::acquire(home, "other").await.expect("acquire");

        let target = home.join("elsewhere.lock");
        fs::write(&target, b"").expect("write target");
        let link = sessions_dir(home).join(format!("{}.lock", session_key("victim")));
        symlink(&target, &link).expect("symlink");

        let err = SessionLock::acquire(home, "victim")
            .await
            .err()
            .expect("symlinked lock must be refused");
        assert!(matches!(err, HookError::Io(ref e) if e.raw_os_error() == Some(libc::ELOOP)));
        assert!(
            fs::symlink_metadata(&link)
                .expect("metadata")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn require_regular_accepts_a_file_and_rejects_a_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("plain.lock");
        fs::write(&file, b"").expect("write");
        let file_meta = fs::metadata(&file).expect("file metadata");
        assert!(require_regular(&file_meta).is_ok());

        let dir_meta = fs::metadata(tmp.path()).expect("dir metadata");
        let err = require_regular(&dir_meta).expect_err("a directory is not a regular file");
        assert!(matches!(err, HookError::Io(ref e) if e.kind() == io::ErrorKind::InvalidInput));
    }

    #[tokio::test]
    async fn directory_at_lock_path_makes_acquire_fail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        // Creates the directories.
        let _ = SessionLock::acquire(home, "other").await.expect("acquire");

        let planted = sessions_dir(home).join(format!("{}.lock", session_key("planted")));
        fs::create_dir(&planted).expect("mkdir planted lock");

        assert!(SessionLock::acquire(home, "planted").await.is_err());
        assert!(planted.is_dir());
    }

    #[test]
    fn symlinked_state_json_loads_as_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        fs::create_dir_all(&dir).expect("mkdir");
        let target = tmp.path().join("elsewhere.json");
        fs::write(&target, br#"{"cursor":9,"stop_blocks":1}"#).expect("write target");
        let link = dir.join(format!("{}.json", session_key("linked")));
        symlink(&target, &link).expect("symlink");

        assert_eq!(load(tmp.path(), "linked"), SessionState::default());
    }

    #[tokio::test]
    async fn symlinked_sessions_dir_makes_acquire_fail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        fs::create_dir_all(home.join("hooks")).expect("mkdir hooks");
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("mkdir elsewhere");
        symlink(&elsewhere, sessions_dir(home)).expect("symlink");

        let err = SessionLock::acquire(home, "s")
            .await
            .err()
            .expect("symlinked sessions dir must be refused");
        assert!(matches!(err, HookError::Io(_)));
        assert_eq!(
            fs::read_dir(&elsewhere).expect("read elsewhere").count(),
            0,
            "nothing may be created through the symlink"
        );
    }
}
