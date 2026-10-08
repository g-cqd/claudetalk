//! The stateless HTTP transport (axum 0.8): `POST /mcp` for both protocol eras.
//!
//! Never mints `Mcp-Session-Id`, ignores one if sent. Check order on every request:
//! (1) Origin: every Origin header must be loopback (`http(s)://localhost|127.0.0.1|[::1]`, any
//! port) or equal (case-insensitively) to one allowed origin, else 403. (2) Host (DNS-rebinding
//! guard), only when bound to a loopback address: exactly one Host header, whose hostname is
//! localhost / 127.0.0.1 / [::1], else 403. (2b) CORS preflight: `OPTIONS` from an allowed
//! Origin -> 204 with the Access-Control-* headers, before any credential is read; `OPTIONS`
//! without an Origin takes the same path as any other non-POST. (3) `Authorization: Bearer
//! <token>` matching a token entry (the file is re-read when its stamp changes), else 401 with
//! `WWW-Authenticate: Bearer realm="agora"` and JSON `{"error":"unauthorized"}`. (4) method /
//! path / size / content-type / accept: GET or DELETE /mcp -> 405 (`Allow: POST`); other paths
//! -> 404; body over 8 MiB -> 413; Content-Type not application/json -> 415; an Accept header
//! containing none of application/json, text/event-stream, */* -> 406. (5) JSON-RPC via
//! `Engine::handle`.
//!
//! Every response to a request that carried an allowed Origin also gets
//! `Access-Control-Allow-Origin` (the request's own origin) and `Vary: Origin`.
//!
//! Revocation latency: the token file is checked on every request, so a revoked token's next
//! request is refused at once. Its calls already in flight are cancelled by the next request the
//! server receives, from any token; with no further traffic they end on their own, within the
//! call's own bound (at most about 55 s for an `inbox_wait`).
//!
//! Status mapping (`status_for`): OK result -> 200 `application/json`; notification or client
//! response -> 202 empty; a request the engine cancelled (the peer ended) -> 503
//! `{"error":"cancelled"}`; a panic inside a call -> 500 with a -32603 body; a request still
//! running when the hard stop fires -> 503 `{"error":"shutting down"}` (`Connection: close`);
//! -32700/-32600/-32602/-32022/-32020 -> 400 with the JSON-RPC error
//! body; -32601 -> 404 with the body; tool errors are 200 (isError). A tools/call with a
//! progressToken whose Accept includes text/event-stream is answered as `text/event-stream`
//! (`X-Accel-Buffering: no`) once its first message is a progress notification: `event: message`
//! / `data: <json>` per notification, then the final response, then the stream ends. A call
//! whose first message is already the final response is answered as plain JSON instead. A
//! client disconnect cancels the request. TLS is out of scope: put a TLS proxy in front for
//! remote use.

use crate::McpError;
use crate::engine::{Engine, HttpMeta, Notifier, Peer, Transport};
use crate::jsonrpc::{self, Incoming, MAX_MESSAGE_BYTES, RpcError};
use crate::tokens::{self, TokenEntry, TokenFile};
use crate::version;
use agora_client::Ctx;
use agora_daemon::Paths;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Where the daemon socket lives (the front-end talks to the daemon like any client).
    pub paths: Paths,
    pub token_file: PathBuf,
    /// Extra allowed `Origin` values (exact match), besides loopback origins.
    pub allowed_origins: Vec<String>,
}

/// Hostnames that count as loopback, for both `Origin` and `Host`.
const LOOPBACK_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
/// Media types an Accept header may name (`*/*` included) for a request to be answered.
const ACCEPTABLE: [&str; 3] = ["application/json", "text/event-stream", "*/*"];
/// Capacity of the queue between a streamed tools/call and its SSE body.
const SSE_QUEUE: usize = 64;
/// How long a stopping server may drain in-flight requests before they are hard-stopped.
const DRAIN_LIMIT: Duration = Duration::from_secs(3);
/// Headers a CORS preflight answer names.
const CORS_ALLOW_HEADERS: &str =
    "authorization, content-type, accept, mcp-protocol-version, mcp-method";
/// Seconds a browser may cache a preflight answer.
const CORS_MAX_AGE: &str = "600";

/// Bind `addr`. A non-loopback address requires `allow_remote`, else `McpError::Config`.
pub async fn bind(addr: SocketAddr, allow_remote: bool) -> Result<TcpListener, McpError> {
    if !addr.ip().is_loopback() && !allow_remote {
        return Err(McpError::Config(format!(
            "refusing to listen on non-loopback {addr} without --allow-remote"
        )));
    }
    Ok(TcpListener::bind(addr).await?)
}

