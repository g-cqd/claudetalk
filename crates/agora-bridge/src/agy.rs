//! Antigravity (`agy`) bridge (DOCS-ONLY, antigravity.google/docs/cli/headless). `agy` is not a
//! resident process: each delivery runs `<bin> -p <text> --conversation <id> --output-format json`
//! (argv, no shell; cwd = `cwd`; stdin null; stdout captured, capped at 1 MiB). Deliveries are
//! sequential. Exit 0 and JSON `status` absent, or equal to "success" ignoring ASCII case, -> reply
//! true. Only that word counts: any other status (`COMPLETED`, `ERROR`, ...) -> false (logged).
//! Timeout -> kill the run's process group, true: the text reached the conversation, and a re-offer
//! would repeat the run's side effects. Session key = the conversation id. The
//! `OwnedSessionApi` beat continues only while the bridge runs.

use crate::pump::{self, PumpConfig};
use crate::{BridgeError, Delivery, SessionReady};
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

/// Default per-delivery timeout (10 minutes).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Most stdout kept per run (bytes). Longer output is read to its end and the rest dropped.
const STDOUT_CAP: usize = 1024 * 1024;
/// Most stderr kept per run (bytes). Longer output is read to its end and the rest dropped.
const STDERR_CAP: usize = 64 * 1024;
/// Characters of stderr quoted in a failure log line.
const STDERR_LOG_CHARS: usize = 200;
/// Longest wait for a killed `agy` to be reaped.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// Longest wait, once the child has exited, for its pipes to reach their end. A leftover process
/// (a background job, say) can hold a pipe open; the output read by then is judged.
const PIPE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct AgyOptions {
    /// The `agy` binary.
    pub bin: PathBuf,
    /// Working directory of every `agy` run; also the identity folder.
    pub cwd: PathBuf,
    /// The conversation to deliver into (required).
    pub conversation: String,
    /// Per-delivery timeout (`DEFAULT_TIMEOUT`). A run that reaches it is killed and counted as
    /// handed over.
    pub timeout: Duration,
}

