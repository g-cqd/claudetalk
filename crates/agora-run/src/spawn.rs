//! Spawning the detached supervisor process for the daemon (D8).

use std::ffi::OsString;
use std::io::{Error, ErrorKind};
use std::process::Stdio;

use crate::jobdir::JobDir;

/// Spawn `argv[0] argv[1..] --job-dir <dir>` detached for the daemon: stdin/stdout/stderr null, env cleared except
/// PATH and HOME (the job env is in spec.json), kill_on_drop(false). The child calls setsid itself.
pub fn spawn_supervisor(argv: &[String], dir: &JobDir) -> std::io::Result<tokio::process::Child> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "empty supervisor argv"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest);
    cmd.arg("--job-dir");
    cmd.arg(&dir.dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.env_clear();
    for name in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(OsString::from(name), value);
        }
    }
    cmd.kill_on_drop(false);
    cmd.spawn()
}
