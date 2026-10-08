//! Delivery heartbeats: how a pull rung (a Claude Code hook or the plugin mod) proves it is
//! alive, so the MCP shim can tell which rungs are "armed" before it escalates past them.
//!
//! Each writer stores the decimal millisecond timestamp of its latest run in
//! `$AGORA_HOME/delivery/heartbeats/<pk_hex>/<mechanism>` (`Mechanism::as_str()`), where
//! `pk_hex` is the 64 lowercase hex characters of the folder identity's public key.
//! Writes are atomic (temp file + rename); directories are 0700 and files 0600. Nothing is
//! created when `$AGORA_HOME` itself does not exist.

use agora_daemon::Paths;
use agora_deliver::Mechanism;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A rung whose heartbeat is younger than this was "seen" and stays in the ladder even when
/// `capabilities.json` does not list it (7 days).
pub const SEEN_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Heartbeats stamped more than this far in the future are not trusted (clock skew allowance).
const FUTURE_SKEW_MS: i64 = 60_000;

/// A heartbeat file holds one decimal timestamp; anything longer is not one.
const MAX_BEAT_BYTES: u64 = 64;

/// `paths.home/delivery/heartbeats/<pk_hex>`.
pub fn dir(paths: &Paths, pk_hex: &str) -> PathBuf {
    paths.home.join("delivery").join("heartbeats").join(pk_hex)
}

/// Record that mechanism `m` ran at `now_ms`: atomically write the decimal timestamp to
/// `dir(paths, pk_hex)/<m.as_str()>` (temp file + rename; dirs 0700, file 0600). A no-op
/// `Ok(())` when `paths.home` is not a directory. `pk_hex` that is not exactly 64 lowercase
/// hex characters -> `ErrorKind::InvalidInput` (it becomes a path component).
pub fn beat(paths: &Paths, pk_hex: &str, m: Mechanism, now_ms: i64) -> std::io::Result<()> {
    if !paths.home.is_dir() {
        return Ok(());
    }
    if !valid_pk(pk_hex) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "heartbeat key must be 64 lowercase hex characters",
        ));
    }
    let delivery = paths.home.join("delivery");
    make_dir(&delivery)?;
    let heartbeats = delivery.join("heartbeats");
    make_dir(&heartbeats)?;
    let target_dir = heartbeats.join(pk_hex);
    make_dir(&target_dir)?;
    write_atomic(&target_dir, m.as_str(), now_ms.to_string().as_bytes())
}

/// Latest heartbeat per mechanism for `pk_hex`. Missing directory, unknown file names,
/// unreadable or garbage contents and files larger than 64 bytes are ignored.
pub fn read_all(paths: &Paths, pk_hex: &str) -> BTreeMap<Mechanism, i64> {
    let mut out = BTreeMap::new();
    if !valid_pk(pk_hex) {
        return out;
    }
    let target_dir = dir(paths, pk_hex);
    for m in Mechanism::ALL {
        if let Some(t) = read_one(&target_dir.join(m.as_str())) {
            out.insert(m, t);
        }
    }
    out
}

/// How long after a heartbeat the rung counts as armed (ms): `ModSubmit` 60 s,
/// `AsyncRewakeHook` 120 s, `StopContinue` 30 min, `HookInject` 30 min, every other rung 0.
pub fn armed_window_ms(m: Mechanism) -> i64 {
    match m {
        Mechanism::ModSubmit => 60_000,
        Mechanism::AsyncRewakeHook => 120_000,
        Mechanism::StopContinue | Mechanism::HookInject => 1_800_000,
        Mechanism::ChannelPush | Mechanism::OwnedSessionApi | Mechanism::WaitTool => 0,
    }
}

/// Mechanisms whose heartbeat `t` satisfies `now_ms - t <= armed_window_ms(m)` and
/// `t <= now_ms + 60_000` (a timestamp from the future beyond clock skew is not trusted).
pub fn armed(beats: &BTreeMap<Mechanism, i64>, now_ms: i64) -> BTreeSet<Mechanism> {
    beats
        .iter()
        .filter(|(m, t)| {
            let window = armed_window_ms(**m);
            window > 0 && fresh(**t, now_ms, window)
        })
        .map(|(m, _)| *m)
        .collect()
}

