//! opencode bridge over its HTTP server (`opencode serve`; DOCS-ONLY, opencode.ai/docs/server).
//!
//! Loopback only (we have no TLS): the URL must be a plain `http://` origin on `127.0.0.1`,
//! `localhost` or `[::1]`. The session is created (`POST /session {"title":"agora"}`) unless one is
//! given. `GET /event` (SSE) keeps `busy` for OUR session current: `session.status` (busy unless the
//! status is `idle`) and `session.idle`; the stream reconnects with backoff. A delivery waits until
//! `busy` is false, then `POST /session/:id/prompt_async`; a 2xx answers true. Basic auth comes from
//! the env var named by `password_env` (user from `OPENCODE_SERVER_USERNAME`, default `opencode`);
//! the header value is never printed. Session key = the opencode session id.

mod sse;

use crate::pump::{self, PumpConfig};
use crate::{BridgeError, Delivery, SessionReady};
use agora_client::ClientError;
use base64::Engine as _;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, redirect::Policy};
use serde_json::{Value, json};
use sse::track;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// Default server URL.
pub const DEFAULT_URL: &str = "http://127.0.0.1:4096";
/// Basic-auth user when `OPENCODE_SERVER_USERNAME` is unset (DOCS-ONLY).
const DEFAULT_USERNAME: &str = "opencode";
const USERNAME_ENV: &str = "OPENCODE_SERVER_USERNAME";
/// The refusal for any URL that is not plain http to a loopback host.
const NOT_LOOPBACK: &str = "opencode bridge only talks to a loopback server";
/// Bound on establishing a connection to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on one non-streaming request (session create, prompt).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest body accepted from the session-create answer, read before it is parsed.
const MAX_SESSION_BODY_BYTES: usize = 1024 * 1024;
/// How long a delivery waits for the session to go idle. The server queues a prompt sent while
/// busy, so after this bound the prompt goes out anyway rather than holding delivery forever.
const BUSY_MAX_WAIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct OpencodeOptions {
    /// Server base URL (`DEFAULT_URL`); must resolve to loopback.
    pub url: String,
    /// Use this session instead of creating one.
    pub session: Option<String>,
    /// Identity folder.
    pub cwd: PathBuf,
    /// Name of the env var that holds the server password (never the password itself).
    pub password_env: Option<String>,
    /// First prompt once the session is ready.
    pub prompt: Option<String>,
}

impl OpencodeOptions {
    /// Defaults: `url` = `DEFAULT_URL`, no session/password_env/prompt.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            session: None,
            cwd,
            password_env: None,
            prompt: None,
        }
    }
}

/// The `Authorization` header value for `password_env`: `Basic base64("<user>:<password>")`, the
/// password read from the env var named by `password_env` and the user from
/// `OPENCODE_SERVER_USERNAME` (default `opencode`). `None` when no `password_env` is set. `env` is
/// the environment lookup, injected so tests need no process environment. Errors name the variable,
/// never its value.
pub fn auth_from_env(
    password_env: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, BridgeError> {
    let Some(var) = password_env else {
        return Ok(None);
    };
    let password = env(var).filter(|p| !p.is_empty()).ok_or_else(|| {
        BridgeError::Client(ClientError::Usage(format!(
            "opencode password env var {var} is not set or empty"
        )))
    })?;
    let user = env(USERNAME_ENV)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_USERNAME.to_string());
    Ok(Some(basic_header(&user, &password)))
}

/// `Basic base64("<user>:<password>")`.
fn basic_header(user: &str, password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {encoded}")
}

/// A client for one loopback opencode server. Cloneable. The `Authorization` value is private and
/// `Debug` prints it redacted.
#[derive(Clone)]
pub struct OpencodeHttp {
    /// The validated origin, without a trailing slash.
    base: String,
    /// The `Authorization` value, marked sensitive, if the server is password-protected.
    auth: Option<HeaderValue>,
    http: Client,
}