/// Serve until `shutdown` fires (axum `with_graceful_shutdown`), then give the drain at most
/// `DRAIN_LIMIT`. When the limit passes, or once `serve` has returned, `hard_stop` fires: every
/// request still running answers 503 `{"error":"shutting down"}` and every SSE stream ends.
/// Caches one `Arc<Peer>` per token id (HTTP peers: `Peer::new(ctx, Transport::Http, Some(token
/// id))`, ctx identity from the token's seed, label `http:<label>`); a peer first seen after
/// `shutdown` fired is cancelled at once. `McpError::Config` when an allowed origin is `null`,
/// `*` or empty.
pub async fn serve(
    listener: TcpListener,
    mut opts: HttpOptions,
    engine: Arc<Engine>,
    shutdown: CancellationToken,
) -> Result<(), McpError> {
    opts.allowed_origins = normalise_origins(&opts.allowed_origins)?;
    let local = listener.local_addr()?;
    let state = Arc::new(HttpState {
        engine,
        opts,
        loopback: local.ip().is_loopback(),
        cache: Mutex::new(TokenCache::default()),
        stopping: shutdown.clone(),
        hard_stop: CancellationToken::new(),
    });
    let app = Router::new()
        .route("/mcp", any(mcp))
        .fallback(not_found)
        .with_state(Arc::clone(&state));
    info!(addr = %local, "MCP HTTP front-end listening");

    let stopping = Arc::clone(&state);
    let stop_signal = shutdown.clone();
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop_signal.cancelled().await;
            // Cancel the peers first, so in-flight calls end and the graceful stop can finish.
            stopping.cancel_peers();
        })
        .into_future();
    let drain_limit = async {
        shutdown.cancelled().await;
        tokio::time::sleep(DRAIN_LIMIT).await;
    };
    let served = tokio::select! {
        result = serving => result,
        () = drain_limit => {
            info!("MCP HTTP drain limit reached; hard-stopping the requests still running");
            Ok(())
        }
    };
    // Connections spawned by axum may outlive `serve`: their requests and streams end here.
    state.hard_stop.cancel();
    state.cancel_peers();
    served?;
    info!("MCP HTTP front-end stopped");
    Ok(())
}

/// The configured origins in the form `origin_allowed` compares: lowercase, with one trailing
/// `/` removed. `null` and `*` would admit any page or any origin, and an empty value would
/// match an empty Origin header, so all three are refused.
fn normalise_origins(origins: &[String]) -> Result<Vec<String>, McpError> {
    origins
        .iter()
        .map(|raw| {
            let lower = raw.to_ascii_lowercase();
            let origin = lower.strip_suffix('/').unwrap_or(lower.as_str());
            if origin.is_empty() || origin == "null" || origin == "*" {
                return Err(McpError::Config(format!(
                    "allowed origin {raw:?} is not an origin (\"null\" and \"*\" are refused)"
                )));
            }
            Ok(origin.to_owned())
        })
        .collect()
}

/// `origin` is a loopback origin or equals one of `allowed` (case-insensitively; `allowed` is
/// normalised by `serve`).
pub fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    if origin.is_empty() {
        return false;
    }
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(origin)) {
        return true;
    }
    // `split_authority` accepts only `host[:digits]`, so a path, query, fragment or userinfo
    // in the origin makes the host not loopback.
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let web_scheme = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    web_scheme && split_authority(authority).is_some_and(|(host, _)| is_loopback_name(host))
}

/// The Host header's hostname (port stripped) is localhost, 127.0.0.1 or [::1].
pub fn host_allowed(host: &str) -> bool {
    split_authority(host).is_some_and(|(name, _)| is_loopback_name(name))
}

/// HTTP status for a JSON-RPC response object (table in the module doc).
pub fn status_for(response: &Value) -> u16 {
    match jsonrpc::error_code(response) {
        None => 200,
        Some(jsonrpc::METHOD_NOT_FOUND) => 404,
        Some(jsonrpc::INTERNAL_ERROR) => 500,
        Some(_) => 400,
    }
}