/// Mechanisms whose heartbeat is within `SEEN_WINDOW_MS` of `now_ms` (and not from the future
/// beyond 60 s of skew).
pub fn seen(beats: &BTreeMap<Mechanism, i64>, now_ms: i64) -> BTreeSet<Mechanism> {
    beats
        .iter()
        .filter(|(_, t)| fresh(**t, now_ms, SEEN_WINDOW_MS))
        .map(|(m, _)| *m)
        .collect()
}

/// `t` is no more than `window` old and not more than the skew allowance in the future.
fn fresh(t: i64, now_ms: i64, window: i64) -> bool {
    t <= now_ms.saturating_add(FUTURE_SKEW_MS) && now_ms.saturating_sub(t) <= window
}

/// Exactly 64 lowercase hex characters: the value becomes a path component.
fn valid_pk(pk_hex: &str) -> bool {
    pk_hex.len() == 64
        && pk_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Create one directory level with mode 0700 (an existing one is fine). The result must be a
/// real directory, not a symlink, so a beat cannot be redirected elsewhere.
fn make_dir(path: &Path) -> std::io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if std::fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other("heartbeat path is not a directory"))
    }
}

/// Write `bytes` to `dir/name` through a temp file in the same directory and a rename. The temp
/// file is removed when any step fails.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{name}.tmp-{}-{nanos}", std::process::id()));
    let result = write_tmp_then_rename(&tmp, &dir.join(name), bytes);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_tmp_then_rename(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(tmp, target)
}

