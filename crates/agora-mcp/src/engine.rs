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
use agora_client::heartbeat;
use agora_client::ops::now_ms;
use agora_deliver::{Harness, Mechanism};
use agora_store::hex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

    /// The harness of `peer`'s client: the name in its session's `clientInfo` when the session has
    /// one (legacy `initialize`), else the name of the last registry record of its token (modern
    /// requests, HTTP tokens), else `Harness::Unknown`.
    pub fn peer_harness(&self, peer: &Peer) -> Harness {
        let session_name = lock(&peer.session).client.as_ref().map(|c| c.name.clone());
        let name = match session_name {
            Some(name) => Some(name),
            None => self
                .clients
                .last_for(peer.token_id.as_deref())
                .map(|record| record.name),
        };
        name.map_or(Harness::Unknown, |name| clients::harness_of(&name))
    }

    /// Channel push is possible for `peer` whose client is `harness`: stdio transport,
    /// `config.channel_push`, and `Harness::ClaudeCode`. The session's initialization is not
    /// checked here (`Engine::push_enabled` adds it).
    pub fn push_capable(&self, peer: &Peer, harness: Harness) -> bool {
        peer.transport == Transport::Stdio
            && self.config.channel_push
            && harness == Harness::ClaudeCode
    }

    /// Channel push is on for `peer`: its legacy session is initialized and has a version, and
    /// `push_capable` holds for its harness (`peer_harness`). The session lock is released before
    /// the harness is read, since reading it locks the session again.
    pub fn push_enabled(&self, peer: &Peer) -> bool {
        let initialized = {
            let s = lock(&peer.session);
            s.initialized && s.version.is_some()
        };
        initialized && self.push_capable(peer, self.peer_harness(peer))
    }

    /// Refreshes the delivery state of `peer` from what its ladder depends on, and returns the
    /// rungs that are armed at `now_ms`. The harness (`peer_harness`) comes first, since the
    /// heartbeats are read from that harness's directory. Then, with no lock held, the file reads:
    /// `capabilities.json` and the heartbeats (stdio only; an HTTP peer's hooks are not on this
    /// host), then the push flag (`push_enabled`). The delivery lock is taken once, at the end:
    /// the tracker is reconfigured only when its ladder differs from the new one, and then
    /// `refresh` stores the heartbeats and the armed rungs (kept to the ladder) so `status_json`
    /// shows live values. Used by the push tick before `tick_with`, and by
    /// `delivery_status` before `status_json`.
    pub fn sync_delivery(&self, peer: &Peer, now_ms: i64) -> BTreeSet<Mechanism> {
        let harness = self.peer_harness(peer);
        let (caps, beats) = if peer.transport == Transport::Stdio {
            let pk = hex(&peer.ctx.identity.public());
            (
                agora_client::capabilities::load(&peer.ctx.paths),
                heartbeat::read_all_for(&peer.ctx.paths, &pk, harness),
            )
        } else {
            (None, BTreeMap::new())
        };
        let push = self.push_enabled(peer);
        let ladder = ladder_from(peer, harness, push, caps.as_ref(), &beats, now_ms);
        let mut armed = heartbeat::armed(&beats, now_ms);
        armed.retain(|m| ladder.contains(m));
        {
            let mut tracker = lock(&peer.delivery);
            if tracker.ladder() != ladder.as_slice() {
                tracker.reconfigure(ladder, push);
            }
            tracker.refresh(beats, armed.clone());
        }
        armed
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
                Ok(cacheable(discover_result()))
            } else {
                Err(RpcError::invalid_params(
                    "server/discover requires io.modelcontextprotocol/protocolVersion in _meta",
                ))
            }),
            "ping" => Some(Ok(json!({}))),
            "tools/list" => Some(
                legacy_gate(peer, modern)
                    .map(|()| tools::list_json())
                    .map(|list| if modern { cacheable(list) } else { list }),
            ),
            "tools/call" => match legacy_gate(peer, modern) {
                Ok(()) => self.tools_call(peer, id, params, notifier).await,
                Err(e) => Some(Err(e)),
            },
            _ => Some(Err(RpcError::method_not_found(method))),
        };
        outcome.map(|r| r.map(|v| if modern { version::modernize(v) } else { v }))
    }

    /// `initialize` (legacy): negotiate, store the session on stdio, reconfigure the delivery
    /// ladder (`ladder_for_peer`: the installed `capabilities.json` and the peer's heartbeats
    /// decide which hook and mod rungs are in it), and answer the capabilities with
    /// `claude/channel` iff push is capable.
    fn initialize(&self, peer: &Peer, params: &Value) -> Result<Value, RpcError> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("initialize requires protocolVersion"))?;
        let negotiated = version::negotiate_legacy(requested);
        let client = params.get("clientInfo").and_then(version::client_info_from);
        let caps = capabilities_of(params);
        let harness = clients::harness_of(client.as_ref().map_or("", |c| c.name.as_str()));
        let push_capable = self.push_capable(peer, harness);
        if peer.transport == Transport::Stdio {
            {
                let mut session = lock(&peer.session);
                session.version = Some(negotiated.to_owned());
                session.client = client;
                session.capabilities = caps;
            }
            // The ladder reads files, so it is computed before the delivery lock is taken (N6).
            let ladder = ladder_for_peer(peer, harness, push_capable);
            lock(&peer.delivery).reconfigure(ladder, push_capable);
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

/// The delivery ladder for `peer` whose client is `harness`, from the given install-time
/// capabilities `caps` (`capabilities.json`, `None` when absent) and heartbeats `beats` of the
/// peer's identity, at `now_ms`: `clients::ladder_for_caps` with the whole `caps` file. Claude
/// reads `caps.claude`; every other harness reads its `caps.harness(h)` entry.
/// An HTTP peer's hooks are not on this host, so for it `caps` is ignored and `beats` is treated
/// as empty (R4-5). Pure apart from reading `peer.transport`.
pub fn ladder_from(
    peer: &Peer,
    harness: Harness,
    push_enabled: bool,
    caps: Option<&agora_client::capabilities::CapabilitiesFile>,
    beats: &BTreeMap<Mechanism, i64>,
    now_ms: i64,
) -> Vec<Mechanism> {
    let stdio = peer.transport == Transport::Stdio;
    let caps = if stdio { caps } else { None };
    let no_beats = BTreeMap::new();
    let beats = if stdio { beats } else { &no_beats };
    clients::ladder_for_caps(harness, push_enabled, caps, beats, now_ms)
}

/// The delivery ladder for `peer` whose client is `harness`: `ladder_from` with the installed
/// `$AGORA_HOME/capabilities.json` (`capabilities::load`; a missing or unreadable file counts as
/// none) and the heartbeats of the peer's identity for `harness` (`heartbeat::read_all_for`), at
/// the current time. Only a stdio peer reads those files (its hooks run on this host). Callers
/// that need the ladder `initialize` uses (for example `whoami`) should call this rather than
/// recompute it.
pub fn ladder_for_peer(peer: &Peer, harness: Harness, push_enabled: bool) -> Vec<Mechanism> {
    let (caps, beats) = if peer.transport == Transport::Stdio {
        let pk = hex(&peer.ctx.identity.public());
        (
            agora_client::capabilities::load(&peer.ctx.paths),
            heartbeat::read_all_for(&peer.ctx.paths, &pk, harness),
        )
    } else {
        (None, BTreeMap::new())
    };
    ladder_from(peer, harness, push_enabled, caps.as_ref(), &beats, now_ms())
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

/// `ttlMs` on modern `tools/list` and `server/discover` results: how long, in milliseconds, a
/// client may reuse the result. Modern `CacheableResult` (MCP 2026-07-28, SEP-2549) requires it
/// as an integer >= 0. The tool list is fixed for the process and identical for every caller,
/// so one hour is safe.
const CACHE_TTL_MS: u64 = 3_600_000;

/// `cacheScope` on modern `tools/list` and `server/discover` results: `"public"` means every
/// caller gets the same answer, so a shared cache may hold it (MCP 2026-07-28, SEP-2549).
const CACHE_SCOPE_PUBLIC: &str = "public";

/// Add the modern `CacheableResult` fields (`CACHE_TTL_MS`, `CACHE_SCOPE_PUBLIC`) to a result
/// object; a non-object result is returned unchanged. Legacy results never get them.
fn cacheable(mut result: Value) -> Value {
    if let Some(map) = result.as_object_mut() {
        map.insert("ttlMs".to_owned(), json!(CACHE_TTL_MS));
        map.insert("cacheScope".to_owned(), json!(CACHE_SCOPE_PUBLIC));
    }
    result
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
        assert!(!engine.push_enabled(&peer), "not initialized yet");
        let none = send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        assert!(none.is_none());
        assert!(engine.push_enabled(&peer));

        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(&engine, &peer, initialize(1, "2025-11-25", "codex"), None).await;
        send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        assert!(!engine.push_enabled(&peer), "not claude-code");
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
        assert_eq!(tools.len(), 60, "the tool table has 60 tools");
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
    async fn modern_tools_list_carries_the_cache_hints() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(&engine, &peer, modern(1, "tools/list"), None)
            .await
            .expect("a response");
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["ttlMs"].as_u64(), Some(3_600_000));
        assert_eq!(r["result"]["cacheScope"], "public");
    }

    #[tokio::test]
    async fn modern_discover_carries_the_cache_hints() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(&engine, &peer, modern(1, "server/discover"), None)
            .await
            .expect("a response");
        assert_eq!(r["result"]["resultType"], "complete");
        assert_eq!(r["result"]["ttlMs"].as_u64(), Some(3_600_000));
        assert_eq!(r["result"]["cacheScope"], "public");
    }

    #[tokio::test]
    async fn legacy_tools_list_has_no_cache_hints() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        let r = send(&engine, &peer, request(1, "tools/list", json!({})), None)
            .await
            .expect("a response");
        assert!(r["result"]["tools"].is_array());
        assert!(r["result"]["ttlMs"].is_null(), "{r}");
        assert!(r["result"]["cacheScope"].is_null(), "{r}");
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
    async fn claude_code_initialize_ladder_includes_installed_hook_rungs() {
        let home = tempfile::tempdir().expect("tempdir");
        let file = agora_client::capabilities::CapabilitiesFile {
            claude: Some(agora_client::capabilities::ClaudeCapabilities {
                stop_hook: true,
                context_hook: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        agora_client::capabilities::save(&Paths::new(home.path()), &file)
            .expect("save capabilities.json");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await;
        let status = lock(&peer.delivery).status_json();
        let ladder: Vec<&str> = status["ladder"]
            .as_array()
            .expect("ladder array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let wait = ladder
            .iter()
            .position(|m| *m == "wait_tool")
            .expect("wait_tool is in the ladder");
        for rung in ["stop_continue", "hook_inject"] {
            assert!(
                ladder
                    .iter()
                    .position(|m| *m == rung)
                    .is_some_and(|i| i < wait),
                "{rung} must come before wait_tool: {ladder:?}"
            );
        }
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

    /// The "now" of the ladder and harness tests.
    const NOW: i64 = 1_000_000_000;

    /// Every Claude Code rung installed by `capabilities.json`.
    fn full_caps() -> agora_client::capabilities::CapabilitiesFile {
        agora_client::capabilities::CapabilitiesFile {
            claude: Some(agora_client::capabilities::ClaudeCapabilities {
                mods: true,
                async_rewake: true,
                stop_hook: true,
                context_hook: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn ladder_from_http_peer_is_wait_tool_whatever_the_caps_and_beats() {
        let home = tempfile::tempdir().expect("tempdir");
        let (_engine, peer) = http_peer(&home);
        let beats: BTreeMap<Mechanism, i64> = Mechanism::ALL.iter().map(|m| (*m, NOW)).collect();
        let ladder = ladder_from(
            &peer,
            Harness::ClaudeCode,
            false,
            Some(&full_caps()),
            &beats,
            NOW,
        );
        assert_eq!(ladder, vec![Mechanism::WaitTool]);
    }

    #[test]
    fn ladder_from_stdio_peer_includes_the_installed_hook_rungs() {
        let home = tempfile::tempdir().expect("tempdir");
        let (_engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let ladder = ladder_from(
            &peer,
            Harness::ClaudeCode,
            false,
            Some(&full_caps()),
            &BTreeMap::new(),
            NOW,
        );
        assert_eq!(
            ladder,
            vec![
                Mechanism::ModSubmit,
                Mechanism::AsyncRewakeHook,
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool,
            ]
        );
    }

    #[test]
    fn ladder_from_stdio_peer_counts_a_recent_heartbeat_without_the_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let (_engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        let beats = BTreeMap::from([(Mechanism::StopContinue, NOW)]);
        let ladder = ladder_from(&peer, Harness::ClaudeCode, false, None, &beats, NOW);
        assert_eq!(ladder, vec![Mechanism::StopContinue, Mechanism::WaitTool]);
    }

    #[test]
    fn peer_harness_is_the_session_client_then_the_token_record_then_unknown() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = http_peer(&home);
        assert_eq!(engine.peer_harness(&peer), Harness::Unknown, "nothing seen");

        let codex = ClientInfo {
            name: "codex-mcp-client".into(),
            version: "1.0".into(),
        };
        engine.clients.observe(
            peer.token_id.as_deref(),
            &codex,
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "tools/call",
            &json!({}),
            NOW,
        );
        assert_eq!(
            engine.peer_harness(&peer),
            Harness::Codex,
            "from the token's last record"
        );

        // Another token's record does not count for this peer.
        let claude = ClientInfo {
            name: "claude-code".into(),
            version: "1.0".into(),
        };
        engine.clients.observe(
            Some("t_00000002"),
            &claude,
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "tools/call",
            &json!({}),
            NOW,
        );
        assert_eq!(engine.peer_harness(&peer), Harness::Codex);

        lock(&peer.session).client = Some(ClientInfo {
            name: "claude-code".into(),
            version: "2.0".into(),
        });
        assert_eq!(
            engine.peer_harness(&peer),
            Harness::ClaudeCode,
            "the session's client wins over the record"
        );
    }

    #[test]
    fn peer_harness_of_a_stdio_session_client_claude_code_is_claude_code() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        lock(&peer.session).client = Some(ClientInfo {
            name: "claude-code".into(),
            version: "2.0".into(),
        });
        assert_eq!(engine.peer_harness(&peer), Harness::ClaudeCode);
    }

    #[tokio::test]
    async fn engine_push_enabled_needs_an_initialized_claude_code_stdio_session() {
        let home = tempfile::tempdir().expect("tempdir");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        assert!(!engine.push_enabled(&peer), "before initialize");
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await;
        assert!(
            !engine.push_enabled(&peer),
            "initialize alone is not initialized"
        );
        send(
            &engine,
            &peer,
            notification_body("notifications/initialized", json!({})),
            None,
        )
        .await;
        assert!(
            engine.push_enabled(&peer),
            "claude-code over stdio with channel_push"
        );

        let (engine, peer) = http_peer(&home);
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
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
        assert!(!engine.push_enabled(&peer), "http never pushes");
    }

    #[tokio::test]
    async fn sync_delivery_arms_the_installed_rung_with_a_fresh_heartbeat() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(home.path());
        let file = agora_client::capabilities::CapabilitiesFile {
            claude: Some(agora_client::capabilities::ClaudeCapabilities {
                stop_hook: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        agora_client::capabilities::save(&paths, &file).expect("save capabilities.json");
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        send(
            &engine,
            &peer,
            initialize(1, "2025-11-25", "claude-code"),
            None,
        )
        .await;
        let pk = hex(&peer.ctx.identity.public());
        heartbeat::beat(&paths, &pk, Mechanism::StopContinue, NOW).expect("beat");

        let armed = engine.sync_delivery(&peer, NOW);
        assert_eq!(armed, BTreeSet::from([Mechanism::StopContinue]));
        let tracker = lock(&peer.delivery);
        assert!(tracker.ladder().contains(&Mechanism::StopContinue));
        let status = tracker.status_json();
        assert!(
            status["ladder"]
                .as_array()
                .is_some_and(|l| l.contains(&json!("stop_continue"))),
            "{status}"
        );
        assert_eq!(status["armed"], json!(["stop_continue"]), "{status}");
        assert_eq!(
            status["heartbeats"]["stop_continue"],
            json!(NOW),
            "{status}"
        );
    }

    #[test]
    fn sync_delivery_a_claude_beat_does_not_arm_a_codex_peer() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(home.path());
        let (engine, peer) = stdio_peer(&home, EngineConfig::stdio());
        lock(&peer.session).client = Some(ClientInfo {
            name: "codex-mcp-client".into(),
            version: "1.0".into(),
        });
        let pk = hex(&peer.ctx.identity.public());
        heartbeat::beat(&paths, &pk, Mechanism::StopContinue, NOW).expect("claude beat");
        assert!(
            engine.sync_delivery(&peer, NOW).is_empty(),
            "a Claude beat must not arm a Codex peer"
        );

        heartbeat::beat_for(&paths, &pk, Harness::Codex, Mechanism::StopContinue, NOW)
            .expect("codex beat");
        assert_eq!(
            engine.sync_delivery(&peer, NOW),
            BTreeSet::from([Mechanism::StopContinue]),
            "a Codex beat arms the Codex peer"
        );
    }

    #[test]
    fn sync_delivery_ignores_files_and_beats_for_an_http_peer() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(home.path());
        let file = agora_client::capabilities::CapabilitiesFile {
            claude: Some(agora_client::capabilities::ClaudeCapabilities {
                stop_hook: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        agora_client::capabilities::save(&paths, &file).expect("save capabilities.json");
        let (engine, peer) = http_peer(&home);
        engine.clients.observe(
            peer.token_id.as_deref(),
            &ClientInfo {
                name: "claude-code".into(),
                version: "2.0".into(),
            },
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "tools/call",
            &json!({}),
            NOW,
        );
        let pk = hex(&peer.ctx.identity.public());
        heartbeat::beat(&paths, &pk, Mechanism::StopContinue, NOW).expect("beat");

        let armed = engine.sync_delivery(&peer, NOW);
        assert!(armed.is_empty(), "{armed:?}");
        assert_eq!(
            lock(&peer.delivery).ladder().to_vec(),
            vec![Mechanism::WaitTool]
        );
    }

    #[test]
    fn push_capable_is_stdio_with_channel_push_and_claude_code() {
        let home = tempfile::tempdir().expect("tempdir");
        for transport in [Transport::Stdio, Transport::Http] {
            for channel_push in [true, false] {
                let mut config = match transport {
                    Transport::Stdio => EngineConfig::stdio(),
                    Transport::Http => EngineConfig::http(),
                };
                config.channel_push = channel_push;
                let engine = Engine::new(config);
                let peer = Peer::new(ctx(&home), transport, None);
                for harness in [Harness::ClaudeCode, Harness::Codex, Harness::Unknown] {
                    let expected = transport == Transport::Stdio
                        && channel_push
                        && harness == Harness::ClaudeCode;
                    assert_eq!(
                        engine.push_capable(&peer, harness),
                        expected,
                        "{transport:?} channel_push={channel_push} {harness:?}"
                    );
                }
            }
        }
    }
}
