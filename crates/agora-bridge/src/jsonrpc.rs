//! Line-delimited JSON-RPC client over any `AsyncRead`/`AsyncWrite` (Codex app-server: no
//! `"jsonrpc"` field; ACP: `"jsonrpc":"2.0"`).
//!
//! Integer request ids from 1. A reader task routes responses to the pending requests and
//! everything else to the `Incoming` channel (capacity 1024; when full, notifications are dropped
//! with a tracing warn, requests are never dropped: the reader awaits). Blank lines are skipped
//! silently; lines longer than 8 MiB and invalid JSON are skipped with a warn. Reader EOF fails
//! all pending requests with `Closed` and closes the channel.

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Longest accepted line, newline excluded. Longer lines are skipped up to their newline.
pub(crate) const MAX_LINE: usize = 8 * 1024 * 1024;
/// Capacity of the channel that carries server requests and notifications to the caller.
///
/// Backpressure trade-off: server requests are sent with `send().await`, so when the channel is
/// full the reader task stops until the caller drains it. While the reader is stopped no
/// response is routed either, so a caller that awaits `request` without draining `Incoming`
/// stalls until its timeout. Notifications never stall the reader; they are dropped (with a
/// warn) when the channel is full. The cap is 1024 so that a burst of notifications during one
/// turn does not drop, while memory stays bounded.
const INCOMING_CAP: usize = 1024;
/// Capacity of the queue of serialized lines waiting for the writer task.
const OUTGOING_CAP: usize = 64;
/// Longest a `respond` / `respond_error` may wait for room to write. A writer that never drains
/// (the peer stopped reading) fails the caller with `Timeout` instead of stalling it forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// `-32603`, JSON-RPC "internal error": used when a remote error object has no usable code.
const INTERNAL_ERROR: i64 = -32603;

/// A message from the peer that is not a response to one of our requests.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// A server-to-client request: the handler must `respond` or `respond_error` with this `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    /// The connection ended (EOF or writer closed) before a response arrived.
    #[error("connection closed")]
    Closed,
    /// No response within the request timeout.
    #[error("timed out")]
    Timeout,
    /// The peer answered with a JSON-RPC error object.
    #[error("remote error {code}: {message}")]
    Remote { code: i64, message: String },
    #[error("io: {0}")]
    Io(String),
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

/// Removes a request's pending entry when the `request` future ends without the entry having
/// been taken by a response or by `close`: a timeout, a failed enqueue, or the caller dropping
/// the future mid-flight. Ids are never reused, so removing an absent entry is a no-op.
struct PendingEntry {
    pending: Arc<Mutex<HashMap<i64, Waiter>>>,
    id: i64,
}

impl Drop for PendingEntry {
    fn drop(&mut self) {
        locked(&self.pending).remove(&self.id);
    }
}

/// State shared by the caller handles and the two tasks of one connection.
struct Shared {
    next_id: AtomicI64,
    jsonrpc_field: bool,
    /// Set under the `pending` lock, so a request either sees it or is failed by `close`.
    closed: AtomicBool,
    pending: Arc<Mutex<HashMap<i64, Waiter>>>,
    /// `None` once closed. Dropping the last sender ends the writer task.
    outgoing: Mutex<Option<mpsc::Sender<String>>>,
    /// Bound on `respond` / `respond_error` writes; `WRITE_TIMEOUT` outside tests.
    write_timeout: Duration,
}

/// Cloneable handle to one connection.
#[derive(Clone)]
pub struct RpcConn {
    inner: Arc<Shared>,
}

impl RpcConn {
    /// Start the reader and writer tasks (needs a tokio runtime). `jsonrpc_field`: add
    /// `"jsonrpc":"2.0"` to every outgoing message (ACP: true; Codex app-server: false).
    pub fn spawn<R, W>(
        reader: R,
        writer: W,
        jsonrpc_field: bool,
    ) -> (RpcConn, tokio::sync::mpsc::Receiver<Incoming>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        spawn_with_cap(reader, writer, jsonrpc_field, MAX_LINE, WRITE_TIMEOUT)
    }

    /// Send a request and wait for its response `result`; `timeout` elapsed -> `Timeout`
    /// (the pending entry is removed); JSON-RPC error object -> `Remote`; connection gone -> `Closed`.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let shared = &self.inner;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let line = shared.frame(call_frame(Some(json!(id)), method, params))?;
        let (waiter, answer) = oneshot::channel();
        {
            let mut pending = locked(&shared.pending);
            if shared.closed.load(Ordering::SeqCst) {
                return Err(RpcError::Closed);
            }
            pending.insert(id, waiter);
        }
        // Armed before the first await, so every way out of this function, including the
        // caller dropping the future, removes the entry.
        let _entry = PendingEntry {
            pending: Arc::clone(&shared.pending),
            id,
        };
        let exchange = async {
            shared.enqueue(line).await?;
            answer.await.unwrap_or(Err(RpcError::Closed))
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .unwrap_or(Err(RpcError::Timeout))
    }

