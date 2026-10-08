//! The stdio transport: newline-delimited JSON-RPC over an async reader/writer pair.

use crate::McpError;
use crate::engine::{self, Engine, Notifier, Peer, Transport};
use crate::jsonrpc::{self, INTERNAL_ERROR, Incoming, MAX_MESSAGE_BYTES, RpcError};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// Capacity of the outgoing queue shared by responses, progress and channel pushes.
const OUTPUT_QUEUE: usize = 256;
/// How long the writer may take to flush already-queued messages after the session ends.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// Request tasks in flight at once; a request over the limit is refused with -32603.
const MAX_CONCURRENT_REQUESTS: usize = 64;
/// After the input ends, how long requests already read may run before they are cancelled.
const EOF_GRACE: Duration = Duration::from_millis(500);

/// Serve one stdio session until `input` reaches EOF or `shutdown` fires.
///
/// Newline-delimited messages; empty lines are ignored; a line over `MAX_MESSAGE_BYTES` is
/// discarded up to its newline and answered with -32600 "message too large" (id null). Each
/// Request runs in its own task (a JoinSet, at most `MAX_CONCURRENT_REQUESTS` at once; over the
/// limit it is answered -32603 "too many concurrent requests"), except `initialize`, which is
/// handled inline so a pipelined `initialize` takes effect before the next line is read.
/// Notifications are handled inline (`notifications/cancelled` cancels `peer.inflight[requestId]`,
/// and that request gets no response). A `tools/call` is registered in `peer.inflight` by this
/// reader before its task starts, so a cancel read right behind it always finds it. Each message
/// runs in an inner task: a panic there answers a Request with -32603. ONE writer task owns
/// `output` (an `mpsc<Value>` of capacity 256; responses are sent with `send().await`; the
/// `Notifier` handed to the engine is built on the same channel). On EOF, requests already read
/// get `EOF_GRACE` to answer. On EOF or shutdown: cancel `peer.shutdown`, abort in-flight tasks,
/// flush the writer, return Ok. Output is exactly one JSON document per line, nothing else.
pub async fn serve<R, W>(
    engine: Arc<Engine>,
    peer: Arc<Peer>,
    input: R,
    output: W,
    shutdown: CancellationToken,
) -> Result<(), McpError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ending = CancellationToken::new();
    let (tx, rx) = mpsc::channel::<Value>(OUTPUT_QUEUE);
    let mut writer = tokio::spawn(write_loop(output, rx, ending.clone()));
    let notifier = Notifier::new(tx.clone());
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    let mut tasks: JoinSet<()> = JoinSet::new();
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let mut input_ended = false;

    loop {
        while let Some(joined) = tasks.try_join_next() {
            log_task_end(joined);
        }
        let frame = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ending.cancelled() => break,
            frame = read_frame(&mut input, &mut line) => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => {
                tracing::info!(error = %e, "stdin read failed; ending the session");
                input_ended = true;
                break;
            }
        };
        match frame {
            Frame::Eof => {
                tracing::info!("stdin closed; ending the session");
                input_ended = true;
                break;
            }
            Frame::TooLarge => {
                tracing::debug!("discarded a message over the size limit");
                let error = RpcError::invalid_request("message too large");
                if tx
                    .send(jsonrpc::error_response(&Value::Null, &error))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Frame::Line => {
                let bytes = line.strip_suffix(b"\r").unwrap_or(line.as_slice());
                if bytes.trim_ascii().is_empty() {
                    continue;
                }
                let msg = jsonrpc::parse(bytes);
                let spawned = match &msg {
                    Incoming::Request { id, method, .. } if method != "initialize" => {
                        Some((id.clone(), method == "tools/call"))
                    }
                    _ => None,
                };
                if let Some((id, is_call)) = spawned {
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        let error = RpcError::new(INTERNAL_ERROR, "too many concurrent requests");
                        if tx.send(jsonrpc::error_response(&id, &error)).await.is_err() {
                            break;
                        }
                        continue;
                    };
                    // Registered here, before the task exists, so that a cancel read right
                    // behind this request cannot miss it (R3-1).
                    let tracked = is_call && peer.transport == Transport::Stdio;
                    if tracked {
                        engine::register_inflight(&peer, &id);
                    }
                    let engine = Arc::clone(&engine);
                    let peer = Arc::clone(&peer);
                    let notifier = notifier.clone();
                    let tx = tx.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let response = run_guarded(&engine, &peer, msg, &notifier).await;
                        if tracked {
                            engine::release_inflight(&peer, &id);
                        }
                        if let Some(response) = response {
                            let _ = tx.send(response).await;
                        }
                    });
                } else if let Some(response) = run_guarded(&engine, &peer, msg, &notifier).await
                    && tx.send(response).await.is_err()
                {
                    break;
                }
            }
        }
    }

    if input_ended {
        // The input ended, not the session: requests already read may still answer.
        let _ = tokio::time::timeout(EOF_GRACE, drain(&mut tasks)).await;
    }
    peer.shutdown.cancel();
    tasks.abort_all();
    drain(&mut tasks).await;
    // Only the push task's Notifier clone can outlive this point; it ends with `peer.shutdown`.
    drop(notifier);
    drop(tx);
    match tokio::time::timeout(FLUSH_TIMEOUT, &mut writer).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "stdio writer task failed"),
        Err(_) => {
            writer.abort();
            tracing::warn!("stdout did not drain in time; dropping queued messages");
        }
    }
    Ok(())
}