impl std::fmt::Debug for OpencodeHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpencodeHttp")
            .field("base", &self.base)
            .field("auth", &self.auth.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl OpencodeHttp {
    /// Validates `url` (see `loopback_base`) and builds the client. No proxy (the traffic and the
    /// password must not leave the host), no redirects (a redirect would be followed with the
    /// credentials), and a bounded connect. `auth` is the full header value from `auth_from_env`.
    pub fn new(url: &str, auth: Option<String>) -> Result<Self, BridgeError> {
        let base = loopback_base(url)?;
        let auth = auth.map(sensitive_header).transpose()?;
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| BridgeError::Http(format!("client: {}", e.without_url())))?;
        Ok(Self { base, auth, http })
    }

    /// `POST /session {"title":"agora"}`; returns the new session id.
    pub async fn create_session(&self) -> Result<String, BridgeError> {
        let resp = self.post("/session", &json!({"title": "agora"})).await?;
        let body = read_json_capped("POST /session", resp).await?;
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::Protocol("POST /session response has no id".into()))?;
        check_session_id(id)?;
        Ok(id.to_owned())
    }

    /// `POST /session/:id/prompt_async {"parts":[{"type":"text","text":text}]}`. Any 2xx (opencode
    /// answers 204) is accepted; the turn itself is reported on `/event`.
    pub async fn prompt_async(&self, session_id: &str, text: &str) -> Result<(), BridgeError> {
        check_session_id(session_id)?;
        let path = format!("/session/{session_id}/prompt_async");
        let body = json!({"parts": [{"type": "text", "text": text}]});
        self.post(&path, &body).await?;
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `GET /session/:id`: `--session` must name a session the server has. Any non-2xx answer is
    /// refused with the same error, so the message names the session, not the status.
    pub async fn check_session(&self, session_id: &str) -> Result<(), BridgeError> {
        check_session_id(session_id)?;
        let req = self
            .http
            .get(self.url(&format!("/session/{session_id}")))
            .timeout(REQUEST_TIMEOUT);
        let resp = self
            .with_auth(req)
            .send()
            .await
            .map_err(|e| transport("GET /session/:id", e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(BridgeError::Http(format!(
                "opencode session {session_id} not found"
            )))
        }
    }

    fn with_auth(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.auth {
            Some(value) => req.header(AUTHORIZATION, value.clone()),
            None => req,
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Response, BridgeError> {
        let what = format!("POST {path}");
        let req = self
            .http
            .post(self.url(path))
            .json(body)
            .timeout(REQUEST_TIMEOUT);
        let resp = self
            .with_auth(req)
            .send()
            .await
            .map_err(|e| transport(&what, e))?;
        ensure_success(&what, resp)
    }

    /// `GET /event` as an event stream; a non-2xx answer is an error.
    async fn open_events(&self) -> Result<Response, BridgeError> {
        let req = self
            .http
            .get(self.url("/event"))
            .header(ACCEPT, "text/event-stream");
        let resp = self
            .with_auth(req)
            .send()
            .await
            .map_err(|e| transport("GET /event", e))?;
        ensure_success("GET /event", resp)
    }
}

/// The origin of `url` as `OpencodeHttp` keeps it. Only `http://` to `127.0.0.1`, `localhost` or
/// `[::1]` is accepted; `https` is refused because the bridge has no TLS. The URL must be a bare
/// origin: no credentials (a URL with them would be echoed in errors), path, query or fragment.
fn loopback_base(url: &str) -> Result<String, BridgeError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| BridgeError::Http("opencode URL is not a valid URL".into()))?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if parsed.scheme() != "http" || !loopback {
        return Err(BridgeError::Http(NOT_LOOPBACK.into()));
    }
    let bare = parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if !bare {
        return Err(BridgeError::Http(
            "opencode URL must be a bare origin such as http://127.0.0.1:4096".into(),
        ));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// A session id as it goes into a URL path: non-empty `[A-Za-z0-9_-]`.
fn check_session_id(id: &str) -> Result<(), BridgeError> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(BridgeError::Protocol(
            "opencode session id has characters outside [A-Za-z0-9_-]".into(),
        ))
    }
}

/// A transport failure, without the URL (reqwest errors print it).
fn transport(what: &str, e: reqwest::Error) -> BridgeError {
    BridgeError::Http(format!("{what}: {}", e.without_url()))
}

fn ensure_success(what: &str, resp: Response) -> Result<Response, BridgeError> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        Err(BridgeError::Http(format!("{what}: status {status}")))
    }
}

/// The `Authorization` value as a header marked sensitive, so that hyper's debug output and
/// tracing do not print it.
fn sensitive_header(value: String) -> Result<HeaderValue, BridgeError> {
    let mut header = HeaderValue::from_str(&value)
        .map_err(|_| BridgeError::Http("opencode password is not a valid header value".into()))?;
    header.set_sensitive(true);
    Ok(header)
}

/// The JSON body of `resp`, read up to `MAX_SESSION_BODY_BYTES` before it is parsed.
async fn read_json_capped(what: &str, mut resp: Response) -> Result<Value, BridgeError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| transport(what, e))? {
        if body.len() + chunk.len() > MAX_SESSION_BODY_BYTES {
            return Err(BridgeError::Protocol(format!("{what} body exceeds 1 MiB")));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|e| BridgeError::Protocol(format!("{what} body is not JSON: {e}")))
}

