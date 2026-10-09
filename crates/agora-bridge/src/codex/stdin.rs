//! The child stdin as a shared, closable writer for the codex RPC connection.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::process::ChildStdin;

/// The child's stdin behind a shared handle. The RPC writer task outlives the driver, so dropping
/// the connection does not close stdin; `close` drops the pipe, which is what lets the app-server
/// see EOF.
#[derive(Clone)]
pub(super) struct StdinHandle(Arc<Mutex<Option<ChildStdin>>>);

impl StdinHandle {
    /// Wraps the child's stdin (`None` when it was not piped).
    pub(super) fn new(stdin: Option<ChildStdin>) -> Self {
        Self(Arc::new(Mutex::new(stdin)))
    }

    /// Drop the pipe. Later writes fail with `BrokenPipe`.
    pub(super) fn close(&self) {
        let taken = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(taken);
    }
}

impl AsyncWrite for StdinHandle {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, buf),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}
