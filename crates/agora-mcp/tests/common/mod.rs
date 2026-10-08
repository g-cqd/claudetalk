//! Shared harness and mock MCP client for the agora-mcp integration tests.
//!
//! Contract (SPEC "Tests"): an in-process daemon in a SHORT temp home under /tmp (the unix
//! socket path must stay under 104 bytes), identities from fixed seeds, owner bootstrap and
//! channel helpers; `StdioClient` drives `stdio::serve` over `tokio::io::duplex` (raw lines
//! in/out, request / notify / next_notification, `initialize`, the modern `_meta` helper,
//! `call_tool` for both eras); `HttpHarness` starts `http::serve` on 127.0.0.1:0 with N tokens
//! and offers a raw `post()` returning (status, headers, body) via reqwest. Every wait in the
//! harness has a timeout so a bug fails instead of hanging.
#![allow(dead_code, unused_variables)]

use agora_client::Ctx;
use agora_client::api::chat::{self, ChatParams};
use agora_client::api::org::{self, ChannelCreateParams, InitParams};
use agora_crypto::Identity;
use agora_daemon::{Daemon, DaemonConfig, DaemonError, Paths};
use agora_mcp::McpError;
use agora_mcp::engine::{Engine, EngineConfig, Peer, Transport};
use agora_mcp::http::{self, HttpOptions};
use agora_mcp::stdio;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Upper bound on each wait for a request's response.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// Upper bound on a daemon or server stopping after it was asked to.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Upper bound on one raw HTTP POST, body included.
const POST_WAIT: Duration = Duration::from_secs(15);
/// Capacity of the in-memory pipe between the mock client and `stdio::serve`.
const PIPE_BYTES: usize = 1 << 20;

/// A running in-process daemon plus its temp home.
pub struct World {
    home: tempfile::TempDir,
    paths: Paths,
    token: CancellationToken,
    task: JoinHandle<Result<(), DaemonError>>,
}

impl World {
    /// Start a daemon (single writer, event bus) in a fresh short temp home and wait until
    /// it accepts connections.
    pub async fn start() -> World {
        let home = tempfile::Builder::new()
            .prefix("am")
            .tempdir_in("/tmp")
            .expect("create a short temp home under /tmp");
        let config = DaemonConfig {
            home: home.path().to_path_buf(),
            open_registration: true,
            handle_signals: false,
            ..Default::default()
        };
        // `bind` creates the unix socket before it returns, so connections queue from here on.
        let daemon = Daemon::bind(config)
            .await
            .expect("bind the in-process daemon");
        let token = daemon.shutdown_token();
        let task = tokio::spawn(daemon.run());
        World {
            paths: Paths::new(home.path()),
            home,
            token,
            task,
        }
    }

    /// The daemon's paths (socket, db, home).
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// A client context for `Identity::from_seed([seed; 32])`, label `agent-<seed>`. Not
    /// registered; `Ctx.paths` is this world's paths.
    pub fn ctx(&self, seed: u8) -> Ctx {
        Ctx {
            paths: self.paths.clone(),
            identity: Identity::from_seed([seed; 32]),
            label: format!("agent-{seed}"),
        }
    }

    /// The bootstrap owner (seed 1), registered as owner.
    pub async fn owner(&self) -> Ctx {
        let ctx = self.ctx(1);
        org::init(
            &ctx,
            InitParams {
                owner: true,
                label: None,
            },
        )
        .await
        .expect("bootstrap the owner (seed 1)");
        ctx
    }

