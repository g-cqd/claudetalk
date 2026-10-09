//! The `agy` driver against fake `agy` shell scripts: each test writes a script into a temp dir,
//! hands `drive` one delivery and checks the reply and the process it ran. Scripts write their
//! files relative to the working directory, which must be the temp dir (`opts.cwd`).

#![cfg(all(feature = "agy", unix))]

use agora_bridge::agy::{AgyOptions, drive};
use agora_bridge::{BridgeError, Delivery};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Bound on any single `drive` call in these tests.
const T: Duration = Duration::from_secs(10);

/// Writes `#!/bin/sh` followed by `body` as an executable `agy` in `dir`.
fn fake_agy(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("agy");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake agy");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod agy");
    path
}

fn options(bin: PathBuf, cwd: &Path) -> AgyOptions {
    AgyOptions::new(bin, cwd.to_path_buf(), "conv-1".to_string())
}

/// Hands `drive` one delivery and closes the channel behind it, so `drive` returns after that
/// delivery. Returns the drive result and the reply (`None`: the driver dropped it unanswered).
async fn deliver_one(
    opts: &AgyOptions,
    text: &str,
    shutdown: CancellationToken,
) -> (Result<(), BridgeError>, Option<bool>) {
    let (tx, rx) = mpsc::channel(1);
    let (reply, answer) = oneshot::channel();
    let delivery = Delivery {
        text: text.to_string(),
        max_seq: 1,
        count: 1,
        reply,
    };
    tx.send(delivery).await.expect("driver is listening");
    drop(tx);
    let res = tokio::time::timeout(T, drive(opts, rx, shutdown))
        .await
        .expect("drive returns in time");
    (res, answer.await.ok())
}

/// Whether the process `pid` exists (`kill -0` succeeds).
fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .expect("kill -0 runs")
        .success()
}

/// Asserts that the process whose pid the script wrote to `pid_file` is gone. A process orphaned
/// by a group kill is reaped by its new parent a moment later, so this polls for a few seconds.
fn assert_killed(pid_file: &Path) {
    let pid = std::fs::read_to_string(pid_file).expect("pid file");
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "agy child {pid} is still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kills the process whose pid the script wrote to `pid_file`, so a test leaves nothing running.
fn kill_leftover(pid_file: &Path) {
    if let Ok(pid) = std::fs::read_to_string(pid_file) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", pid.trim()])
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
async fn runs_the_fixed_argv_with_the_text_as_one_argument_in_the_folder() {
    let dir = tempfile::tempdir().expect("temp dir");
    // One NUL-terminated record per argument: the text holds newlines and quotes.
    let bin = fake_agy(
        dir.path(),
        r#"printf '%s\000' "$@" > argv.bin
echo '{"conversation_id":"c1","status":"SUCCESS","response":"ok"}'"#,
    );
    let text = "hello \"world\"\n  it's $HOME `date` ; rm -rf /\nline three";
    let (res, reply) = deliver_one(&options(bin, dir.path()), text, CancellationToken::new()).await;

    assert!(res.is_ok());
    assert_eq!(reply, Some(true));
    let raw = std::fs::read_to_string(dir.path().join("argv.bin")).expect("argv recorded in cwd");
    let args: Vec<&str> = raw
        .strip_suffix('\0')
        .expect("records end with NUL")
        .split('\0')
        .collect();
    assert_eq!(
        args,
        [
            "-p",
            text,
            "--conversation",
            "conv-1",
            "--output-format",
            "json"
        ]
    );
}

#[tokio::test]
async fn envelope_status_error_with_exit_zero_is_a_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(
        dir.path(),
        r#"echo '{"conversation_id":"c1","status":"ERROR","response":"no"}'"#,
    );
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    assert!(res.is_ok());
    assert_eq!(reply, Some(false));
}

#[tokio::test]
async fn status_success_in_any_letter_case_is_a_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(dir.path(), r#"echo '{"status":"success"}'"#);
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    assert!(res.is_ok());
    assert_eq!(reply, Some(true));
}

#[tokio::test]
async fn only_the_word_success_counts_so_completed_is_a_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(dir.path(), r#"echo '{"status":"COMPLETED"}'"#);
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    assert!(res.is_ok());
    assert_eq!(reply, Some(false));
}

#[tokio::test]
async fn nonzero_exit_is_a_failure_even_when_the_envelope_says_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(
        dir.path(),
        "echo '{\"status\":\"SUCCESS\"}'\necho boom >&2\nexit 1",
    );
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    assert!(res.is_ok());
    assert_eq!(reply, Some(false));
}

