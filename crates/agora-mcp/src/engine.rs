//! Method routing shared by both transports: initialize / server/discover / ping /
//! tools/list / tools/call, era selection, client tracking, push start.

use crate::clients;
use crate::delivery;
use crate::instructions::INSTRUCTIONS;
use crate::jsonrpc::{self, Incoming, RpcError};
use crate::push;
use crate::tools::{self, Progress, ToolCx, ToolError, ToolResult};
use crate::version::{self, ClientInfo, Era, RequestMeta};
use crate::{SERVER_NAME, SERVER_VERSION};
use agora_client::Ctx;
use agora_client::api::org::{self, InitParams};
use agora_client::ops::now_ms;
use agora_deliver::{Harness, Mechanism};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{OnceCell, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
}

/// Channel-push tuning (Claude Code rung 1).
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// Quiet period before a dirty inbox is fetched.
    pub coalesce_ms: u64,
    /// Token-bucket size.
    pub burst: u32,
    /// One token per this many ms.
    pub refill_ms: u64,
    /// Messages framed into one notification.
    pub max_messages: usize,
    /// Cap of the notification content.
    pub max_content_bytes: usize,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            coalesce_ms: 300,
            burst: 3,
            refill_ms: 2000,
            max_messages: 5,
            max_content_bytes: 8192,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub transport: Transport,
    /// Declare and send `notifications/claude/channel` (stdio + legacy + Claude Code only).
    pub channel_push: bool,
    /// Allow `path` / `save_to` tool arguments (never over HTTP).
    pub allow_paths: bool,
    pub push: PushConfig,
    /// Interval of `notifications/progress` for long tools.
    pub progress_interval_ms: u64,
}

impl EngineConfig {
    /// stdio: push true, paths true, progress every 10 s.
    pub fn stdio() -> Self {
        Self {
            transport: Transport::Stdio,
            channel_push: true,
            allow_paths: true,
            push: PushConfig::default(),
            progress_interval_ms: 10_000,
        }
    }

    /// HTTP: push false, paths false, progress every 10 s.
    pub fn http() -> Self {
        Self {
            transport: Transport::Http,
            channel_push: false,
            allow_paths: false,
            push: PushConfig::default(),
            progress_interval_ms: 10_000,
        }
    }
}

/// Sends server-initiated JSON-RPC notifications to the peer without blocking.
#[derive(Clone)]
pub struct Notifier {
    tx: Option<mpsc::Sender<Value>>,
}

impl Notifier {
    /// A notifier writing to the transport's outgoing queue.
    pub fn new(tx: mpsc::Sender<Value>) -> Self {
        Self { tx: Some(tx) }
    }

    /// A notifier that drops everything (HTTP without a stream, tests).
    pub fn none() -> Self {
        Self { tx: None }
    }

    /// `try_send` the full notification `{"jsonrpc":"2.0","method":method,"params":params}`.
    /// False when there is no sink, the queue is full or the peer is gone.
    pub fn send(&self, method: &str, params: Value) -> bool {
        match &self.tx {
            Some(tx) => tx.try_send(jsonrpc::notification(method, params)).is_ok(),
            None => false,
        }
    }
}

/// What `initialize` / the first requests established (legacy sessions; modern is stateless).
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub initialized: bool,
    pub version: Option<String>,
    pub client: Option<ClientInfo>,
    pub capabilities: Value,
}

/// One authenticated caller: the stdio process, or one HTTP token.
pub struct Peer {
    pub ctx: Arc<Ctx>,
    pub transport: Transport,
    /// HTTP: the token id; stdio: None.
    pub token_id: Option<String>,
    pub session: std::sync::Mutex<Session>,
    /// In-flight requests by `id.to_string()` (stdio): `notifications/cancelled` cancels them.
    pub inflight: std::sync::Mutex<HashMap<String, CancellationToken>>,
    pub delivery: std::sync::Mutex<delivery::Tracker>,
    /// Set by the first request that starts the push task (stdio).
    pub link_started: AtomicBool,
    /// Cancelled when the peer is gone: ends in-flight work and the push task.
    pub shutdown: CancellationToken,
    /// Registration happens once per peer, on the first tools/call.
    registered: OnceCell<()>,
}