    /// `ctx(seed)` registered as a normal agent (the owner exists first).
    pub async fn agent(&self, seed: u8) -> Ctx {
        let ctx = self.ctx(seed);
        org::init(
            &ctx,
            InitParams {
                owner: false,
                label: None,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("register agent (seed {seed}): {e}"));
        ctx
    }

    /// Create the plain group channel `id` as `admin` with `members` (the admin is always a
    /// member).
    pub async fn channel(&self, admin: &Ctx, id: &str, members: &[&Ctx]) {
        // The admin is a member already; listing it again must not add a second entry.
        let admin_pk = agora_store::hex(&admin.identity.public());
        let members: Vec<String> = members
            .iter()
            .map(|m| agora_store::hex(&m.identity.public()))
            .filter(|pk| *pk != admin_pk)
            .collect();
        org::channel_create(
            admin,
            ChannelCreateParams {
                id: id.to_owned(),
                kind: "group".to_owned(),
                name: id.to_owned(),
                parent: None,
                members,
                encrypt: false,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("create channel {id}: {e}"));
    }

    /// Post `text` from `from` to `channel` and return its seq.
    pub async fn post(&self, from: &Ctx, channel: &str, text: &str) -> i64 {
        let out = chat::chat(
            from,
            ChatParams {
                channel: channel.to_owned(),
                text: text.to_owned(),
                thread_of: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("post to {channel}: {e}"));
        out.json["seq"]
            .as_i64()
            .unwrap_or_else(|| panic!("chat output has no integer seq: {}", out.json))
    }

    /// Stop the daemon and remove the temp home.
    pub async fn shutdown(self) {
        let World {
            home, token, task, ..
        } = self;
        token.cancel();
        match tokio::time::timeout(STOP_WAIT, task).await {
            Err(_) => panic!("daemon did not stop within {STOP_WAIT:?} of cancellation"),
            Ok(Err(join)) => panic!("daemon run task panicked: {join}"),
            Ok(Ok(Err(e))) => panic!("daemon run returned an error: {e}"),
            Ok(Ok(Ok(()))) => {}
        }
        // Removed only after the daemon has stopped, so it no longer writes into the home.
        drop(home);
    }
}

/// A mock MCP client speaking newline-delimited JSON-RPC to `stdio::serve` over a duplex pipe.
pub struct StdioClient {
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    next_id: i64,
    /// Notifications read while waiting for a response, handed out by `next_notification`.
    pending: VecDeque<Value>,
    serve: JoinHandle<Result<(), McpError>>,
    cancel: CancellationToken,
}

impl StdioClient {
    /// Build `Engine::new(cfg)` and `Peer::new(Arc::new(ctx), Transport::Stdio, None)` and run
    /// `stdio::serve` on the other end of the pipe.
    pub async fn start(world: &World, ctx: Ctx, cfg: EngineConfig) -> StdioClient {
        let (client_side, server_side) = tokio::io::duplex(PIPE_BYTES);
        let (server_read, server_write) = tokio::io::split(server_side);
        let (client_read, client_write) = tokio::io::split(client_side);
        let engine = Engine::new(cfg);
        let peer = Arc::new(Peer::new(Arc::new(ctx), Transport::Stdio, None));
        let cancel = CancellationToken::new();
        let serve = tokio::spawn(stdio::serve(
            engine,
            peer,
            server_read,
            server_write,
            cancel.clone(),
        ));
        StdioClient {
            reader: BufReader::new(client_read).lines(),
            writer: client_write,
            next_id: 1,
            pending: VecDeque::new(),
            serve,
            cancel,
        }
    }

    /// Write `line` plus a newline without any processing.
    pub async fn send_raw(&mut self, line: &str) {
        let framed = format!("{line}\n");
        self.writer
            .write_all(framed.as_bytes())
            .await
            .expect("write a line to the stdio server");
    }

    /// The next message the server wrote, or None after `within` (or on EOF).
    pub async fn read_message(&mut self, within: Duration) -> Option<Value> {
        tokio::time::timeout(within, self.recv())
            .await
            .unwrap_or_default()
    }

    /// Send a request with a fresh id and return the full response; notifications that arrive
    /// meanwhile are buffered for `next_notification`.
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        match tokio::time::timeout(REQUEST_WAIT, self.await_response(id, method)).await {
            Ok(response) => response,
            Err(_elapsed) => panic!("no response to {method} (id {id}) within {REQUEST_WAIT:?}"),
        }
    }

    /// Send a notification (no id).
    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The next buffered or incoming notification, or None after `within`.
    pub async fn next_notification(&mut self, within: Duration) -> Option<Value> {
        if let Some(buffered) = self.pending.pop_front() {
            return Some(buffered);
        }
        tokio::time::timeout(within, self.next_incoming_notification())
            .await
            .unwrap_or_default()
    }

    /// `initialize` with protocol 2025-11-25 and the given client name, then
    /// `notifications/initialized`; returns the initialize result.
    pub async fn initialize(&mut self, client_name: &str) -> Value {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": client_name, "version": "1.0"},
                }),
            )
            .await;
        let result = result_of(response, "initialize");
        self.notify("notifications/initialized", json!({})).await;
        result
    }

    /// The modern `_meta` object (protocolVersion 2026-07-28, empty clientCapabilities,
    /// clientInfo `{name: client_name, version: "1.0"}`).
    pub fn modern_meta(client_name: &str) -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": client_name, "version": "1.0"},
        })
    }

    /// Legacy-era `tools/call`; returns the result object (content / structuredContent / isError).
    pub async fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let response = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await;
        result_of(response, "tools/call")
    }

    /// Modern-era `tools/call` (with `modern_meta`); returns the result object.
    pub async fn call_tool_modern(&mut self, name: &str, args: Value) -> Value {
        let response = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": args,
                    "_meta": Self::modern_meta("mock-modern"),
                }),
            )
            .await;
        result_of(response, "tools/call")
    }

    /// Close the client's input (EOF) and wait for `serve` to return.
    pub async fn close(mut self) {
        // A split write half sends EOF only through `shutdown`; dropping it does not.
        self.writer.shutdown().await.expect("close the stdio input");
        match tokio::time::timeout(STOP_WAIT, &mut self.serve).await {
            Err(_elapsed) => {
                self.cancel.cancel();
                panic!("stdio::serve did not return within {STOP_WAIT:?} of EOF");
            }
            Ok(Err(join)) => panic!("stdio::serve task panicked: {join}"),
            Ok(Ok(Err(e))) => panic!("stdio::serve returned an error: {e}"),
            Ok(Ok(Ok(()))) => {}
        }
    }

    async fn send(&mut self, msg: &Value) {
        self.send_raw(&msg.to_string()).await;
    }

    /// The next line as JSON; None on EOF. Panics on a line that is not JSON, because the
    /// server must only ever write protocol messages to stdout.
    async fn recv(&mut self) -> Option<Value> {
        let line = self
            .reader
            .next_line()
            .await
            .expect("read a line from the stdio server")?;
        Some(
            serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdio server wrote a non-JSON line ({e}): {line}")),
        )
    }

    /// Read until the response with `id` arrives; notifications are buffered on the way.
    async fn await_response(&mut self, id: i64, method: &str) -> Value {
        loop {
            let Some(msg) = self.recv().await else {
                panic!("stdio server closed before answering {method} (id {id})");
            };
            match msg.get("id") {
                None => self.pending.push_back(msg),
                Some(got) if *got == json!(id) => return msg,
                Some(got) => {
                    panic!("unexpected message with id {got} while waiting for {method} (id {id})")
                }
            }
        }
    }

    /// The next message, which must be a notification: a message with an id fails the test,
    /// because no request is outstanding that could own it.
    async fn next_incoming_notification(&mut self) -> Option<Value> {
        let msg = self.recv().await?;
        match msg.get("id") {
            None => Some(msg),
            Some(id) => panic!("unexpected message with id {id} while waiting for a notification"),
        }
    }
}

