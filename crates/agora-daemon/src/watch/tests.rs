//! Unit tests for the watch helpers (`watch.rs`).

use super::*;
use std::io::Write;
use std::time::Instant;

/// Upper bound for any wait in these tests.
const WAIT: Duration = Duration::from_secs(2);

fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("a valid pattern")
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(bytes).expect("append");
}

#[tokio::test]
async fn changed_resolves_when_the_file_is_created() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ready.txt");
    let writer_path = path.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&writer_path, b"done\n")
    });
    let watched = path.clone();
    let watch = tokio::spawn(async move { changed(&watched, Duration::from_secs(10)).await });
    tokio::time::timeout(WAIT, watch)
        .await
        .expect("changed resolved within 2 s")
        .expect("the watch task ran");
    writer
        .await
        .expect("the writer task ran")
        .expect("write the file");
    assert!(path.exists());
}

#[tokio::test]
async fn changed_returns_after_max_when_nothing_happens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("quiet.txt");
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        changed(&path, Duration::from_millis(300)),
    )
    .await
    .expect("changed returned after max");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "returned before max"
    );
}

#[tokio::test]
async fn changed_ignores_other_files_in_the_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wanted.txt");
    let sibling = dir.path().join("other.txt");
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&sibling, b"x")
    });
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        changed(&path, Duration::from_millis(300)),
    )
    .await
    .expect("changed returned after max");
    writer
        .await
        .expect("the writer task ran")
        .expect("write the sibling");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "woke for another file"
    );
}

#[tokio::test]
async fn changed_falls_back_to_a_timed_sleep_without_a_parent_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("no-such-dir").join("file.txt");
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        changed(&path, Duration::from_millis(300)),
    )
    .await
    .expect("the fallback returned");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the fallback slept"
    );
}

#[test]
fn file_ready_is_false_for_a_missing_empty_or_directory_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("status");
    assert!(!file_ready(&path), "missing");
    std::fs::write(&path, b"").expect("create an empty file");
    assert!(!file_ready(&path), "empty");
    assert!(!file_ready(dir.path()), "a directory");
}

#[test]
fn file_ready_is_true_once_the_file_has_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("status");
    std::fs::write(&path, b"0\n").expect("write");
    assert!(file_ready(&path));
}

#[test]
fn log_scan_matches_a_line_already_in_the_file_from_offset_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    std::fs::write(&path, b"alpha\nbeta gamma\n").expect("write");
    let (offset, carry, matched) = log_scan(&path, 0, "", &re("gamma"));
    assert_eq!((offset, carry.as_str(), matched), (17, "", true));
}

#[test]
fn log_scan_advances_the_offset_and_tests_only_new_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    append(&path, b"one\n");
    let (offset, carry, matched) = log_scan(&path, 0, "", &re("one"));
    assert_eq!((offset, carry.as_str(), matched), (4, "", true));
    append(&path, b"two\n");
    let (offset, carry, matched) = log_scan(&path, offset, &carry, &re("one"));
    assert_eq!(
        (offset, carry.as_str(), matched),
        (8, "", false),
        "old bytes are not retested"
    );
}

#[test]
fn log_scan_matches_a_line_written_in_two_parts_once_complete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    let pattern = re("hello world");
    append(&path, b"hel");
    let (offset, carry, matched) = log_scan(&path, 0, "", &pattern);
    assert_eq!(
        (offset, carry.as_str(), matched),
        (3, "hel", false),
        "a fragment does not match"
    );
    append(&path, b"lo world\n");
    let (offset, carry, matched) = log_scan(&path, offset, &carry, &pattern);
    assert_eq!((offset, carry.as_str(), matched), (12, "", true));
}

#[test]
fn log_scan_matches_a_partial_last_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    append(&path, b"partial match");
    let (offset, carry, matched) = log_scan(&path, 0, "", &re("match"));
    assert_eq!(
        (offset, carry.as_str(), matched),
        (13, "partial match", true)
    );
}

#[test]
fn log_scan_does_not_treat_the_end_of_the_file_as_an_empty_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    append(&path, b"x\n");
    assert!(
        !log_scan(&path, 0, "", &re("^$")).2,
        "no empty line in x\\n"
    );
    append(&path, b"\n");
    assert!(
        log_scan(&path, 2, "", &re("^$")).2,
        "a complete empty line matches"
    );
}

