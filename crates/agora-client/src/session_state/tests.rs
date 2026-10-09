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
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
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
        Ok::<i64, io::Error>(lock.state().cursor)
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

    let now = crate::ops::now_ms();
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
    assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
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
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
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

    assert!(
        SessionLock::acquire(home, "s").await.is_err(),
        "symlinked sessions dir must be refused"
    );
    assert_eq!(
        fs::read_dir(&elsewhere).expect("read elsewhere").count(),
        0,
        "nothing may be created through the symlink"
    );
}

/// `try_advance` that expects to win the lock. `flock` belongs to the open file description, so a
/// child forked by a concurrent test in this binary (e.g. `mkfifo` in `ops::tests`) keeps the lock
/// alive until it execs; `try_advance` then reports `Ok(false)` ("busy") although every
/// `SessionLock` is gone. Retry on busy; a seq that is genuinely not above the cursor still fails.
fn advance_when_free(home: &Path, session_id: &str, seq: i64) -> bool {
    for _ in 0..200 {
        if try_advance(home, session_id, seq).expect("try_advance") {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn try_advance_moves_the_cursor_forward() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    assert!(advance_when_free(home, "adv", 5), "advance to 5");
    let loaded = load(home, "adv");
    assert_eq!(loaded.cursor, 5);
    assert!(loaded.updated_ms > 0);

    assert!(advance_when_free(home, "adv", 7), "advance to 7");
    assert_eq!(load(home, "adv").cursor, 7);
}

#[test]
fn try_advance_ignores_a_seq_not_above_the_cursor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    assert!(advance_when_free(home, "low", 5), "advance to 5");
    assert!(!try_advance(home, "low", 3).expect("lower seq"));
    assert!(!try_advance(home, "low", 5).expect("equal seq"));
    assert_eq!(load(home, "low").cursor, 5);
}

#[tokio::test]
async fn try_advance_returns_false_while_the_session_lock_is_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let mut lock = SessionLock::acquire(home, "held").await.expect("acquire");
    lock.state().cursor = 1;
    lock.save().expect("save");

    assert!(!try_advance(home, "held", 9).expect("try while held"));
    assert_eq!(load(home, "held").cursor, 1);

    drop(lock);
    assert!(advance_when_free(home, "held", 9), "try after release");
    assert_eq!(load(home, "held").cursor, 9);
}

#[test]
fn try_advance_on_missing_home_fails_and_creates_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("missing-home");
    let err = try_advance(&home, "s", 1).expect_err("missing home must fail");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(!home.exists());
}