impl AgyOptions {
    /// `timeout` = `DEFAULT_TIMEOUT`.
    pub fn new(bin: PathBuf, cwd: PathBuf, conversation: String) -> Self {
        Self {
            bin,
            cwd,
            conversation,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Report the conversation id through `on_session` (at once: agy conversations are created by the
/// user beforehand), run `pump::run` (folder = `opts.cwd`) and execute each `Delivery` via `drive`
/// until `shutdown` fires. A pump failure (no identity for `opts.cwd`) is returned once the driver
/// has stopped.
pub async fn run(
    opts: AgyOptions,
    home: PathBuf,
    shutdown: CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    if let Some(tx) = on_session {
        // A dropped receiver only means nobody asked for the id.
        let _ = tx.send(opts.conversation.clone());
    }
    let stop = shutdown.child_token();
    let (delivery_tx, delivery_rx) = mpsc::channel(1);
    let pump_task = tokio::spawn(pump::run(
        PumpConfig::new(home, opts.cwd.clone(), opts.conversation.clone())
            .with_harness(agora_deliver::Harness::Antigravity),
        delivery_tx,
        stop.clone(),
    ));
    let driven = drive(&opts, delivery_rx, stop.clone()).await;
    // The driver ends on shutdown or once the pump has dropped its sender; either way the pump
    // stops too.
    stop.cancel();
    driven.and(joined(pump_task.await))
}

/// Runs each delivery as one `agy -p` call, in arrival order, until `deliveries` closes or
/// `shutdown` fires. A delivery is answered `true` when its run succeeded or timed out (see
/// `execute`).
/// Shutdown kills a running child and answers its delivery `false`. A child that cannot start
/// answers `false` and returns `BridgeError::Spawn`.
pub async fn drive(
    opts: &AgyOptions,
    mut deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    loop {
        let next = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(()),
            next = deliveries.recv() => next,
        };
        let Some(Delivery { text, reply, .. }) = next else {
            return Ok(());
        };
        match execute(opts, &text, &shutdown).await {
            Ok(accepted) => {
                let _ = reply.send(accepted);
            }
            Err(err) => {
                let _ = reply.send(false);
                return Err(err);
            }
        }
    }
}

/// One `agy -p` run for `text`. `Ok(true)` when it succeeded or timed out; `Ok(false)` when it
/// failed or was stopped by `shutdown` (the run's process group is killed in each case). `Err` only
/// when the child could not be started.
///
/// The child leads its own process group, so a kill reaches the background processes it started.
/// The prompt text travels in argv, so `ps` shows it to other local users while the run lasts.
/// That is a CLI limitation: `agy -p` takes the text as an argument.
async fn execute(
    opts: &AgyOptions,
    text: &str,
    shutdown: &CancellationToken,
) -> Result<bool, BridgeError> {
    let mut child = Command::new(&opts.bin)
        .args([
            "-p",
            text,
            "--conversation",
            &opts.conversation,
            "--output-format",
            "json",
        ])
        .current_dir(&opts.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BridgeError::Spawn(format!("{}: {e}", opts.bin.display())))?;
    // `process_group(0)` makes the child's pid its group id; without a pid there is no group.
    let Some(pgid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) else {
        return Err(BridgeError::Protocol("agy has no process id".into()));
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill(pgid, &mut child).await;
        return Err(BridgeError::Protocol("agy stdio is not piped".into()));
    };
    // Each pipe has its own task, so neither can fill and block the child. The tasks write into
    // shared sinks, so what they read survives their abort.
    let out_sink = Arc::new(Mutex::new(Output::default()));
    let err_sink = Arc::new(Mutex::new(Output::default()));
    let mut out_task = tokio::spawn(read_capped(stdout, STDOUT_CAP, out_sink.clone()));
    let mut err_task = tokio::spawn(read_capped(stderr, STDERR_CAP, err_sink.clone()));
    let finished = {
        let run = async {
            let status = child.wait().await;
            // The pipes end with the child, unless a leftover process still holds them.
            let reads = tokio::join!(settle(&mut out_task), settle(&mut err_task));
            (status, reads)
        };
        tokio::select! {
            biased;
            () = shutdown.cancelled() => None,
            timed = tokio::time::timeout(opts.timeout, run) => Some(timed),
        }
    };
    out_task.abort();
    err_task.abort();
    let (status, (out_read, err_read)) = match finished {
        Some(Ok(parts)) => parts,
        Some(Err(_elapsed)) => {
            kill(pgid, &mut child).await;
            tracing::warn!(
                timeout = ?opts.timeout,
                "agy run timed out and was killed; its batch counts as handed over"
            );
            return Ok(true);
        }
        None => {
            kill(pgid, &mut child).await;
            tracing::debug!("agy delivery stopped by shutdown; run killed");
            return Ok(false);
        }
    };
    let status = match (status, out_read, err_read) {
        (Ok(status), Ok(()), Ok(())) => status,
        (status, out_read, err_read) => {
            let cause = status.err().or(out_read.err()).or(err_read.err());
            kill(pgid, &mut child).await;
            tracing::warn!(error = ?cause, "agy run could not be read; counted as failed");
            return Ok(false);
        }
    };
    let out = take_output(&out_sink);
    let err = take_output(&err_sink);
    if out.truncated {
        tracing::warn!(
            cap = STDOUT_CAP,
            "agy stdout exceeded the cap; judged on its first {STDOUT_CAP} bytes"
        );
    }
    let reported = reported_status(&out.bytes);
    let ok = status.success()
        && reported
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case("success"));
    if ok {
        tracing::debug!("agy delivery succeeded");
    } else {
        let stderr_head: String = String::from_utf8_lossy(&err.bytes)
            .chars()
            .take(STDERR_LOG_CHARS)
            .collect();
        tracing::warn!(
            exit = %status,
            status = ?reported,
            stderr = ?stderr_head,
            "agy delivery failed"
        );
    }
    Ok(ok)
}

/// The `status` of an `agy` JSON envelope: its text for a string, its JSON form otherwise. `None`
/// when stdout is not a JSON object or has no `status` member.
fn reported_status(stdout: &[u8]) -> Option<String> {
    let Ok(Value::Object(envelope)) = serde_json::from_slice::<Value>(stdout) else {
        return None;
    };
    match envelope.get("status")? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// What one pipe held: the first `STDOUT_CAP`/`STDERR_CAP` bytes, and whether more followed.
#[derive(Debug, Default)]
struct Output {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Takes what a pipe reader has stored so far.
fn take_output(sink: &Mutex<Output>) -> Output {
    std::mem::take(&mut sink.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Reads `reader` to its end, keeping the first `cap` bytes in `sink`. Bytes past the cap are read
/// and dropped, so the child never blocks on a full pipe. The sink is filled chunk by chunk, so
/// what was read survives the task being aborted.
async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    sink: Arc<Mutex<Output>>,
) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        let mut out = sink.lock().unwrap_or_else(PoisonError::into_inner);
        let room = cap - out.bytes.len();
        out.truncated |= n > room;
        out.bytes.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Waits up to `PIPE_GRACE` for a pipe reader. A reader still running after that is aborted by
/// `execute`; the bytes it has read still count.
async fn settle(task: &mut JoinHandle<io::Result<()>>) -> io::Result<()> {
    match tokio::time::timeout(PIPE_GRACE, task).await {
        Ok(Ok(read)) => read,
        Ok(Err(joined)) => Err(io::Error::other(joined)),
        Err(_elapsed) => {
            tracing::debug!("agy pipe still open after the run; output read so far is kept");
            Ok(())
        }
    }
}

/// Kills the process group `pgid` (the child and every process it started in that group), then
/// waits, for at most `KILL_WAIT`, for the child to be reaped.
async fn kill(pgid: libc::pid_t, child: &mut Child) {
    // SAFETY: `killpg` only sends a signal and touches no memory. `pgid` is the pid of a child this
    // bridge spawned as its own group leader, so it names that group.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let err = io::Error::last_os_error();
        // ESRCH: the group is already gone, which is the state a kill wants.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(error = %err, "could not kill agy's process group");
        }
    }
    match tokio::time::timeout(KILL_WAIT, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "waiting for agy failed"),
        Err(_) => tracing::warn!("agy did not exit after kill"),
    }
}

/// A task's result as the bridge's result; a panicked or cancelled task is a protocol error.
fn joined<T>(outcome: Result<Result<T, BridgeError>, JoinError>) -> Result<T, BridgeError> {
    match outcome {
        Ok(result) => result,
        Err(e) => Err(BridgeError::Protocol(format!("bridge task failed: {e}"))),
    }
}