#[test]
fn log_scan_restarts_at_zero_with_an_empty_carry_when_the_file_shrinks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    std::fs::write(&path, b"fresh\n").expect("write");
    assert!(
        log_scan(&path, 100, "stale", &re("fresh")).2,
        "re-read from the top"
    );
    assert!(
        !log_scan(&path, 100, "stale", &re("stale")).2,
        "the old carry is dropped"
    );
    assert_eq!(
        log_scan(&path, 100, "", &re("x")).0,
        6,
        "the offset restarts at the new length"
    );
}

#[test]
fn log_scan_understands_unicode_classes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("job.log");
    append(&path, "héllo 漢字\n".as_bytes());
    assert!(log_scan(&path, 0, "", &re(r"\p{Han}{2}")).2, "a Han class");
    assert!(
        log_scan(&path, 0, "", &re(r"^h\wllo ")).2,
        "\\w is Unicode by default"
    );
}

#[test]
fn log_scan_reads_at_most_16_mib_per_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("big.log");
    let mut bytes = vec![b'a'; MAX_READ as usize];
    bytes.extend_from_slice(b"\nmatch\n");
    std::fs::write(&path, &bytes).expect("write");
    let pattern = re("match");
    let (offset, carry, matched) = log_scan(&path, 0, "", &pattern);
    assert_eq!(offset, MAX_READ, "the first call stops at the cap");
    assert!(!matched);
    assert_eq!(carry.len(), MAX_CARRY, "the carry keeps its last 64 KiB");
    let (offset, _, matched) = log_scan(&path, offset, &carry, &pattern);
    assert_eq!(offset, bytes.len() as u64);
    assert!(matched, "the rest of the file is read on the next call");
}

#[test]
fn log_scan_keeps_a_character_split_by_the_read_cap_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("split.log");
    // The two-byte "é" straddles the 16 MiB cap: its first byte is the last byte of the first read.
    let mut bytes = vec![b'a'; MAX_READ as usize - 1];
    bytes.extend_from_slice("é\n".as_bytes());
    std::fs::write(&path, &bytes).expect("write");
    let pattern = re("é$");
    let (offset, carry, matched) = log_scan(&path, 0, "", &pattern);
    assert!(!matched);
    let (_, _, matched) = log_scan(&path, offset, &carry, &pattern);
    assert!(matched, "the character is decoded whole on the next call");
}

#[test]
fn log_scan_keeps_the_carry_within_the_64_kib_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("long.log");
    let bytes = vec![b'a'; MAX_CARRY + MAX_CARRY / 2];
    std::fs::write(&path, &bytes).expect("write");
    let (offset, carry, matched) = log_scan(&path, 0, "", &re("b"));
    assert_eq!(offset, bytes.len() as u64);
    assert_eq!(carry.len(), MAX_CARRY);
    assert!(!matched);
}

#[test]
fn log_scan_leaves_the_state_alone_for_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent.log");
    assert_eq!(
        log_scan(&path, 7, "keep", &re(".")),
        (7, "keep".to_owned(), false)
    );
}

#[cfg(unix)]
#[test]
fn log_scan_refuses_a_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("real.log");
    let link = dir.path().join("link.log");
    std::fs::write(&target, b"match\n").expect("write");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    assert_eq!(
        log_scan(&link, 0, "", &re("match")),
        (0, String::new(), false)
    );
}

#[test]
fn pid_alive_is_true_for_our_own_process() {
    assert!(pid_alive(i64::from(std::process::id())));
}

#[test]
fn pid_alive_is_false_for_a_reaped_child() {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let pid = i64::from(child.id());
    child.wait().expect("reap the child");
    assert!(!pid_alive(pid));
}

#[test]
fn pid_alive_is_false_for_zero_negative_and_out_of_range_pids() {
    assert!(!pid_alive(0), "zero addresses our own group");
    assert!(!pid_alive(-1), "-1 addresses every process");
    assert!(!pid_alive(i64::MIN));
    assert!(!pid_alive(i64::from(i32::MAX) + 1));
}

