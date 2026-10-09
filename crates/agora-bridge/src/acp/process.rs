//! The child process of the ACP bridge: spawn `copilot --acp --stdio`, run the driver and the pump
//! over its pipes until one ends, and reap the child. Kept apart from the protocol in `acp.rs`.

use super::{AcpOptions, drive};
use crate::jsonrpc::RpcConn;
use crate::pump::{self, PumpConfig};
use crate::{BridgeError, Delivery, SessionReady};
use std::io;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// How long the child may take to exit on its own once its stdin is closed (EOF), before it is
/// killed.
const STDIN_CLOSE_GRACE: Duration = Duration::from_secs(2);
/// How long the child may take to exit once it was killed.
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn `<bin> --acp --stdio` in its own process group (kill_on_drop, stderr inherited), handshake,
/// create/load the session, report the session id through `on_session`, and run `drive` plus
/// `pump::run` (folder = `opts.cwd`) until the child exits or `shutdown` fires. Returns Ok on a
/// clean end. Shutdown during setup ends the driver with Ok without waiting for the agent's answers.
/// Ending the bridge closes the child's stdin, waits 2 s for it to exit, then kills its whole
/// process group, so what the child started dies with it.
pub async fn run(
    opts: AcpOptions,
    shutdown: CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    let mut child = Command::new(&opts.bin)
        .args(["--acp", "--stdio"])
        .current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // The child's pid is its process group id, so a kill can reach its descendants.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BridgeError::Spawn(format!("{}: {e}", opts.bin.display())))?;
    let stdin = StdinGate::new(child.stdin.take());
    let result = serve(&mut child, stdin.clone(), &opts, &shutdown, on_session).await;
    reap(&mut child, &stdin).await;
    result
}

/// Everything `run` does once the child is up: the driver and the pump, until one of them ends.
async fn serve(
    child: &mut Child,
    stdin: StdinGate,
    opts: &AcpOptions,
    shutdown: &CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    let Some(stdout) = child.stdout.take() else {
        return Err(BridgeError::Protocol("--acp stdout is not piped".into()));
    };
    let (conn, incoming) = RpcConn::spawn(stdout, stdin, true);
    let stop = shutdown.child_token();
    let (ready_tx, mut ready_rx) = oneshot::channel();
    let (delivery_tx, delivery_rx) = mpsc::channel::<Delivery>(1);
    let (drive_opts, drive_stop) = (opts.clone(), stop.clone());
    let mut driver = tokio::spawn(async move {
        drive(
            conn,
            incoming,
            &drive_opts,
            ready_tx,
            delivery_rx,
            drive_stop,
        )
        .await
    });
    let session_id = tokio::select! {
        ready = &mut ready_rx => match ready {
            Ok(session_id) => session_id,
            // `ready` is dropped only when the driver has returned: with an error, or because
            // shutdown arrived during setup (then the outcome is Ok, and so is the bridge's).
            Err(_) => {
                return match joined(driver.await) {
                    Ok(()) if shutdown.is_cancelled() => Ok(()),
                    Ok(()) => Err(BridgeError::Protocol(
                        "copilot ended before the session was ready".into(),
                    )),
                    Err(e) => Err(e),
                };
            }
        },
        _ = shutdown.cancelled() => {
            // `stop` is a child of `shutdown`, so the driver is already stopping.
            if let Err(e) = joined(driver.await) {
                tracing::warn!(error = %e, "copilot --acp ended with an error during setup");
            }
            return Ok(());
        }
    };
    let mut pump_task = tokio::spawn(pump::run(
        PumpConfig::new(opts.home.clone(), opts.cwd.clone(), session_id.clone())
            .with_harness(agora_deliver::Harness::CopilotCli),
        delivery_tx,
        stop.clone(),
    ));
    if let Some(tx) = on_session {
        let _ = tx.send(session_id);
    }
    // The pump only ends when stopped or when it fails; either way the driver stops too.
    let (driven, pumped) = tokio::select! {
        joined_driven = &mut driver => {
            stop.cancel();
            (joined(joined_driven), joined(pump_task.await))
        }
        joined_pumped = &mut pump_task => {
            stop.cancel();
            (joined(driver.await), joined(joined_pumped))
        }
    };
    driven.and(pumped)
}

/// Close the child's stdin, give it `STDIN_CLOSE_GRACE` to exit on EOF, then kill its process group
/// and wait, bounded by `CHILD_EXIT_TIMEOUT`.
async fn reap(child: &mut Child, stdin: &StdinGate) {
    stdin.close();
    if let Ok(outcome) = tokio::time::timeout(STDIN_CLOSE_GRACE, child.wait()).await {
        return log_exit(outcome);
    }
    kill_group(child);
    match tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait()).await {
        Ok(outcome) => log_exit(outcome),
        Err(_) => tracing::warn!("copilot --acp did not exit after kill"),
    }
}

/// SIGKILL the child's process group. Only called after a `wait` on the child timed out, so the
/// child has not been reaped and its pid still names the group. Without a pid, kill the child alone.
fn kill_group(child: &mut Child) {
    let Some(pgid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) else {
        let _ = child.start_kill();
        return;
    };
    // SAFETY: `killpg` only sends a signal and touches no memory. `pgid` is the pid of the child this
    // bridge spawned as its own group leader, and it is not yet reaped, so it cannot name another
    // process.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let err = io::Error::last_os_error();
        // ESRCH: the group is already gone, which is the state a kill wants.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(error = %err, "could not kill copilot's process group");
        }
    }
}

fn log_exit(outcome: io::Result<ExitStatus>) {
    match outcome {
        Ok(status) => tracing::debug!(%status, "copilot --acp exited"),
        Err(e) => tracing::warn!(error = %e, "waiting for copilot --acp failed"),
    }
}

/// A task's result as the bridge's result; a panicked or cancelled task is a protocol error.
fn joined<T>(outcome: Result<Result<T, BridgeError>, JoinError>) -> Result<T, BridgeError> {
    match outcome {
        Ok(result) => result,
        Err(e) => Err(BridgeError::Protocol(format!("bridge task failed: {e}"))),
    }
}

/// The child's stdin, shared by the connection's writer (which keeps it for as long as the
/// connection lives) and `reap`. Dropping the `ChildStdin` is what sends EOF, so `close` takes it
/// out of the gate. Writes after `close` fail with `BrokenPipe`.
#[derive(Clone)]
struct StdinGate(Arc<Mutex<Option<ChildStdin>>>);

impl StdinGate {
    fn new(stdin: Option<ChildStdin>) -> Self {
        Self(Arc::new(Mutex::new(stdin)))
    }

    fn close(&self) {
        locked(&self.0).take();
    }
}

impl AsyncWrite for StdinGate {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match locked(&self.0).as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, buf),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match locked(&self.0).as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match locked(&self.0).as_mut() {
            Some(stdin) => Pin::new(stdin).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

/// Lock a std mutex, ignoring poisoning: the guarded value is never left half-updated.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