impl Peer {
    /// A peer with an empty session, an empty inflight map, a tracker over
    /// `clients::ladder_for(Harness::Unknown, false)` and a fresh shutdown token.
    pub fn new(ctx: Arc<Ctx>, transport: Transport, token_id: Option<String>) -> Self {
        Self {
            ctx,
            transport,
            token_id,
            session: Mutex::new(Session::default()),
            inflight: Mutex::new(HashMap::new()),
            delivery: Mutex::new(delivery::Tracker::new(vec![Mechanism::WaitTool], false)),
            link_started: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            registered: OnceCell::new(),
        }
    }

    /// "local" for Stdio (the shim signs), "server-held" for Http (the front-end holds the seed).
    pub fn signing(&self) -> &'static str {
        match self.transport {
            Transport::Stdio => "local",
            Transport::Http => "server-held",
        }
    }

    /// Register this peer's key once (`agora_client::api::org::init` with owner=false,
    /// label=None; HTTP peers use label `http:<token label>` through their Ctx). A failure
    /// (for example RegistrationClosed) is returned as the tool error and retried on the
    /// next call; success is remembered.
    pub async fn ensure_registered(&self) -> Result<(), ToolError> {
        self.registered
            .get_or_try_init(|| async {
                org::init(
                    &self.ctx,
                    InitParams {
                        owner: false,
                        label: None,
                    },
                )
                .await
                .map(|_| ())
                .map_err(ToolError::from)
            })
            .await
            .map(|_| ())
    }

    /// Channel push is enabled: stdio transport, initialized legacy session, harness
    /// ClaudeCode (from the session's clientInfo), and `engine.config.channel_push`.
    pub fn push_enabled(&self, engine: &Engine) -> bool {
        if self.transport != Transport::Stdio || !engine.config.channel_push {
            return false;
        }
        let session = lock(&self.session);
        session.initialized
            && session.version.is_some()
            && session
                .client
                .as_ref()
                .is_some_and(|c| clients::harness_of(&c.name) == Harness::ClaudeCode)
    }
}

/// Transport facts the HTTP layer extracted from headers (for the modern header checks and
/// the legacy version).
#[derive(Debug, Clone, Default)]
pub struct HttpMeta {
    /// `MCP-Protocol-Version` header, if present.
    pub protocol_version: Option<String>,
    /// `Mcp-Method` header, if present.
    pub method: Option<String>,
}

/// The shared MCP state: configuration and the client registry.
pub struct Engine {
    pub config: EngineConfig,
    pub clients: clients::Registry,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Arc<Engine> {
        Arc::new(Engine {
            config,
            clients: clients::Registry::new(),
        })
    }

    /// Route one incoming message and return the response to send (None for notifications,
    /// client responses and cancelled requests).
    ///
    /// Rules (SPEC Part 3): era per request (modern `_meta` -> modern even on an initialized
    /// stdio process; otherwise legacy: stdio needs a prior `initialize` except for `ping`,
    /// else -32600 "not initialized: ..."; HTTP uses `http.protocol_version`, default
    /// `HTTP_DEFAULT_LEGACY`, a value that is neither legacy nor modern -> -32022). Methods:
    /// `initialize`, `notifications/initialized`, `notifications/cancelled`, `ping`,
    /// `server/discover` (modern only), `tools/list`, `tools/call`; unknown request method ->
    /// -32601; unknown notifications ignored. HTTP modern header checks: `http.protocol_version`
    /// must equal the `_meta` version and `http.method` the body method, else -32020. Modern
    /// results go through `version::modernize`. Records the client in `self.clients` on every
    /// request (first request logged once) and, on Stdio, calls `push::start` once
    /// (`peer.link_started`) after the first request. `tools/call` builds a `ToolCx`
    /// (progress iff a progressToken; cancel token registered in `peer.inflight` under
    /// `id.to_string()` on Stdio and removed when done), runs `tools::call` and answers
    /// `tools::result_json`; unknown tool -> -32602 "Unknown tool: <name>".
    pub async fn handle(
        self: &Arc<Self>,
        peer: &Arc<Peer>,
        msg: Incoming,
        notifier: &Notifier,
        http: Option<&HttpMeta>,
    ) -> Option<Value> {
        match msg {
            Incoming::Invalid { error, id } => Some(jsonrpc::error_response(&id, &error)),
            Incoming::Response => None,
            Incoming::Notification { method, params } => {
                notification(peer, &method, &params);
                None
            }
            Incoming::Request { id, method, params } => {
                match self
                    .request(peer, &id, &method, &params, notifier, http)
                    .await
                {
                    None => None,
                    Some(Ok(result)) => Some(jsonrpc::response(&id, result)),
                    Some(Err(e)) => Some(jsonrpc::error_response(&id, &e)),
                }
            }
        }
    }