/// The single writer: one compact JSON document per line, flushed after each message. Ends when
/// every sender is dropped, or on a write error (which cancels `ending` so the reader stops).
async fn write_loop<W>(mut output: W, mut rx: mpsc::Receiver<Value>, ending: CancellationToken)
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = rx.recv().await {
        let mut line = match serde_json::to_string(&message) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!(error = %e, "cannot encode an outgoing message");
                continue;
            }
        };
        line.push('\n');
        let written = async {
            output.write_all(line.as_bytes()).await?;
            output.flush().await
        };
        if let Err(e) = written.await {
            tracing::debug!(error = %e, "stdout write failed; ending the session");
            break;
        }
    }
    ending.cancel();
}

/// Runs one message through the engine in an inner task, so that a panic in a handler is caught
/// and logged (R3-3). A Request whose handler panicked is answered -32603; the inner task is
/// aborted if the caller is dropped first.
async fn run_guarded(
    engine: &Arc<Engine>,
    peer: &Arc<Peer>,
    msg: Incoming,
    notifier: &Notifier,
) -> Option<Value> {
    let id = match &msg {
        Incoming::Request { id, .. } => Some(id.clone()),
        _ => None,
    };
    let engine = Arc::clone(engine);
    let peer = Arc::clone(peer);
    let notifier = notifier.clone();
    let mut work = AbortOnDrop(tokio::spawn(async move {
        engine.handle(&peer, msg, &notifier, None).await
    }));
    match (&mut work.0).await {
        Ok(response) => response,
        Err(e) if e.is_panic() => {
            tracing::error!(error = %e, "request handler panicked");
            id.map(|id| jsonrpc::error_response(&id, &RpcError::internal()))
        }
        Err(_) => None,
    }
}

/// Aborts its task when dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Waits for every request task to end, logging any panic.
async fn drain(tasks: &mut JoinSet<()>) {
    while let Some(joined) = tasks.join_next().await {
        log_task_end(joined);
    }
}

/// One framed unit read from the input.
enum Frame {
    /// End of input with nothing pending.
    Eof,
    /// A complete line (without its `\n`) is in the read buffer.
    Line,
    /// A line over `MAX_MESSAGE_BYTES`; its bytes were discarded.
    TooLarge,
}

/// Read one newline-terminated message into `buf`, never holding more than
/// `MAX_MESSAGE_BYTES` of it. An oversized line is consumed up to its newline without being
/// buffered. A final line without a newline is returned as a line.
async fn read_frame<I>(input: &mut I, buf: &mut Vec<u8>) -> std::io::Result<Frame>
where
    I: AsyncBufRead + Unpin,
{
    buf.clear();
    let mut oversized = false;
    loop {
        let chunk = input.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if oversized {
                Frame::TooLarge
            } else if buf.is_empty() {
                Frame::Eof
            } else {
                Frame::Line
            });
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let content_len = newline.unwrap_or(chunk.len());
        let consumed = newline.map_or(chunk.len(), |i| i.saturating_add(1));
        if !oversized {
            let content = chunk.get(..content_len).unwrap_or_default();
            if buf.len().saturating_add(content.len()) > MAX_MESSAGE_BYTES {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(content);
            }
        }
        input.consume(consumed);
        if newline.is_some() {
            return Ok(if oversized {
                Frame::TooLarge
            } else {
                Frame::Line
            });
        }
    }
}