/// The `result` of a response; panics with the error object when the response has one.
fn result_of(response: Value, method: &str) -> Value {
    if let Some(error) = response.get("error") {
        panic!("{method} answered with an error: {error}");
    }
    match response.get("result") {
        Some(result) => result.clone(),
        None => panic!("{method} response has neither result nor error: {response}"),
    }
}

/// `http::serve` on 127.0.0.1:0 with N freshly created tokens, plus a raw POST helper.
pub struct HttpHarness {
    url: String,
    tokens: Vec<String>,
    client: reqwest::Client,
    cancel: CancellationToken,
    serve: JoinHandle<Result<(), McpError>>,
}

impl HttpHarness {
    /// Create `tokens` tokens (labels `token-<i>`) in a temp token file, bind 127.0.0.1:0 and
    /// serve with `EngineConfig::http()`.
    pub async fn start(world: &World, tokens: usize) -> HttpHarness {
        let token_file = agora_mcp::tokens::default_path(world.paths());
        let now = agora_daemon::now_ms();
        let mut issued = Vec::with_capacity(tokens);
        for i in 0..tokens {
            let (token, _entry) = agora_mcp::tokens::add(&token_file, &format!("t{i}"), now)
                .expect("create an HTTP bearer token");
            issued.push(token);
        }
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback socket address");
        let listener = http::bind(loopback, false)
            .await
            .expect("bind the MCP HTTP listener on 127.0.0.1:0");
        let addr = listener.local_addr().expect("listener local address");
        let cancel = CancellationToken::new();
        let opts = HttpOptions {
            paths: world.paths().clone(),
            token_file,
            allowed_origins: Vec::new(),
        };
        let serve = tokio::spawn(http::serve(
            listener,
            opts,
            Engine::new(EngineConfig::http()),
            cancel.clone(),
        ));
        HttpHarness {
            url: format!("http://{addr}/mcp"),
            tokens: issued,
            // No proxy: the harness talks to loopback directly, whatever the environment says.
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build the HTTP client"),
            cancel,
            serve,
        }
    }