    /// One request: header checks, client tracking, era gates, then the method. `None` only
    /// for a cancelled `tools/call` (no response is sent).
    async fn request(
        self: &Arc<Self>,
        peer: &Arc<Peer>,
        id: &Value,
        method: &str,
        params: &Value,
        notifier: &Notifier,
        http: Option<&HttpMeta>,
    ) -> Option<Result<Value, RpcError>> {
        let meta = match version::modern_meta(params) {
            Ok(meta) => meta,
            Err(e) => return Some(Err(e)),
        };
        if let (Some(meta), Some(http)) = (&meta, http) {
            if http
                .protocol_version
                .as_deref()
                .is_some_and(|v| v != meta.version)
            {
                return Some(Err(RpcError::header_mismatch("MCP-Protocol-Version")));
            }
            if http.method.as_deref().is_some_and(|m| m != method) {
                return Some(Err(RpcError::header_mismatch("Mcp-Method")));
            }
        }
        let modern = meta.is_some();
        tracing::debug!(method = ?log_text(method, LOG_METHOD_BYTES), modern, "request");
        self.observe(peer, method, params, meta.as_ref());
        if peer.transport == Transport::Stdio && !peer.link_started.swap(true, Ordering::SeqCst) {
            push::start(Arc::clone(self), Arc::clone(peer), notifier.clone());
        }
        if !modern && peer.transport == Transport::Http && method != "initialize" {
            let v = http
                .and_then(|h| h.protocol_version.as_deref())
                .unwrap_or(version::HTTP_DEFAULT_LEGACY);
            // A legacy request must not claim a modern version in its header (R3-7).
            if version::is_modern(v) {
                return Some(Err(RpcError::header_mismatch("MCP-Protocol-Version")));
            }
            if !version::is_legacy(v) {
                return Some(Err(RpcError::unsupported_version(v)));
            }
        }
        let outcome = match method {
            "initialize" => Some(self.initialize(peer, params)),
            "server/discover" => Some(if modern {
                Ok(discover_result())
            } else {
                Err(RpcError::invalid_params(
                    "server/discover requires io.modelcontextprotocol/protocolVersion in _meta",
                ))
            }),
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(legacy_gate(peer, modern).map(|()| tools::list_json())),
            "tools/call" => match legacy_gate(peer, modern) {
                Ok(()) => self.tools_call(peer, id, params, notifier).await,
                Err(e) => Some(Err(e)),
            },
            _ => Some(Err(RpcError::method_not_found(method))),
        };
        outcome.map(|r| r.map(|v| if modern { version::modernize(v) } else { v }))
    }