/// Validate the URL (loopback only), resolve/create the session, report its id through
/// `on_session`, post the optional first prompt, then run `drive` and `pump::run` (folder =
/// `opts.cwd`) until `shutdown` fires or either task ends. Returns Ok on a clean end.
pub async fn run(
    opts: OpencodeOptions,
    home: PathBuf,
    shutdown: CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    let auth = auth_from_env(opts.password_env.as_deref(), |name| {
        std::env::var(name).ok()
    })?;
    let client = OpencodeHttp::new(&opts.url, auth)?;
    let session_id = match &opts.session {
        Some(id) => {
            check_session_id(id)?;
            client.check_session(id).await?;
            id.clone()
        }
        None => client.create_session().await?,
    };
    if let Some(tx) = on_session {
        let _ = tx.send(session_id.clone());
    }
    if let Some(prompt) = &opts.prompt {
        client.prompt_async(&session_id, prompt).await?;
    }

    let stop = shutdown.child_token();
    let (delivery_tx, delivery_rx) = mpsc::channel(1);
    let mut driver = tokio::spawn(drive(
        client,
        session_id.clone(),
        delivery_rx,
        stop.clone(),
        BUSY_MAX_WAIT,
    ));
    let mut pump_task = tokio::spawn(pump::run(
        PumpConfig::new(home, opts.cwd.clone(), session_id)
            .with_harness(agora_deliver::Harness::OpenCode),
        delivery_tx,
        stop.clone(),
    ));
    // The pump ends on shutdown or failure; the driver also ends once the pump drops its sender.
    // Whichever ends first stops the other.
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

/// A task's result as the bridge's result; a panicked or cancelled task is a protocol error.
fn joined(outcome: Result<Result<(), BridgeError>, JoinError>) -> Result<(), BridgeError> {
    outcome.unwrap_or_else(|e| Err(BridgeError::Protocol(format!("bridge task failed: {e}"))))
}

/// Deliver prompts to the opencode session `session_id` over `client`: the busy state is tracked
/// from `GET /event` for that session only; each delivery waits until the session is idle (at most
/// `busy_max_wait`, then it is posted anyway), is posted with `prompt_async`, and is answered
/// `true` on a 2xx, `false` otherwise. Runs until `deliveries` closes or `shutdown` fires;
/// event-stream failures reconnect and never end it.
pub async fn drive(
    client: OpencodeHttp,
    session_id: String,
    mut deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
    busy_max_wait: Duration,
) -> Result<(), BridgeError> {
    check_session_id(&session_id)?;
    let busy_tx = Arc::new(watch::channel(false).0);
    let mut busy = busy_tx.subscribe();
    let stop = shutdown.child_token();
    let mut tracker = tokio::spawn(track(
        client.clone(),
        session_id.clone(),
        Arc::clone(&busy_tx),
        stop.clone(),
    ));
    // The tracker ends only on `stop`, or by panicking; a panic must not leave deliveries waiting.
    tokio::select! {
        outcome = deliver_loop(
            &client, &session_id, &mut deliveries, &mut busy, busy_max_wait, &busy_tx, &stop,
        ) => {
            stop.cancel();
            outcome.and(joined_unit(tracker.await))
        }
        done = &mut tracker => joined_unit(done),
    }
}

fn joined_unit(outcome: Result<(), JoinError>) -> Result<(), BridgeError> {
    outcome.map_err(|e| BridgeError::Protocol(format!("bridge task failed: {e}")))
}

async fn deliver_loop(
    client: &OpencodeHttp,
    session_id: &str,
    deliveries: &mut mpsc::Receiver<Delivery>,
    busy: &mut watch::Receiver<bool>,
    busy_max_wait: Duration,
    busy_tx: &watch::Sender<bool>,
    stop: &CancellationToken,
) -> Result<(), BridgeError> {
    loop {
        let delivery = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            next = deliveries.recv() => match next {
                Some(delivery) => delivery,
                None => return Ok(()),
            },
        };
        // Dropping `delivery` on shutdown counts as a refusal for the pump.
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            waited = tokio::time::timeout(busy_max_wait, busy.wait_for(|b| !*b)) => match waited {
                Ok(idle) => {
                    idle.map_err(|_| BridgeError::Protocol("opencode busy state lost".into()))?;
                }
                Err(_) => tracing::warn!(
                    wait = ?busy_max_wait,
                    "opencode stayed busy past the bound; sending the agora prompt anyway"
                ),
            }
        }
        // Mark busy before the POST, not after its answer: an idle event that arrives first (the
        // turn finished before the 2xx was read) then clears it, where a late mark would stick at
        // busy with no idle left to clear it. A refused POST clears it again.
        busy_tx.send_replace(true);
        let accepted = match client.prompt_async(session_id, &delivery.text).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "opencode refused the agora prompt");
                busy_tx.send_replace(false);
                false
            }
        };
        let _ = delivery.reply.send(accepted);
    }
}