/// Splits `host[:port]` (or `[v6][:port]`) into the host, which keeps its brackets, and the
/// optional port, which must be digits only. Anything else is `None`.
fn split_authority(s: &str) -> Option<(&str, Option<&str>)> {
    let (host, rest) = if s.starts_with('[') {
        let close = s.find(']')?;
        (s.get(..=close)?, s.get(close + 1..)?)
    } else {
        match s.find(':') {
            Some(colon) => (s.get(..colon)?, s.get(colon..)?),
            None => (s, ""),
        }
    };
    let port = match rest {
        "" => None,
        _ => Some(rest.strip_prefix(':')?),
    };
    match port {
        Some(p) if !is_port(p) => None,
        _ => Some((host, port)),
    }
}

fn is_port(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
}

fn is_loopback_name(host: &str) -> bool {
    LOOPBACK_NAMES
        .iter()
        .any(|name| host.eq_ignore_ascii_case(name))
}

/// What identifies one version of the token file on disk. Writes replace the file by rename, so
/// the inode changes; the mtime and length cover an in-place write, down to the nanosecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_nsec: i64,
    len: u64,
}

impl FileStamp {
    /// The stamp of `path`; `None` when it cannot be stat'ed.
    fn of(path: &Path) -> Option<FileStamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(FileStamp {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            len: meta.len(),
        })
    }
}

/// The token cache: the token file as last read, plus one peer per token id.
#[derive(Default)]
struct TokenCache {
    loaded: bool,
    /// The file's stamp when it was last read; `None` when it could not be stat'ed.
    stamp: Option<FileStamp>,
    file: TokenFile,
    peers: HashMap<String, Arc<Peer>>,
}

impl TokenCache {
    /// Re-read the token file when its stamp changed (or when it cannot be stat'ed). Peers of
    /// tokens that no longer exist are removed and cancelled.
    fn refresh(&mut self, path: &Path) -> Result<(), McpError> {
        let stamp = FileStamp::of(path);
        if self.loaded && stamp.is_some() && stamp == self.stamp {
            return Ok(());
        }
        self.file = tokens::load(path)?;
        self.loaded = true;
        self.stamp = stamp;
        let live: HashSet<&str> = self.file.tokens.iter().map(|t| t.id.as_str()).collect();
        self.peers.retain(|id, peer| {
            let keep = live.contains(id.as_str());
            if !keep {
                debug!(token_id = %id, "token removed; ending its peer");
                peer.shutdown.cancel();
            }
            keep
        });
        Ok(())
    }

    /// The cached peer of `entry`, created on first use with the token's identity.
    fn peer_for(&mut self, entry: &TokenEntry, paths: &Paths) -> Result<Arc<Peer>, McpError> {
        if let Some(peer) = self.peers.get(&entry.id) {
            return Ok(Arc::clone(peer));
        }
        let identity = tokens::identity_of(entry)?;
        let ctx = Ctx {
            paths: paths.clone(),
            identity,
            label: format!("http:{}", entry.label),
        };
        let peer = Arc::new(Peer::new(
            Arc::new(ctx),
            Transport::Http,
            Some(entry.id.clone()),
        ));
        self.peers.insert(entry.id.clone(), Arc::clone(&peer));
        Ok(peer)
    }
}

struct HttpState {
    engine: Arc<Engine>,
    opts: HttpOptions,
    /// The listener is bound to a loopback address (the Host check applies).
    loopback: bool,
    cache: Mutex<TokenCache>,
    /// Fires when shutdown is requested: the graceful drain starts.
    stopping: CancellationToken,
    /// Fires when the drain limit passes or `serve` returns: running requests end with 503.
    hard_stop: CancellationToken,
}

impl HttpState {
    fn cancel_peers(&self) {
        for peer in lock(&self.cache).peers.values() {
            peer.shutdown.cancel();
        }
    }
}