    /// Send a notification (no id, no response).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.inner.send(call_frame(None, method, params)).await
    }

    /// Answer a server request with a result.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), RpcError> {
        let mut msg = Map::new();
        msg.insert("id".to_owned(), id);
        msg.insert("result".to_owned(), result);
        self.inner.send_bounded(msg).await
    }

    /// Answer a server request with a JSON-RPC error.
    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), RpcError> {
        let mut msg = Map::new();
        msg.insert("id".to_owned(), id);
        msg.insert(
            "error".to_owned(),
            json!({ "code": code, "message": message }),
        );
        self.inner.send_bounded(msg).await
    }

    /// Number of requests still waiting for a response.
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        locked(&self.inner.pending).len()
    }
}

fn spawn_with_cap<R, W>(
    reader: R,
    writer: W,
    jsonrpc_field: bool,
    cap: usize,
    write_timeout: Duration,
) -> (RpcConn, mpsc::Receiver<Incoming>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outgoing_tx, outgoing_rx) = mpsc::channel(OUTGOING_CAP);
    let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAP);
    let shared = Arc::new(Shared {
        next_id: AtomicI64::new(1),
        jsonrpc_field,
        closed: AtomicBool::new(false),
        pending: Arc::new(Mutex::new(HashMap::new())),
        outgoing: Mutex::new(Some(outgoing_tx)),
        write_timeout,
    });
    tokio::spawn(write_loop(writer, outgoing_rx, Arc::clone(&shared)));
    tokio::spawn(read_loop(reader, Arc::clone(&shared), incoming_tx, cap));
    (RpcConn { inner: shared }, incoming_rx)
}

impl Shared {
    /// Serialize one message, adding `"jsonrpc":"2.0"` when the connection asks for it.
    fn frame(&self, mut msg: Map<String, Value>) -> Result<String, RpcError> {
        if self.jsonrpc_field {
            msg.insert("jsonrpc".to_owned(), Value::from("2.0"));
        }
        serde_json::to_string(&Value::Object(msg)).map_err(|e| RpcError::Io(e.to_string()))
    }

    /// Serialize a message and queue it for the writer.
    async fn send(&self, msg: Map<String, Value>) -> Result<(), RpcError> {
        let line = self.frame(msg)?;
        self.enqueue(line).await
    }

    /// `send`, bounded by the write timeout.
    async fn send_bounded(&self, msg: Map<String, Value>) -> Result<(), RpcError> {
        tokio::time::timeout(self.write_timeout, self.send(msg))
            .await
            .unwrap_or(Err(RpcError::Timeout))
    }

    async fn enqueue(&self, line: String) -> Result<(), RpcError> {
        let tx = locked(&self.outgoing)
            .as_ref()
            .cloned()
            .ok_or(RpcError::Closed)?;
        tx.send(line).await.map_err(|_| RpcError::Closed)
    }

    /// Mark the connection closed, fail every pending request with `Closed`, and stop the
    /// writer once it has drained. Idempotent.
    fn close(&self) {
        let waiters = {
            let mut pending = locked(&self.pending);
            self.closed.store(true, Ordering::SeqCst);
            std::mem::take(&mut *pending)
        };
        *locked(&self.outgoing) = None;
        for (_, waiter) in waiters {
            // The request may already have timed out and dropped its receiver; that is fine.
            let _ = waiter.send(Err(RpcError::Closed));
        }
    }
}

/// Lock a std mutex, ignoring poisoning: none of the guarded maps can be left half-updated by
/// a panic, because no code panics while holding one.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A request (`id` present) or a notification (`id` absent). `null` params are omitted.
fn call_frame(id: Option<Value>, method: &str, params: Value) -> Map<String, Value> {
    let mut msg = Map::new();
    if let Some(id) = id {
        msg.insert("id".to_owned(), id);
    }
    msg.insert("method".to_owned(), Value::from(method));
    if !params.is_null() {
        msg.insert("params".to_owned(), params);
    }
    msg
}

async fn write_loop<W>(mut writer: W, mut lines: mpsc::Receiver<String>, shared: Arc<Shared>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if let Err(e) = write_line(&mut writer, line.as_bytes()).await {
            tracing::warn!(error = %e, "jsonrpc write failed; closing connection");
            shared.close();
            return;
        }
    }
}