#[cfg(unix)]
#[test]
fn pid_alive_is_false_for_a_zombie() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = i64::from(child.id());
    assert!(pid_alive(pid), "a running child is alive");
    child.kill().expect("kill the child");
    let deadline = Instant::now() + WAIT;
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!pid_alive(pid), "a zombie counts as exited");
    let os_pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .expect("a real pid");
    assert!(
        test_kill_process(os_pid).is_ok(),
        "the zombie still exists until reaped"
    );
    child.wait().expect("reap the zombie");
}

#[test]
fn log_scan_matches_a_non_ascii_pattern_split_across_two_scans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("split.log");
    let pattern = re("café");
    // The "é" (C3 A9) is cut after its first byte: that byte waits for the next call.
    append(&path, b"caf\xC3");
    let (offset, carry, matched) = log_scan(&path, 0, "", &pattern);
    assert_eq!(
        (offset, carry.as_str(), matched),
        (3, "caf", false),
        "the incomplete byte is not read yet"
    );
    append(&path, b"\xA9\n");
    let (offset, carry, matched) = log_scan(&path, offset, &carry, &pattern);
    assert_eq!((offset, carry.as_str(), matched), (6, "", true));
}

#[test]
fn log_scan_holds_back_a_three_byte_character_cut_at_the_end_of_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cjk.log");
    let pattern = re("漢字");
    // "漢" is E6 BC A2; the file ends after its first two bytes.
    append(&path, b"\xE6\xBC");
    let (offset, carry, matched) = log_scan(&path, 0, "", &pattern);
    assert_eq!((offset, carry.as_str(), matched), (0, "", false));
    append(&path, "漢字\n".as_bytes().get(2..).unwrap_or_default());
    let (_, _, matched) = log_scan(&path, offset, &carry, &pattern);
    assert!(matched, "the character is decoded whole once complete");
}

#[test]
fn complete_utf8_len_cuts_only_a_trailing_incomplete_sequence() {
    assert_eq!(complete_utf8_len(b""), 0);
    assert_eq!(complete_utf8_len(b"ab"), 2);
    assert_eq!(complete_utf8_len(&[b'a', 0xC3]), 1, "lead byte of é");
    assert_eq!(complete_utf8_len(&[0xE6, 0xBC]), 0, "two of three bytes");
    assert_eq!(complete_utf8_len("é".as_bytes()), 2, "complete");
    assert_eq!(
        complete_utf8_len(&[0xFF, 0xC3]),
        1,
        "an invalid byte is kept, the incomplete tail is cut"
    );
}

#[tokio::test]
async fn watcher_resolves_on_a_write_in_a_watched_dir_within_2_s() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ready.txt");
    let mut watcher = Watcher::new(std::slice::from_ref(&path));
    let writer_path = path.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&writer_path, b"done\n")
    });
    tokio::time::timeout(WAIT, watcher.changed(Duration::from_secs(10)))
        .await
        .expect("the watcher resolved within 2 s");
    writer
        .await
        .expect("the writer task ran")
        .expect("write the file");
}

#[tokio::test]
async fn watcher_keeps_an_event_queued_between_two_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("queued.txt");
    let mut watcher = Watcher::new(std::slice::from_ref(&path));
    // The write happens while nobody waits: the event is queued in the watcher, not lost.
    std::fs::write(&path, b"x").expect("write");
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(WAIT, watcher.changed(Duration::from_secs(10)))
        .await
        .expect("the queued event resolves the next call");
}

#[tokio::test]
async fn watcher_returns_after_max_when_nothing_happens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("quiet.txt");
    let mut watcher = Watcher::new(std::slice::from_ref(&path));
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        watcher.changed(Duration::from_millis(300)),
    )
    .await
    .expect("changed returned after max");
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "returned before max"
    );
}

#[tokio::test]
async fn watcher_without_paths_polls_at_most_one_second() {
    let mut watcher = Watcher::new(&[]);
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(5),
        watcher.changed(Duration::from_secs(5)),
    )
    .await
    .expect("the poll returned");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(900), "slept: {waited:?}");
    assert!(
        waited < Duration::from_secs(3),
        "polled too long: {waited:?}"
    );
}