/// Aborts its task when dropped: dropping the response stream cancels the streamed call.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Ends the wrapped stream as soon as `stop` completes (the hard stop of a stopping server).
struct UntilStopped<S> {
    inner: S,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl<S: Stream + Unpin> Stream for UntilStopped<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        if self.stop.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn mcp(
    State(state): State<Arc<HttpState>>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let cors = cors_origin(&state, &headers);
    let mut response = tokio::select! {
        biased;
        () = state.hard_stop.cancelled() => shutting_down_reply(),
        response = route(&state, &method, &headers, body, cors.is_some()) => response,
    };
    if let Some(origin) = cors {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

/// Checks (1) to (5) in the module's order. `allowed_origin`: the request carries exactly one
/// Origin and it is allowed (the caller adds the CORS headers to whatever this returns).
async fn route(
    state: &Arc<HttpState>,
    method: &Method,
    headers: &HeaderMap,
    body: Body,
    allowed_origin: bool,
) -> Response {
    // (1) Origin.
    if !origin_ok(state, headers) {
        return error_reply(StatusCode::FORBIDDEN, "forbidden origin");
    }
    // (2) Host, on a loopback listener only.
    if state.loopback && !host_ok(headers) {
        return error_reply(StatusCode::FORBIDDEN, "forbidden host");
    }
    // (2b) CORS preflight from an allowed origin: the browser sends no credentials.
    if *method == Method::OPTIONS && allowed_origin {
        return preflight();
    }
    // (3) Bearer token.
    let peer = match authenticate(state, headers) {
        Ok(peer) => peer,
        Err(rejected) => return rejected.response(),
    };
    debug!(
        token_id = peer.token_id.as_deref().unwrap_or("-"),
        http_method = %method,
        "mcp http request"
    );
    // (4) Method, content type, accept, size.
    if *method != Method::POST {
        return method_not_allowed();
    }
    if !is_json_content_type(headers) {
        return error_reply(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported media type");
    }
    let accepted = accepted_types(headers);
    if headers.contains_key(header::ACCEPT)
        && !accepted.iter().any(|t| ACCEPTABLE.contains(&t.as_str()))
    {
        return error_reply(StatusCode::NOT_ACCEPTABLE, "not acceptable");
    }
    let accepts_sse = accepted.iter().any(|t| t.as_str() == "text/event-stream");
    let bytes = match read_body(headers, body).await {
        Ok(bytes) => bytes,
        Err(BodyFault::TooLarge) => {
            return error_reply(StatusCode::PAYLOAD_TOO_LARGE, "message too large");
        }
        Err(BodyFault::Unreadable) => {
            return error_reply(StatusCode::BAD_REQUEST, "unreadable body");
        }
    };
    // (5) JSON-RPC.
    let meta = HttpMeta {
        protocol_version: header_text(headers, "mcp-protocol-version"),
        method: header_text(headers, "mcp-method"),
    };
    answer(
        Arc::clone(&state.engine),
        peer,
        jsonrpc::parse(&bytes),
        meta,
        accepts_sse,
        state.hard_stop.clone(),
    )
    .await
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// Answers one parsed message: an error reply for malformed input, the streamed call for an
/// SSE request, otherwise the engine's reply. No reply is 202 for a notification or a client
/// response, and 503 for a request the engine cancelled.
async fn answer(
    engine: Arc<Engine>,
    peer: Arc<Peer>,
    msg: Incoming,
    meta: HttpMeta,
    accepts_sse: bool,
    hard_stop: CancellationToken,
) -> Response {
    if let Incoming::Invalid { error, id } = &msg {
        let body = jsonrpc::error_response(id, error);
        return json_reply(status_code(status_for(&body)), &body);
    }
    if wants_progress_stream(&msg, accepts_sse) {
        return progress_stream(engine, peer, msg, meta, hard_stop).await;
    }
    let is_request = matches!(msg, Incoming::Request { .. });
    match guarded_call(engine, peer, msg, Notifier::none(), meta).await {
        Some(body) => json_reply(status_code(status_for(&body)), &body),
        None if is_request => cancelled_reply(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// One engine call on a task of its own, aborted when the caller goes away. A panic inside the
/// call is answered as -32603 for the request's id; `None` when the call was cancelled or the
/// message needs no reply.
async fn guarded_call(
    engine: Arc<Engine>,
    peer: Arc<Peer>,
    msg: Incoming,
    notifier: Notifier,
    meta: HttpMeta,
) -> Option<Value> {
    let id = match &msg {
        Incoming::Request { id, .. } => id.clone(),
        _ => Value::Null,
    };
    let mut call = AbortOnDrop(tokio::spawn(async move {
        engine.handle(&peer, msg, &notifier, Some(&meta)).await
    }));
    match (&mut call.0).await {
        Ok(reply) => reply,
        Err(e) if e.is_panic() => Some(jsonrpc::error_response(&id, &RpcError::internal())),
        Err(_) => None,
    }
}

fn wants_progress_stream(msg: &Incoming, accepts_sse: bool) -> bool {
    match msg {
        Incoming::Request { method, params, .. } => {
            accepts_sse
                && method.as_str() == "tools/call"
                && version::progress_token(params).is_some()
        }
        _ => false,
    }
}

/// Runs the call on its own task. Its first message decides the answer: a final response is
/// answered as plain JSON; anything else (a progress notification) starts an SSE stream that
/// carries that message, the rest, and the final response last. A task that ends without any
/// message was cancelled (503), and a panic is a -32603 final response. Dropping the stream
/// (client disconnect) aborts the task; the hard stop ends the stream.
async fn progress_stream(
    engine: Arc<Engine>,
    peer: Arc<Peer>,
    msg: Incoming,
    meta: HttpMeta,
    hard_stop: CancellationToken,
) -> Response {
    let (tx, mut rx) = mpsc::channel::<Value>(SSE_QUEUE);
    let notifier = Notifier::new(tx.clone());
    let task = tokio::spawn(async move {
        if let Some(response) = guarded_call(engine, peer, msg, notifier, meta).await {
            // Fails only when the client has gone, and then nobody reads the response.
            let _ = tx.send(response).await;
        }
    });
    let abort = AbortOnDrop(task);
    let Some(first) = rx.recv().await else {
        return cancelled_reply();
    };
    if is_final_response(&first) {
        return json_reply(status_code(status_for(&first)), &first);
    }
    let messages = UntilStopped {
        inner: tokio_stream::once(first).chain(ReceiverStream::new(rx)),
        stop: Box::pin(async move { hard_stop.cancelled().await }),
    };
    let events = messages.map(move |message| {
        let _keep_task_alive = &abort;
        Ok::<_, Infallible>(Event::default().event("message").data(message.to_string()))
    });
    let mut response = Sse::new(events).into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// A JSON-RPC response object (an `id` and no `method`), as opposed to a notification.
fn is_final_response(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").is_none()
}

/// Check (1): every Origin header must be acceptable. No Origin header is fine.
fn origin_ok(state: &HttpState, headers: &HeaderMap) -> bool {
    headers.get_all(header::ORIGIN).iter().all(|v| {
        v.to_str()
            .is_ok_and(|origin| origin_allowed(origin, &state.opts.allowed_origins))
    })
}

/// The Origin to echo in `Access-Control-Allow-Origin`: the request's one Origin header, when
/// it is present, single, and allowed.
fn cors_origin(state: &HttpState, headers: &HeaderMap) -> Option<HeaderValue> {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin = origins.next()?;
    if origins.next().is_some() {
        return None;
    }
    let text = origin.to_str().ok()?;
    origin_allowed(text, &state.opts.allowed_origins).then(|| origin.clone())
}

/// The answer to a CORS preflight: no body; `cors_origin` headers are added by the caller.
fn preflight() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(CORS_ALLOW_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(CORS_MAX_AGE),
    );
    response
}

/// Check (2): exactly one Host header, and it names a loopback host.
fn host_ok(headers: &HeaderMap) -> bool {
    let mut hosts = headers.get_all(header::HOST).iter();
    match (hosts.next(), hosts.next()) {
        (Some(host), None) => host.to_str().is_ok_and(host_allowed),
        _ => false,
    }
}

/// Why a request body could not be read in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFault {
    /// Longer than `MAX_MESSAGE_BYTES` (413).
    TooLarge,
    /// The connection failed while the body was being read (400).
    Unreadable,
}

/// The whole body, at most `MAX_MESSAGE_BYTES`. A declared Content-Length over the cap is
/// refused before any byte is read; otherwise the cap is enforced while reading.
async fn read_body(headers: &HeaderMap, body: Body) -> Result<Vec<u8>, BodyFault> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_MESSAGE_BYTES) {
        return Err(BodyFault::TooLarge);
    }
    let mut chunks = std::pin::pin!(body.into_data_stream());
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| BodyFault::Unreadable)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_MESSAGE_BYTES {
            return Err(BodyFault::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A request the engine cancelled (its peer ended before it answered).
fn cancelled_reply() -> Response {
    error_reply(StatusCode::SERVICE_UNAVAILABLE, "cancelled")
}

/// A request that met the hard stop: 503, and the connection is closed after it.
fn shutting_down_reply() -> Response {
    let mut response = error_reply(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// Why check (3) refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejected {
    /// No Bearer token, or one that matches no entry.
    Unauthorized,
    /// The token file or a peer could not be prepared (details go to the log, never a token).
    Internal,
}

impl Rejected {
    fn response(self) -> Response {
        match self {
            Rejected::Unauthorized => unauthorized(),
            Rejected::Internal => error_reply(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        }
    }
}

/// Check (3): the bearer token must match an entry of the current token file.
fn authenticate(state: &HttpState, headers: &HeaderMap) -> Result<Arc<Peer>, Rejected> {
    let Some(bearer) = bearer_token(headers) else {
        return Err(Rejected::Unauthorized);
    };
    let mut guard = lock(&state.cache);
    let cache = &mut *guard;
    if let Err(e) = cache.refresh(&state.opts.token_file) {
        warn!("cannot read the MCP token file: {e}");
        return Err(Rejected::Internal);
    }
    let Some(entry) = tokens::verify(&cache.file, bearer) else {
        return Err(Rejected::Unauthorized);
    };
    if let Some(peer) = cache.peers.get(&entry.id) {
        return Ok(Arc::clone(peer));
    }
    // Cache miss: only now is the entry copied and its identity derived.
    let entry = entry.clone();
    let peer = cache.peer_for(&entry, &state.opts.paths).map_err(|e| {
        warn!(token_id = %entry.id, "cannot prepare the peer for a token: {e}");
        Rejected::Internal
    })?;
    if state.stopping.is_cancelled() {
        // Created after shutdown began: it gets no time to serve anything.
        peer.shutdown.cancel();
    }
    Ok(peer)
}

/// The token of a single `Authorization: Bearer <token>` header (scheme case-insensitive).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// The media type of the Content-Type header is `application/json` (parameters allowed).
fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// The media types named by every Accept header: lowercased, parameters dropped.
fn accepted_types(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|item| item.split(';').next())
        .map(|media| media.trim().to_ascii_lowercase())
        .collect()
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn status_code(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn json_reply(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn error_reply(status: StatusCode, message: &str) -> Response {
    json_reply(status, &json!({ "error": message }))
}

fn unauthorized() -> Response {
    let mut response = error_reply(StatusCode::UNAUTHORIZED, "unauthorized");
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"agora\""),
    );
    response
}

fn method_not_allowed() -> Response {
    let mut response = error_reply(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::jsonrpc::{self as rpc, RpcError};
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(10);
    const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    const UNKNOWN_METHOD: &str = r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#;
    const BATCH: &str = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#;
    const UNKNOWN_TOOL: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_such_tool","arguments":{},"_meta":{"progressToken":"p1"}}}"#;

    #[test]
    fn loopback_origins_are_allowed() {
        for origin in [
            "http://localhost:3000",
            "https://127.0.0.1",
            "http://[::1]:1",
            "HTTP://LOCALHOST",
        ] {
            assert!(origin_allowed(origin, &[]), "{origin} should be allowed");
        }
    }

    #[test]
    fn other_origins_are_refused() {
        for origin in [
            "http://evil.com",
            "http://localhost.evil.com",
            "http://localhost/path",
            "http://localhost:80/path",
            "http://user@localhost",
            "http://localhost:",
            "ftp://localhost",
            "null",
            "",
        ] {
            assert!(!origin_allowed(origin, &[]), "{origin} should be refused");
        }
    }

    #[test]
    fn allowed_origins_match_exactly() {
        let allowed = vec!["https://app.example.com".to_owned()];
        assert!(origin_allowed("https://app.example.com", &allowed));
        assert!(!origin_allowed("https://app.example.com/", &allowed));
        assert!(!origin_allowed("https://other.example.com", &allowed));
    }

    #[test]
    fn loopback_hosts_are_allowed() {
        for host in [
            "localhost:8080",
            "localhost",
            "127.0.0.1:1",
            "[::1]:9",
            "LOCALHOST",
        ] {
            assert!(host_allowed(host), "{host} should be allowed");
        }
    }

    #[test]
    fn other_hosts_are_refused() {
        for host in [
            "evil.com",
            "evil.com:80",
            "localhost.evil.com",
            "localhost:",
            "localhost:8x",
            "[::1",
            "localhost@evil.com",
            "",
        ] {
            assert!(!host_allowed(host), "{host} should be refused");
        }
    }

    #[test]
    fn status_follows_the_error_code() {
        let id = json!(1);
        assert_eq!(status_for(&rpc::response(&id, json!({}))), 200);
        for code in [
            jsonrpc::PARSE_ERROR,
            jsonrpc::INVALID_REQUEST,
            jsonrpc::INVALID_PARAMS,
            jsonrpc::UNSUPPORTED_VERSION,
            jsonrpc::HEADER_MISMATCH,
        ] {
            assert_eq!(
                status_for(&rpc::error_response(&id, &RpcError::new(code, "x"))),
                400
            );
        }
        let not_found = rpc::error_response(&id, &RpcError::method_not_found("x"));
        assert_eq!(status_for(&not_found), 404);
        let internal = rpc::error_response(&id, &RpcError::internal());
        assert_eq!(status_for(&internal), 500);
    }

    #[test]
    fn host_header_must_be_exactly_one_loopback_name() {
        let with = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(header::HOST, HeaderValue::from_str(value).expect("a value"));
            }
            headers
        };
        assert!(!host_ok(&with(&[])), "a missing Host is refused");
        assert!(host_ok(&with(&["localhost:8765"])));
        assert!(!host_ok(&with(&["evil.example"])));
        assert!(!host_ok(&with(&["localhost", "localhost"])), "two Hosts");
        assert!(!host_ok(&with(&["localhost", "evil.example"])), "two Hosts");
    }

    #[test]
    fn configured_origins_are_normalised_and_wildcards_refused() {
        let normalised = normalise_origins(&["HTTPS://App.Example/".to_owned()]).expect("ok");
        assert_eq!(normalised, vec!["https://app.example".to_owned()]);
        assert!(origin_allowed("https://APP.example", &normalised));
        for bad in ["null", "NULL", "*", "", "/"] {
            let err = normalise_origins(&[bad.to_owned()]).expect_err("refused");
            assert!(matches!(err, McpError::Config(_)), "{bad:?}: {err:?}");
        }
    }

    #[test]
    fn a_same_size_rewrite_with_the_same_mtime_is_reloaded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp-tokens.json");
        let (first, _) = tokens::add(&path, "first", 1).expect("add");
        let mut cache = TokenCache::default();
        cache.refresh(&path).expect("first load");
        assert!(tokens::verify(&cache.file, &first).is_some());

        // A replacement of the same length, renamed over the file with the old mtime restored:
        // only the inode tells the two versions apart.
        let old_mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("mtime");
        let swap = dir.path().join("swap.json");
        let (second, _) = tokens::add(&swap, "first", 1).expect("add the replacement");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&swap)
            .and_then(|file| file.set_modified(old_mtime))
            .expect("restore the mtime");
        std::fs::rename(&swap, &path).expect("replace the file");

        cache.refresh(&path).expect("second load");
        assert!(tokens::verify(&cache.file, &first).is_none());
        assert!(tokens::verify(&cache.file, &second).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bind_refuses_non_loopback_without_allow_remote() {
        let addr: SocketAddr = "0.0.0.0:0".parse().expect("address");
        let Err(McpError::Config(message)) = bind(addr, false).await else {
            panic!("expected a Config error for a non-loopback bind");
        };
        assert!(message.contains("--allow-remote"));
    }

    /// A real server on 127.0.0.1:0 with one token, and a client that talks to it.
    struct TestServer {
        client: reqwest::Client,
        url: String,
        token: String,
        cancel: CancellationToken,
        task: JoinHandle<Result<(), McpError>>,
        _home: tempfile::TempDir,
    }

    struct Reply {
        status: u16,
        headers: reqwest::header::HeaderMap,
        body: String,
    }

    impl Reply {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|v| v.to_str().ok())
        }

        fn json(&self) -> Value {
            serde_json::from_str(&self.body).expect("the body is JSON")
        }
    }

    impl TestServer {
        async fn start() -> Self {
            let home = tempfile::tempdir().expect("a temp dir");
            let token_file = home.path().join("mcp-tokens.json");
            let (token, _entry) = tokens::add(&token_file, "test", agora_daemon::now_ms())
                .expect("create a bearer token");
            let listener = bind("127.0.0.1:0".parse().expect("address"), false)
                .await
                .expect("bind 127.0.0.1:0");
            let addr = listener.local_addr().expect("local address");
            let opts = HttpOptions {
                paths: Paths::new(home.path()),
                token_file,
                allowed_origins: Vec::new(),
            };
            let cancel = CancellationToken::new();
            let task = tokio::spawn(serve(
                listener,
                opts,
                Engine::new(EngineConfig::http()),
                cancel.clone(),
            ));
            TestServer {
                client: reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("an HTTP client"),
                url: format!("http://{addr}/mcp"),
                token,
                cancel,
                task,
                _home: home,
            }
        }

        /// A legacy JSON-RPC POST with the JSON headers and no credentials.
        fn rpc(&self, body: &str) -> reqwest::RequestBuilder {
            self.client
                .post(&self.url)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .body(body.to_owned())
        }

        async fn exchange(&self, request: reqwest::RequestBuilder) -> Reply {
            let response = timeout(WAIT, request.send())
                .await
                .expect("the server answers in time")
                .expect("the request is sent");
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = timeout(WAIT, response.text())
                .await
                .expect("the body arrives in time")
                .expect("the body is readable");
            Reply {
                status,
                headers,
                body,
            }
        }

        /// Drop the client (its pooled connections would hold the graceful stop), stop the
        /// server and check that it ended cleanly.
        async fn stop(self) {
            let TestServer {
                client,
                cancel,
                task,
                ..
            } = self;
            drop(client);
            cancel.cancel();
            let served = timeout(WAIT, task)
                .await
                .expect("the server stops in time")
                .expect("the server task does not panic");
            served.expect("serve returns Ok");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn missing_token_is_unauthorized() {
        let server = TestServer::start().await;
        let reply = server.exchange(server.rpc(PING)).await;
        assert_eq!(reply.status, 401);
        assert_eq!(
            reply.header("www-authenticate"),
            Some("Bearer realm=\"agora\"")
        );
        assert_eq!(reply.json()["error"], "unauthorized");
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bad_origin_is_forbidden_before_authentication() {
        let server = TestServer::start().await;
        let request = server.rpc(PING).header(header::ORIGIN, "http://evil.com");
        let reply = server.exchange(request).await;
        assert_eq!(reply.status, 403);
        assert_eq!(reply.json()["error"], "forbidden origin");
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bad_host_is_forbidden() {
        let server = TestServer::start().await;
        let request = server
            .rpc(PING)
            .bearer_auth(&server.token)
            .header(header::HOST, "evil.com");
        let reply = server.exchange(request).await;
        assert_eq!(reply.status, 403);
        assert_eq!(reply.json()["error"], "forbidden host");
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_is_method_not_allowed() {
        let server = TestServer::start().await;
        let request = server.client.get(&server.url).bearer_auth(&server.token);
        let reply = server.exchange(request).await;
        assert_eq!(reply.status, 405);
        assert_eq!(reply.header("allow"), Some("POST"));
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wrong_content_type_is_unsupported() {
        let server = TestServer::start().await;
        let request = server
            .client
            .post(&server.url)
            .bearer_auth(&server.token)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(PING);
        let reply = server.exchange(request).await;
        assert_eq!(reply.status, 415);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn legacy_ping_is_answered_with_an_empty_result() {
        let server = TestServer::start().await;
        let reply = server
            .exchange(server.rpc(PING).bearer_auth(&server.token))
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.json()["id"], 1);
        assert_eq!(reply.json()["result"], json!({}));
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn notification_is_accepted_without_a_body() {
        let server = TestServer::start().await;
        let reply = server
            .exchange(server.rpc(INITIALIZED).bearer_auth(&server.token))
            .await;
        assert_eq!(reply.status, 202);
        assert!(reply.body.is_empty());
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unknown_method_is_not_found_with_method_not_found() {
        let server = TestServer::start().await;
        let reply = server
            .exchange(server.rpc(UNKNOWN_METHOD).bearer_auth(&server.token))
            .await;
        assert_eq!(reply.status, 404);
        assert_eq!(reply.json()["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn batch_array_is_a_bad_request() {
        let server = TestServer::start().await;
        let reply = server
            .exchange(server.rpc(BATCH).bearer_auth(&server.token))
            .await;
        assert_eq!(reply.status, 400);
        assert_eq!(reply.json()["error"]["code"], jsonrpc::INVALID_REQUEST);
        server.stop().await;
    }

    /// The first message of the call is already its final response, so no stream is opened and
    /// the response is plain JSON with the status of its error code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn progress_call_answered_at_once_is_plain_json() {
        let server = TestServer::start().await;
        let request = server
            .rpc(UNKNOWN_TOOL)
            .bearer_auth(&server.token)
            .header(header::ACCEPT, "text/event-stream");
        let reply = server.exchange(request).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert!(reply.header("x-accel-buffering").is_none());
        assert_eq!(reply.json()["id"], 3);
        assert_eq!(reply.json()["error"]["code"], jsonrpc::INVALID_PARAMS);
        server.stop().await;
    }
}