/// Logs a request task that panicked; a cancelled (aborted) task is the normal end of a session.
fn log_task_end(joined: Result<(), JoinError>) {
    match joined {
        Err(e) if e.is_panic() => tracing::error!(error = %e, "request task panicked"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, Transport};
    use agora_client::Ctx;
    use agora_crypto::Identity;
    use agora_daemon::Paths;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use tokio::io::{DuplexStream, Lines};
    use tokio::task::JoinHandle;

    /// A session driven over in-memory pipes; the engine never starts the push link.
    struct Session {
        _home: tempfile::TempDir,
        input: DuplexStream,
        output: Lines<BufReader<DuplexStream>>,
        task: JoinHandle<Result<(), McpError>>,
        shutdown: CancellationToken,
    }

    fn start() -> Session {
        let home = tempfile::tempdir().expect("tempdir");
        let ctx = Ctx {
            paths: Paths::new(home.path()),
            identity: Identity::from_seed([9; 32]),
            label: "t".into(),
        };
        let engine = Engine::new(EngineConfig {
            channel_push: false,
            ..EngineConfig::stdio()
        });
        let peer = Arc::new(Peer::new(Arc::new(ctx), Transport::Stdio, None));
        peer.link_started.store(true, Ordering::SeqCst);
        let (input, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, output) = tokio::io::duplex(64 * 1024);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(serve(engine, peer, server_in, server_out, shutdown.clone()));
        Session {
            _home: home,
            input,
            output: BufReader::new(output).lines(),
            task,
            shutdown,
        }
    }

    impl Session {
        async fn write(&mut self, bytes: &[u8]) {
            tokio::time::timeout(Duration::from_secs(5), self.input.write_all(bytes))
                .await
                .expect("write timed out")
                .expect("write failed");
        }

        async fn send(&mut self, message: &Value) {
            let mut line = serde_json::to_string(message).expect("encode");
            line.push('\n');
            self.write(line.as_bytes()).await;
        }

        /// The next line the server wrote, parsed.
        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(5), self.output.next_line())
                .await
                .expect("no output within 5 s")
                .expect("read failed")
                .expect("output closed");
            serde_json::from_str(&line).expect("server wrote invalid JSON")
        }
    }

    fn ping(id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ping_is_answered() {
        let mut s = start();
        s.send(&ping(1)).await;
        let reply = s.next().await;
        assert_eq!(reply["id"], json!(1));
        assert!(reply.get("result").is_some(), "{reply}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parse_error_is_answered_with_null_id() {
        let mut s = start();
        s.write(b"{not json\n").await;
        let reply = s.next().await;
        assert_eq!(jsonrpc::error_code(&reply), Some(-32700));
        assert_eq!(reply["id"], Value::Null);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn empty_lines_are_ignored() {
        let mut s = start();
        s.write(b"\n   \n\r\n").await;
        s.send(&ping(2)).await;
        let reply = s.next().await;
        assert_eq!(reply["id"], json!(2), "blank lines must produce no output");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn oversized_line_is_discarded_and_the_next_message_is_answered() {
        let mut s = start();
        let mut big = vec![b'a'; MAX_MESSAGE_BYTES + 10];
        big.push(b'\n');
        s.write(&big).await;
        let reply = s.next().await;
        assert_eq!(jsonrpc::error_code(&reply), Some(-32600));
        assert_eq!(reply["id"], Value::Null);
        s.send(&ping(3)).await;
        let reply = s.next().await;
        assert_eq!(reply["id"], json!(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_requests_are_answered_with_matching_ids() {
        let mut s = start();
        s.send(&ping(1)).await;
        s.send(&json!({"jsonrpc": "2.0", "id": "two", "method": "ping"}))
            .await;
        // Requests run concurrently, so the replies may arrive in either order.
        let first = s.next().await["id"].clone();
        let second = s.next().await["id"].clone();
        let mut ids = vec![first, second];
        ids.sort_by_key(|id| id.to_string());
        assert_eq!(ids, vec![json!("two"), json!(1)]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn notification_produces_no_output() {
        let mut s = start();
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        // Notifications are handled inline, before the next line is read: any reply to the
        // notification would be written before the ping's reply, so the ping is the barrier.
        s.send(&ping(4)).await;
        let reply = s.next().await;
        assert_eq!(reply["id"], json!(4));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eof_ends_serve_within_two_seconds() {
        let s = start();
        let Session { input, task, .. } = s;
        drop(input);
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("serve did not end within 2 s")
            .expect("serve task panicked");
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_ends_serve() {
        let s = start();
        let Session { shutdown, task, .. } = s;
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("serve did not end within 2 s")
            .expect("serve task panicked");
        assert!(result.is_ok());
    }
}