#[tokio::test]
async fn timeout_kills_the_run_and_counts_it_as_handed_over() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(dir.path(), "echo $$ > pid.txt\nexec sleep 5");
    let mut opts = options(bin, dir.path());
    opts.timeout = Duration::from_millis(300);

    let started = Instant::now();
    let (res, reply) = deliver_one(&opts, "hi", CancellationToken::new()).await;

    assert!(res.is_ok());
    assert_eq!(reply, Some(true), "a timed-out run is not re-offered");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the run was cut at the timeout, not at its 5 s sleep"
    );
    assert_killed(&dir.path().join("pid.txt"));
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().expect("temp dir");
    // The background job shares the run's process group, so the group kill reaches it too.
    let bin = fake_agy(dir.path(), "sleep 30 &\necho $! > bg.pid\nsleep 30");
    let mut opts = options(bin, dir.path());
    opts.timeout = Duration::from_secs(1);

    let started = Instant::now();
    let (res, reply) = deliver_one(&opts, "hi", CancellationToken::new()).await;

    assert!(res.is_ok());
    assert_eq!(reply, Some(true));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the run was cut at the timeout, not at its 30 s sleeps"
    );
    assert_killed(&dir.path().join("bg.pid"));
}

#[tokio::test]
async fn a_leftover_background_process_does_not_hold_up_the_reply() {
    let dir = tempfile::tempdir().expect("temp dir");
    // The background job inherits stdout and stderr, so both pipes stay open after agy exits.
    let bin = fake_agy(
        dir.path(),
        "sleep 30 &\necho $! > bg.pid\necho '{\"status\":\"SUCCESS\"}'",
    );

    let started = Instant::now();
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    kill_leftover(&dir.path().join("bg.pid"));

    assert!(res.is_ok());
    assert_eq!(reply, Some(true));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the reply waited for the leftover process instead of the pipe grace"
    );
}

#[tokio::test]
async fn non_json_stdout_with_exit_zero_is_a_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(dir.path(), "echo 'plain text, not an envelope'");
    let (res, reply) = deliver_one(&options(bin, dir.path()), "hi", CancellationToken::new()).await;
    assert!(res.is_ok());
    assert_eq!(reply, Some(true));
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let opts = options(dir.path().join("no-such-agy"), dir.path());
    let (res, reply) = deliver_one(&opts, "hi", CancellationToken::new()).await;
    assert!(matches!(res, Err(BridgeError::Spawn(_))), "{res:?}");
    assert_eq!(reply, Some(false));
}

#[tokio::test]
async fn shutdown_during_a_run_kills_it_and_returns_ok_with_false() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = fake_agy(dir.path(), "echo $$ > pid.txt\ntouch started\nexec sleep 5");
    let opts = options(bin, dir.path());
    let shutdown = CancellationToken::new();
    let (tx, rx) = mpsc::channel(1);
    let (reply, answer) = oneshot::channel();
    tx.send(Delivery {
        text: "hi".to_string(),
        max_seq: 1,
        count: 1,
        reply,
    })
    .await
    .expect("driver is listening");
    // The sender stays open: only the shutdown may end `drive`.
    let _open = tx;
    let driver = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { drive(&opts, rx, shutdown).await }
    });

    // Cancel only once the run has started, so the shutdown hits a running child.
    let started_marker = dir.path().join("started");
    tokio::time::timeout(T, async {
        while !started_marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fake agy started");
    let cancelled_at = Instant::now();
    shutdown.cancel();

    let res = tokio::time::timeout(T, driver)
        .await
        .expect("drive returns in time")
        .expect("driver task did not panic");
    assert!(res.is_ok());
    assert!(cancelled_at.elapsed() < Duration::from_secs(4));
    assert_eq!(answer.await.ok(), Some(false));
    assert_killed(&dir.path().join("pid.txt"));
}