    /// `initialize` (legacy): negotiate, store the session on stdio, reconfigure the delivery
    /// ladder, and answer the capabilities with `claude/channel` iff push is capable.
    fn initialize(&self, peer: &Peer, params: &Value) -> Result<Value, RpcError> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("initialize requires protocolVersion"))?;
        let negotiated = version::negotiate_legacy(requested);
        let client = params.get("clientInfo").and_then(version::client_info_from);
        let caps = capabilities_of(params);
        let harness = clients::harness_of(client.as_ref().map_or("", |c| c.name.as_str()));
        let push_capable = peer.transport == Transport::Stdio
            && self.config.channel_push
            && harness == Harness::ClaudeCode;
        if peer.transport == Transport::Stdio {
            {
                let mut session = lock(&peer.session);
                session.version = Some(negotiated.to_owned());
                session.client = client;
                session.capabilities = caps;
            }
            lock(&peer.delivery)
                .reconfigure(clients::ladder_for(harness, push_capable), push_capable);
        }
        let capabilities = if push_capable {
            json!({"tools": {"listChanged": false}, "experimental": {"claude/channel": {}}})
        } else {
            json!({"tools": {"listChanged": false}})
        };
        Ok(json!({
            "protocolVersion": negotiated,
            "capabilities": capabilities,
            "serverInfo": {"name": SERVER_NAME, "title": "Agora", "version": SERVER_VERSION},
            "instructions": INSTRUCTIONS,
        }))
    }

    /// `tools/call`: the tool table check, the in-flight registration (stdio) and the run.
    /// `None` when the request was cancelled.
    async fn tools_call(
        self: &Arc<Self>,
        peer: &Arc<Peer>,
        id: &Value,
        params: &Value,
        notifier: &Notifier,
    ) -> Option<Result<Value, RpcError>> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Some(Err(RpcError::invalid_params("tools/call requires name")));
        };
        if !tools::table().iter().any(|t| t.name == name) {
            return Some(Err(RpcError::invalid_params(format!(
                "Unknown tool: {name}"
            ))));
        }
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let progress = version::progress_token(params).map(|token| Progress {
            token,
            notifier: notifier.clone(),
        });
        // Stdio: the reader registered this request before its task started, so a cancel read
        // right behind it has already cancelled the token and the call is not run (R3-1).
        let stdio = peer.transport == Transport::Stdio;
        let cancel = if stdio {
            register_inflight(peer, id)
        } else {
            peer.shutdown.child_token()
        };
        let cx = ToolCx {
            engine: self,
            peer,
            progress,
            cancel: cancel.clone(),
        };
        let outcome: Option<Option<ToolResult>> = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            r = tools::call(&cx, name, args) => Some(r),
        };
        if stdio {
            release_inflight(peer, id);
        }
        match outcome {
            None => None,
            Some(None) => Some(Err(RpcError::invalid_params(format!(
                "Unknown tool: {name}"
            )))),
            Some(Some(r)) => Some(Ok(tools::result_json(&r))),
        }
    }

    /// Record the caller in the client registry when its identity is known: modern from
    /// `_meta`; legacy `initialize` from its params; legacy stdio from the session; legacy
    /// HTTP only through `initialize` (later requests use the token's last record).
    fn observe(&self, peer: &Peer, method: &str, params: &Value, meta: Option<&RequestMeta>) {
        let (info, protocol_version, caps, era) = match meta {
            Some(meta) => (
                meta.client_info.clone().unwrap_or_else(|| ClientInfo {
                    name: "unknown".to_owned(),
                    version: String::new(),
                }),
                meta.version.clone(),
                meta.client_capabilities.clone(),
                Era::Modern,
            ),
            None if method == "initialize" => {
                let Some(info) = params.get("clientInfo").and_then(version::client_info_from)
                else {
                    return;
                };
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(version::LEGACY_LATEST);
                (
                    info,
                    version::negotiate_legacy(requested).to_owned(),
                    capabilities_of(params),
                    Era::Legacy,
                )
            }
            None if peer.transport == Transport::Stdio => {
                let session = lock(&peer.session);
                let (Some(info), Some(v)) = (session.client.clone(), session.version.clone())
                else {
                    return;
                };
                (info, v, session.capabilities.clone(), Era::Legacy)
            }
            None => return,
        };
        self.clients.observe(
            peer.token_id.as_deref(),
            &info,
            &protocol_version,
            era,
            peer.transport,
            method,
            &caps,
            now_ms(),
        );
    }
}

/// Registers a stdio request in `peer.inflight` under `id.to_string()` and returns its cancel
/// token: an existing entry is returned as it is, otherwise a new child of `peer.shutdown`.
/// The stdio reader calls this before the request's task starts (R3-1), so a
/// `notifications/cancelled` read right after the request always finds the token.
pub fn register_inflight(peer: &Peer, id: &Value) -> CancellationToken {
    lock(&peer.inflight)
        .entry(id.to_string())
        .or_insert_with(|| peer.shutdown.child_token())
        .clone()
}

/// Removes the entry `register_inflight` made for `id`, once that request has ended.
pub fn release_inflight(peer: &Peer, id: &Value) {
    lock(&peer.inflight).remove(&id.to_string());
}