/// One heartbeat file: a regular file of at most 64 bytes holding a decimal timestamp. The file
/// is opened with `O_NOFOLLOW` (a symlink fails with ELOOP) and the size is checked on the open
/// descriptor, so the checked file is the one that is read.
fn read_one(path: &Path) -> Option<i64> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_BEAT_BYTES {
        return None;
    }
    let mut buf = String::new();
    file.take(MAX_BEAT_BYTES + 1)
        .read_to_string(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_BEAT_BYTES {
        return None;
    }
    buf.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const PK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn beat_then_read_all_round_trips_and_overwrites() {
        let t = tempfile::tempdir().expect("tempdir");
        let p = Paths::new(t.path());
        beat(&p, PK, Mechanism::ModSubmit, NOW - 5).expect("beat");
        beat(&p, PK, Mechanism::StopContinue, NOW - 10).expect("beat");
        beat(&p, PK, Mechanism::ModSubmit, NOW).expect("beat again");
        let beats = read_all(&p, PK);
        assert_eq!(beats.get(&Mechanism::ModSubmit), Some(&NOW));
        assert_eq!(beats.get(&Mechanism::StopContinue), Some(&(NOW - 10)));
        assert_eq!(beats.len(), 2);
        let names: Vec<_> = std::fs::read_dir(dir(&p, PK))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temp files left behind: {names:?}");
    }

    #[test]
    fn beat_creates_nothing_when_home_is_missing() {
        let t = tempfile::tempdir().expect("tempdir");
        let home = t.path().join("absent");
        let p = Paths::new(&home);
        beat(&p, PK, Mechanism::HookInject, NOW).expect("no-op is Ok");
        assert!(!home.exists());
    }

    #[test]
    fn beat_rejects_malformed_pk_before_creating_anything() {
        let t = tempfile::tempdir().expect("tempdir");
        let p = Paths::new(t.path());
        let upper = PK.to_uppercase();
        let short = &PK[..63];
        let long = format!("{PK}0");
        for bad in [upper.as_str(), short, long.as_str(), "../x"] {
            let err = beat(&p, bad, Mechanism::ModSubmit, NOW).expect_err("rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "pk {bad:?}");
        }
        assert!(!t.path().join("delivery").exists());
        assert!(read_all(&p, "../x").is_empty());
    }

    #[test]
    fn read_all_ignores_garbage_and_oversized_files() {
        let t = tempfile::tempdir().expect("tempdir");
        let p = Paths::new(t.path());
        let d = dir(&p, PK);
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join(Mechanism::ModSubmit.as_str()), "12abc").expect("write");
        std::fs::write(d.join(Mechanism::AsyncRewakeHook.as_str()), "1".repeat(65)).expect("write");
        std::fs::write(d.join(Mechanism::StopContinue.as_str()), " 42\n").expect("write");
        std::fs::write(d.join("not_a_mechanism"), "7").expect("write");
        let beats = read_all(&p, PK);
        assert_eq!(beats.len(), 1);
        assert_eq!(beats.get(&Mechanism::StopContinue), Some(&42));
    }

    #[test]
    fn read_all_ignores_symlink_in_place_of_file() {
        let t = tempfile::tempdir().expect("tempdir");
        let p = Paths::new(t.path());
        let d = dir(&p, PK);
        std::fs::create_dir_all(&d).expect("mkdir");
        let elsewhere = t.path().join("elsewhere");
        std::fs::write(&elsewhere, NOW.to_string()).expect("write");
        std::os::unix::fs::symlink(&elsewhere, d.join(Mechanism::ModSubmit.as_str())).expect("ln");
        assert!(read_all(&p, PK).is_empty());
    }

    #[test]
    fn beat_uses_0700_dirs_and_0600_files() {
        let t = tempfile::tempdir().expect("tempdir");
        let p = Paths::new(t.path());
        beat(&p, PK, Mechanism::HookInject, NOW).expect("beat");
        let mode =
            |path: &Path| std::fs::metadata(path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&t.path().join("delivery")), 0o700);
        assert_eq!(mode(&t.path().join("delivery/heartbeats")), 0o700);
        assert_eq!(mode(&dir(&p, PK)), 0o700);
        assert_eq!(
            mode(&dir(&p, PK).join(Mechanism::HookInject.as_str())),
            0o600
        );
    }

    #[test]
    fn armed_windows_hold_exactly_at_the_boundary() {
        let cases = [
            (Mechanism::ModSubmit, 60_000),
            (Mechanism::AsyncRewakeHook, 120_000),
            (Mechanism::StopContinue, 1_800_000),
            (Mechanism::HookInject, 1_800_000),
        ];
        for (m, window) in cases {
            assert_eq!(armed_window_ms(m), window, "{m:?}");
            let at = BTreeMap::from([(m, NOW - window)]);
            assert!(armed(&at, NOW).contains(&m), "{m:?} exactly at window");
            let past = BTreeMap::from([(m, NOW - window - 1)]);
            assert!(!armed(&past, NOW).contains(&m), "{m:?} one ms past window");
        }
    }

    #[test]
    fn armed_ignores_future_beats_beyond_skew() {
        let beats = BTreeMap::from([
            (Mechanism::ModSubmit, NOW + 60_000),
            (Mechanism::StopContinue, NOW + 60_001),
        ]);
        let got = armed(&beats, NOW);
        assert!(got.contains(&Mechanism::ModSubmit));
        assert!(!got.contains(&Mechanism::StopContinue));
    }

    #[test]
    fn wait_tool_and_zero_window_rungs_are_never_armed() {
        let beats = BTreeMap::from([
            (Mechanism::WaitTool, NOW),
            (Mechanism::ChannelPush, NOW),
            (Mechanism::OwnedSessionApi, NOW),
        ]);
        assert!(armed(&beats, NOW).is_empty());
        assert_eq!(seen(&beats, NOW).len(), 3);
    }

    #[test]
    fn seen_window_holds_exactly_at_the_boundary() {
        let at = BTreeMap::from([(Mechanism::WaitTool, NOW - SEEN_WINDOW_MS)]);
        assert!(seen(&at, NOW).contains(&Mechanism::WaitTool));
        let past = BTreeMap::from([(Mechanism::WaitTool, NOW - SEEN_WINDOW_MS - 1)]);
        assert!(seen(&past, NOW).is_empty());
        let future = BTreeMap::from([(Mechanism::WaitTool, NOW + 60_001)]);
        assert!(seen(&future, NOW).is_empty());
    }
}