async fn write_line<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(bytes).await?;
    writer.flush().await
}

async fn read_loop<R>(reader: R, shared: Arc<Shared>, incoming: mpsc::Sender<Incoming>, cap: usize)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        match read_capped_line(&mut reader, &mut line, cap).await {
            Ok(LineRead::Line) => dispatch(&shared, &incoming, &line).await,
            Ok(LineRead::TooLong) => {
                tracing::warn!(cap, "jsonrpc line over the cap skipped");
            }
            Ok(LineRead::Eof) => break,
            Err(e) => {
                tracing::warn!(error = %e, "jsonrpc read failed");
                break;
            }
        }
    }
    shared.close();
    // `incoming` drops here, which closes the channel for the caller.
}

/// Route one complete line to a waiter, or to the caller's channel.
async fn dispatch(shared: &Shared, incoming: &mpsc::Sender<Incoming>, line: &[u8]) {
    // Blank and whitespace-only lines (keep-alive padding, CRLF noise) carry no message.
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let msg = match serde_json::from_slice::<Value>(line) {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(error = %e, "jsonrpc line is not JSON; skipped");
            return;
        }
    };
    match classify(msg) {
        Some(Message::Response(id, outcome)) => {
            let waiter = locked(&shared.pending).remove(&id);
            match waiter {
                // The receiver may be gone after a timeout; that is fine.
                Some(waiter) => {
                    let _ = waiter.send(outcome);
                }
                None => tracing::debug!(id, "jsonrpc response for an unknown id ignored"),
            }
        }
        Some(Message::Incoming(item @ Incoming::Request { .. })) => {
            // Requests are never dropped: wait for room.
            let _ = incoming.send(item).await;
        }
        Some(Message::Incoming(item)) => {
            if let Err(mpsc::error::TrySendError::Full(Incoming::Notification { method, .. })) =
                incoming.try_send(item)
            {
                tracing::warn!(%method, "jsonrpc incoming channel full; notification dropped");
            }
        }
        None => tracing::debug!("jsonrpc message with no request, response or notification shape"),
    }
}

enum Message {
    Response(i64, Result<Value, RpcError>),
    Incoming(Incoming),
}

/// Sort one parsed message by its shape. Anything that fits no shape is `None`.
fn classify(msg: Value) -> Option<Message> {
    let Value::Object(mut obj) = msg else {
        return None;
    };
    let id = obj.remove("id").filter(|id| !id.is_null());
    let method = match obj.remove("method") {
        None => None,
        Some(Value::String(method)) => Some(method),
        Some(_) => return None,
    };
    match (id, method) {
        (Some(id), None) => {
            let outcome = if let Some(error) = obj.remove("error").filter(|e| !e.is_null()) {
                Err(remote_error(error))
            } else {
                Ok(obj.remove("result")?)
            };
            Some(Message::Response(id.as_i64()?, outcome))
        }
        (Some(id), Some(method)) => Some(Message::Incoming(Incoming::Request {
            id,
            method,
            params: obj.remove("params").unwrap_or(Value::Null),
        })),
        (None, Some(method)) => Some(Message::Incoming(Incoming::Notification {
            method,
            params: obj.remove("params").unwrap_or(Value::Null),
        })),
        (None, None) => None,
    }
}

fn remote_error(error: Value) -> RpcError {
    match error {
        Value::Object(mut obj) => RpcError::Remote {
            code: obj
                .remove("code")
                .and_then(|code| code.as_i64())
                .unwrap_or(INTERNAL_ERROR),
            message: match obj.remove("message") {
                Some(Value::String(message)) => message,
                Some(other) => other.to_string(),
                None => String::new(),
            },
        },
        other => RpcError::Remote {
            code: INTERNAL_ERROR,
            message: other.to_string(),
        },
    }
}

enum LineRead {
    /// A complete line (or the final unterminated line before EOF) is in the buffer.
    Line,
    /// A line longer than the cap; it was consumed up to its newline and discarded.
    TooLong,
    Eof,
}

/// Read one `\n`-terminated line into `buf` (newline excluded), holding at most `cap` bytes.
/// A longer line is consumed through its newline without being stored, so the next line is
/// read normally and memory stays bounded.
async fn read_capped_line<R>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<LineRead>
where
    R: AsyncRead + Unpin,
{
    buf.clear();
    let mut too_long = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if too_long {
                LineRead::TooLong
            } else if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let content_len = newline.unwrap_or(chunk.len());
        if !too_long {
            if buf.len() + content_len > cap {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..content_len]);
            }
        }
        let consumed = newline.map_or(content_len, |at| at + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

#[cfg(test)]
mod tests;