    /// `http://127.0.0.1:<port>/mcp`.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The i-th bearer token string.
    pub fn token(&self, i: usize) -> &str {
        match self.tokens.get(i) {
            Some(token) => token.as_str(),
            None => panic!("token {i} was not created ({} exist)", self.tokens.len()),
        }
    }

    /// POST `body` to `/mcp` with `Authorization: Bearer <token>` (when Some), the default
    /// headers `Content-Type: application/json` and `Accept: application/json`, then `headers`
    /// (which replace a default of the same name). Returns (status, response headers, body).
    pub async fn post(
        &self,
        token: Option<&str>,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (u16, reqwest::header::HeaderMap, String) {
        let mut map = HeaderMap::new();
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        map.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(token) = token {
            map.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));
        }
        // The first caller value for a name replaces the default; repeats are appended, so a
        // test can send the same header twice on purpose.
        let mut seen: HashSet<HeaderName> = HashSet::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .unwrap_or_else(|e| panic!("invalid header name {name:?}: {e}"));
            let value = header_value(value);
            if seen.insert(name.clone()) {
                map.insert(name, value);
            } else {
                map.append(name, value);
            }
        }
        let response = self
            .client
            .post(&self.url)
            .headers(map)
            .body(body.to_owned())
            .timeout(POST_WAIT)
            .send()
            .await
            .expect("POST /mcp completes");
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        let text = response
            .text()
            .await
            .expect("read the POST /mcp response body");
        (status, response_headers, text)
    }

    /// Stop the server.
    pub async fn shutdown(self) {
        let HttpHarness {
            client,
            cancel,
            serve,
            ..
        } = self;
        // Pooled keep-alive connections would hold the graceful shutdown open, so drop them.
        drop(client);
        cancel.cancel();
        match tokio::time::timeout(STOP_WAIT, serve).await {
            Err(_elapsed) => panic!("http::serve did not stop within {STOP_WAIT:?}"),
            Ok(Err(join)) => panic!("http::serve task panicked: {join}"),
            Ok(Ok(Err(e))) => panic!("http::serve returned an error: {e}"),
            Ok(Ok(Ok(()))) => {}
        }
    }
}

/// A header value from a string the test controls; the message never echoes the value,
/// because some values are bearer tokens.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| panic!("header value is not valid ASCII"))
}