/// Untrusted text cut to at most `max` bytes at a char boundary, for log fields (R3-8).
fn log_text(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

/// Longest request method name written to a log line.
const LOG_METHOD_BYTES: usize = 64;

/// Lock a std mutex, recovering from poisoning (the guarded state stays valid).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A legacy stdio request needs a prior `initialize`; modern and HTTP requests pass (the HTTP
/// version is checked before routing).
fn legacy_gate(peer: &Peer, modern: bool) -> Result<(), RpcError> {
    if modern || peer.transport == Transport::Http {
        return Ok(());
    }
    if lock(&peer.session).version.is_some() {
        Ok(())
    } else {
        Err(RpcError::invalid_request(
            "not initialized: send initialize, or send io.modelcontextprotocol/protocolVersion in _meta",
        ))
    }
}

/// The `server/discover` result (made modern by the caller).
fn discover_result() -> Value {
    json!({
        "supportedVersions": version::SUPPORTED_VERSIONS,
        "capabilities": {"tools": {"listChanged": false}},
        "instructions": INSTRUCTIONS,
    })
}

/// The request's `capabilities` object, or `{}` when absent or not an object.
fn capabilities_of(params: &Value) -> Value {
    params
        .get("capabilities")
        .filter(|c| c.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

/// Notifications: `notifications/initialized` marks the session ready; `notifications/cancelled`
/// cancels the in-flight request with that id (stdio). Everything else is ignored.
fn notification(peer: &Peer, method: &str, params: &Value) {
    match method {
        "notifications/initialized" => {
            lock(&peer.session).initialized = true;
        }
        "notifications/cancelled" => {
            let Some(request_id) = params
                .get("requestId")
                .filter(|id| id.is_string() || id.is_number())
            else {
                return;
            };
            // The entry stays until the call ends (`release_inflight`): a call whose task starts
            // after this cancel must still find its cancelled token (R3-1).
            let token = lock(&peer.inflight).get(&request_id.to_string()).cloned();
            if let Some(token) = token {
                token.cancel();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::error_code;
    use crate::version::SUPPORTED_VERSIONS;
    use agora_crypto::Identity;
    use agora_daemon::Paths;
    use std::time::Duration;

    fn ctx(home: &tempfile::TempDir) -> Arc<Ctx> {
        Arc::new(Ctx {
            paths: Paths::new(home.path()),
            identity: Identity::from_seed([7; 32]),
            label: "t".into(),
        })
    }

    /// An HTTP peer for `token_id`; HTTP never starts the push link.
    fn http_peer(home: &tempfile::TempDir) -> (Arc<Engine>, Arc<Peer>) {
        let engine = Engine::new(EngineConfig::http());
        let peer = Arc::new(Peer::new(
            ctx(home),
            Transport::Http,
            Some("t_00000001".into()),
        ));
        (engine, peer)
    }

    /// A stdio peer whose push link is marked as started, so no push task is spawned: these
    /// tests do not exercise channel delivery.
    fn stdio_peer(home: &tempfile::TempDir, config: EngineConfig) -> (Arc<Engine>, Arc<Peer>) {
        let engine = Engine::new(config);
        let peer = Arc::new(Peer::new(ctx(home), Transport::Stdio, None));
        peer.link_started.store(true, Ordering::SeqCst);
        (engine, peer)
    }

    async fn send(
        engine: &Arc<Engine>,
        peer: &Arc<Peer>,
        body: Value,
        http: Option<&HttpMeta>,
    ) -> Option<Value> {
        let msg = jsonrpc::classify(body);
        tokio::time::timeout(
            Duration::from_secs(5),
            engine.handle(peer, msg, &Notifier::none(), http),
        )
        .await
        .expect("handle did not finish")
    }

    fn request(id: i64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn modern(id: i64, method: &str) -> Value {
        request(
            id,
            method,
            json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            }}),
        )
    }

    fn initialize(id: i64, version: &str, client: &str) -> Value {
        request(
            id,
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": client, "version": "1.0"},
            }),
        )
    }

    fn notification_body(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": method, "params": params})
    }

    #[tokio::test]
    async fn initialize_echoes_supported_legacy_version() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(
            &engine,
            &peer,
            initialize(1, "2025-06-18", "some-client"),
            None,
        )
        .await
        .expect("a response");
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert!(r["result"]["capabilities"]["experimental"].is_null());
        assert_eq!(r["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(r["result"]["serverInfo"]["title"], "Agora");
        assert_eq!(r["result"]["instructions"], INSTRUCTIONS);
        assert!(r.get("error").is_none());
    }

    #[tokio::test]
    async fn initialize_counter_offers_unknown_versions() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        for requested in ["garbage", "2026-07-28"] {
            let r = send(&engine, &peer, initialize(1, requested, "c"), None)
                .await
                .expect("a response");
            assert_eq!(r["result"]["protocolVersion"], "2025-11-25", "{requested}");
        }
    }

    #[tokio::test]
    async fn initialize_without_protocol_version_is_invalid_params() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let body = request(1, "initialize", json!({"capabilities": {}}));
        let r = send(&engine, &peer, body, None).await.expect("a response");
        assert_eq!(error_code(&r), Some(-32602));
    }

    #[tokio::test]
    async fn experimental_channel_only_for_stdio_claude_code_with_push() {
        let home = tempfile::tempdir().expect("tempdir");
        let claude =
            |r: &Value| r["result"]["capabilities"]["experimental"]["claude/channel"].is_object();

        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let r = send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await
        .expect("a response");
        assert!(
            claude(&r),
            "stdio + claude-code + push must declare channel"
        );

        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let r = send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "codex-mcp-client"),
            None,
        )
        .await
        .expect("a response");
        assert!(!claude(&r), "another harness must not declare channel");

        let mut no_push = EngineConfig::stdio();
        no_push.channel_push = false;
        let (engine, peer) = stdio_peer(&home, no_push);
        let r = send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await
        .expect("a response");
        assert!(!claude(&r), "channel_push off must not declare channel");

        let (engine, peer) = http_peer(&home);
        let r = send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await
        .expect("a response");
        assert!(!claude(&r), "http must not declare channel");
    }

    #[tokio::test]
    async fn push_enabled_requires_initialized_claude_code_stdio() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await;
        assert!(!peer.push_enabled(&engine), "not initialized yet");
        let none = send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        assert!(none.is_none());
        assert!(peer.push_enabled(&engine));

        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(&engine, &peer, initialize(1, "2025-11-25", "codex"), None).await;
        send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        assert!(!peer.push_enabled(&engine), "not claude-code");
    }

    #[tokio::test]
    async fn discover_result_shape() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(&engine, &peer, modern(1, "server/discover"), None)
            .await
            .expect("a response");
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["supportedVersions"], json!(SUPPORTED_VERSIONS));
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert!(r["result"]["instructions"].is_string());
        assert_eq!(
            r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
        assert!(r["result"]["capabilities"]["experimental"].is_null());
    }

    #[tokio::test]
    async fn discover_without_meta_is_invalid_params() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(
            &engine,
            &peer,
            request(1, "server/discover", json!({})),
            None,
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32602));
    }

    #[tokio::test]
    async fn modern_unsupported_version_lists_supported_versions() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let body = request(
            1,
            "tools/list",
            json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": "1999-01-01",
                "io.modelcontextprotocol/clientCapabilities": {},
            }}),
        );
        let r = send(&engine, &peer, body, None).await.expect("a response");
        assert_eq!(error_code(&r), Some(-32022));
        assert_eq!(r["error"]["data"]["supported"], json!(SUPPORTED_VERSIONS));
        assert_eq!(r["error"]["data"]["requested"], "1999-01-01");
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(
            &engine,
            &peer,
            request(1, "resources/list", json!({})),
            None,
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32601));
    }

    #[tokio::test]
    async fn stdio_legacy_tools_list_needs_initialize_and_ping_does_not() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let r = send(&engine, &peer, request(1, "ping", json!({})), None)
            .await
            .expect("a response");
        assert_eq!(r["result"], json!({}));

        let r = send(&engine, &peer, request(2, "tools/list", json!({})), None)
            .await
            .expect("a response");
        assert_eq!(error_code(&r), Some(-32600));

        send(
            &engine,
            &peer,
            initialize(3, "2025-11-25", "some-client"),
            None,
        )
        .await;
        send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        let r = send(&engine, &peer, request(4, "tools/list", json!({})), None)
            .await
            .expect("a response");
        let tools = r["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), tools::table().len());
        assert_eq!(tools.len(), 33, "the tool table has 33 tools");
        assert!(
            r["result"]["resultType"].is_null(),
            "legacy results are not modern"
        );
    }

    #[tokio::test]
    async fn modern_tools_list_has_result_type() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(&engine, &peer, modern(1, "tools/list"), None)
            .await
            .expect("a response");
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(
            r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
        assert!(r["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn http_legacy_bad_header_is_unsupported_version() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let http = HttpMeta {
            protocol_version: Some("1999-01-01".into()),
            method: None,
        };
        let r = send(
            &engine,
            &peer,
            request(1, "tools/list", json!({})),
            Some(&http),
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32022));
        assert_eq!(r["error"]["data"]["requested"], "1999-01-01");
    }

    #[tokio::test]
    async fn modern_header_mismatch_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let wrong_version = HttpMeta {
            protocol_version: Some("2025-11-25".into()),
            method: None,
        };
        let r = send(
            &engine,
            &peer,
            modern(1, "tools/list"),
            Some(&wrong_version),
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32020));

        let wrong_method = HttpMeta {
            protocol_version: None,
            method: Some("ping".into()),
        };
        let r = send(&engine, &peer, modern(2, "tools/list"), Some(&wrong_method))
            .await
            .expect("a response");
        assert_eq!(error_code(&r), Some(-32020));
    }

    #[tokio::test]
    async fn notifications_and_responses_get_no_response() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        for body in [
            notification_body("notifications/initialized", json!({})),
            notification_body("notifications/whatever", json!({})),
            json!({"jsonrpc": "2.0", "id": 9, "result": {}}),
        ] {
            assert!(send(&engine, &peer, body, None).await.is_none());
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(
            &engine,
            &peer,
            request(
                1,
                "tools/call",
                json!({"name": "no_such_tool", "arguments": {}}),
            ),
            None,
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32602));
        assert_eq!(r["error"]["message"], "Unknown tool: no_such_tool");

        let r = send(&engine, &peer, request(2, "tools/call", json!({})), None)
            .await
            .expect("a response");
        assert_eq!(error_code(&r), Some(-32602));
    }

    #[tokio::test]
    async fn cancelled_notification_cancels_the_entry_until_the_call_ends() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let token = CancellationToken::new();
        lock(&peer.inflight).insert("7".to_owned(), token.clone());
        let none = send(
            &engine,
            &peer,
            notification_body("notifications/cancelled", json!({"requestId": 7})),
            None,
        )
        .await;
        assert!(none.is_none());
        assert!(token.is_cancelled());
        assert!(
            lock(&peer.inflight).contains_key("7"),
            "the entry stays until the call ends"
        );
        release_inflight(&peer, &json!(7));
        assert!(lock(&peer.inflight).is_empty());
    }

    #[tokio::test]
    async fn a_cancel_read_before_the_call_starts_cancels_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "some-client"),
            None,
        )
        .await;
        send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        // The stdio reader registers the request, then reads the cancel, then the task runs.
        let early = register_inflight(&peer, &json!(8));
        send(
            &engine,
            &peer,
            notification_body("notifications/cancelled", json!({"requestId": 8})),
            None,
        )
        .await;
        assert!(early.is_cancelled());
        let r = send(
            &engine,
            &peer,
            request(
                8,
                "tools/call",
                json!({"name": "inbox_wait", "arguments": {"timeout_secs": 55}}),
            ),
            None,
        )
        .await;
        assert!(
            r.is_none(),
            "a call cancelled before it starts gets no response: {r:?}"
        );
        assert!(
            lock(&peer.inflight).is_empty(),
            "the call released its entry"
        );
    }

    #[tokio::test]
    async fn http_legacy_request_with_a_modern_header_is_a_header_mismatch() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let http = HttpMeta {
            protocol_version: Some("2026-07-28".into()),
            method: None,
        };
        let r = send(
            &engine,
            &peer,
            request(1, "tools/list", json!({})),
            Some(&http),
        )
        .await
        .expect("a response");
        assert_eq!(error_code(&r), Some(-32020));
        assert_eq!(
            r["error"]["message"],
            "Header mismatch: MCP-Protocol-Version"
        );
    }
}
