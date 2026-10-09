//! `run` against a stand-in `copilot` that starts a grandchild: when the bridge kills the child, the
//! grandchild must die too.

use super::run;
use crate::acp::AcpOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long the test waits for the stand-in's pid file, the bridge's exit, and the grandchild's death.
const WAIT: Duration = Duration::from_secs(10);

/// Writes an executable `copilot` stand-in. It starts a `sleep` (the grandchild), records its pid in
/// `pid_file`, and waits. It ignores stdin EOF, so only a kill ends it.
fn fake_copilot(dir: &Path, pid_file: &Path) -> PathBuf {
    let script = dir.join("copilot");
    let body = format!(
        "#!/bin/sh\nsleep 600 &\necho $! > '{}'\nwait\n",
        pid_file.display()
    );
    std::fs::write(&script, body).expect("write the stand-in");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in executable");
    script
}

/// The grandchild's pid, once the stand-in has written it.
async fn grandchild_pid(pid_file: &Path) -> libc::pid_t {
    tokio::time::timeout(WAIT, async {
        loop {
            let pid = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok());
            if let Some(pid) = pid {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stand-in never wrote its pid")
}

/// Whether `pid` names a process that exists (a zombie counts).
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the pid exists; it sends nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn ending_the_child_kills_what_it_started() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pid_file = dir.path().join("grandchild.pid");
    let opts = AcpOptions {
        bin: fake_copilot(dir.path(), &pid_file),
        cwd: dir.path().to_owned(),
        session: None,
        prompt: None,
        allow_tools: false,
        exe: PathBuf::from("/unused/agora"),
        home: PathBuf::from("/unused/agora-home"),
    };
    let shutdown = CancellationToken::new();
    let bridge = tokio::spawn(run(opts, shutdown.clone(), None));
    let grandchild = grandchild_pid(&pid_file).await;
    assert!(alive(grandchild), "the grandchild should be running");
    // Shutdown during setup: the stand-in never answers `initialize` and ignores stdin EOF, so the
    // bridge gives it the grace period and then kills its process group.
    shutdown.cancel();
    let outcome = tokio::time::timeout(WAIT, bridge)
        .await
        .expect("the bridge did not end")
        .expect("the bridge task panicked");
    assert!(outcome.is_ok(), "{outcome:?}");
    tokio::time::timeout(WAIT, async {
        while alive(grandchild) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the grandchild survived the kill");
}
