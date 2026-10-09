//! ACP bridge over `copilot --acp --stdio` (Agent Client Protocol, JSON-RPC 2.0 with the
//! `"jsonrpc"` field; spec agentclientprotocol.com; Copilot CLI details per SPEC 2.2).
//!
//! `initialize {protocolVersion:1, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},
//! terminal:false}, clientInfo}` -> `session/new {cwd, mcpServers:[{name:"agora", command:<exe>,
//! args:["--home",<home>,"mcp"], env:[]}]}` (or `session/load {sessionId, cwd, mcpServers}` when
//! `session` is set and `agentCapabilities.loadSession`) -> sessionId. ACP has no steer: a delivery
//! while a `session/prompt` is in flight waits (queue of 1; newer deliveries are merged: texts
//! concatenated, max_seq = max) until the prompt result arrives, then `session/prompt {sessionId,
//! prompt:[{type:"text",text}]}`; the batch's replies are answered from that request's outcome:
//! true when the turn ended (not `cancelled`), false on a JSON-RPC error. `session/request_permission`
//! -> the first option whose `kind` is `reject_once` (else `reject_always`, else
//! `{"outcome":{"outcome":"cancelled"}}`) unless `allow_tools` and the request is for an agora tool in
//! `AUTO_APPROVED_TOOLS` -> the first `allow_once`. `fs/*` and `terminal/*` requests -> error -32601.
//! `session/update` notifications are logged at debug.

use crate::jsonrpc::{Incoming, RpcConn, RpcError};
use crate::{BridgeError, Delivery};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

mod process;
pub use process::run;

/// ACP protocol version the bridge speaks (`initialize.protocolVersion`).
const PROTOCOL_VERSION: u64 = 1;
/// `initialize`, `session/new` and `session/load`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for one prompt turn. The agent answers `session/prompt` only when the turn ends, so
/// this is a backstop against a wedged agent, not an expected duration.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
const UNSUPPORTED: &str = "not supported by agora bridge";

#[derive(Debug, Clone)]
pub struct AcpOptions {
    /// The `copilot` binary (run as `<bin> --acp --stdio`).
    pub bin: PathBuf,
    /// Session root; also the identity folder.
    pub cwd: PathBuf,
    /// Load this ACP session instead of creating one.
    pub session: Option<String>,
    /// First prompt once the session is ready.
    pub prompt: Option<String>,
    /// Approve permission requests for the agora tools in `AUTO_APPROVED_TOOLS` (`--allow-tools`;
    /// default false: every `session/request_permission` is rejected). Without it the agent cannot
    /// call agora tools that need permission, such as `chat` to reply to a message. Every other
    /// request, such as a shell command or a file write, is rejected even when this is set.
    pub allow_tools: bool,
    /// The agora executable advertised as the session's MCP server command.
    pub exe: PathBuf,
    /// The agora home passed to that MCP server and used by the pump.
    pub home: PathBuf,
}

/// Protocol state machine (testable over `tokio::io::duplex`): see the module doc. The session id
/// goes out on `ready` once `session/new` / `session/load` succeeded. A delivery is answered once
/// its prompt's outcome is known: `true` for a turn that ended, `false` for an error, so a batch is
/// never acked before the agent took it. A closed connection ends the driver with Ok, and so does
/// `shutdown`, also while the handshake and session setup are still running.
pub async fn drive(
    conn: RpcConn,
    incoming: mpsc::Receiver<Incoming>,
    opts: &AcpOptions,
    ready: oneshot::Sender<String>,
    deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    match drive_session(conn, incoming, opts, ready, deliveries, shutdown).await {
        // The agent closed its side: the normal end of a session, not an error.
        Err(BridgeError::Rpc(RpcError::Closed)) => Ok(()),
        other => other,
    }
}

async fn drive_session(
    conn: RpcConn,
    mut incoming: mpsc::Receiver<Incoming>,
    opts: &AcpOptions,
    ready: oneshot::Sender<String>,
    mut deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    let session_id = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        id = async {
            let init = handshake(&conn).await?;
            open_session(&conn, opts, &init).await
        } => id?,
    };
    let _ = ready.send(session_id.clone());
    let mut session = Session {
        conn,
        session_id,
        prompt: None,
        queued: None,
    };
    if let Some(prompt) = &opts.prompt {
        session.launch(Batch::initial(prompt));
    }
    let mut deliveries_open = true;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                session.shutdown().await;
                return Ok(());
            }
            item = incoming.recv() => match item {
                None => return Ok(()),
                Some(Incoming::Notification { method, .. }) => {
                    tracing::debug!(%method, "copilot notification");
                }
                Some(Incoming::Request { id, method, params }) => {
                    answer(&session.conn, id, &method, &params, opts.allow_tools).await?;
                }
            },
            delivery = deliveries.recv(), if deliveries_open => match delivery {
                None => deliveries_open = false,
                Some(d) => session.submit(d),
            },
            _ = session.prompt_finished(), if session.prompt.is_some() => session.finish_prompt(),
        }
    }
}

/// `initialize`; the agent must answer the protocol version the bridge speaks.
async fn handshake(conn: &RpcConn) -> Result<Value, BridgeError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
        "clientInfo": {
            "name": "agora_bridge",
            "title": "Agora bridge",
            "version": env!("CARGO_PKG_VERSION"),
        },
    });
    let result = conn.request("initialize", params, SETUP_TIMEOUT).await?;
    match result.get("protocolVersion").and_then(Value::as_u64) {
        Some(PROTOCOL_VERSION) => Ok(result),
        other => Err(BridgeError::Protocol(format!(
            "agent answered ACP version {other:?}; the bridge speaks {PROTOCOL_VERSION}"
        ))),
    }
}

/// `session/load` when `opts.session` is set (only if the agent can load sessions), else
/// `session/new`. Returns the session id.
async fn open_session(
    conn: &RpcConn,
    opts: &AcpOptions,
    init: &Value,
) -> Result<String, BridgeError> {
    let cwd = utf8(&opts.cwd, "cwd")?;
    let mcp_servers = mcp_servers(opts)?;
    let Some(id) = &opts.session else {
        let params = json!({"cwd": cwd, "mcpServers": mcp_servers});
        let result = conn.request("session/new", params, SETUP_TIMEOUT).await?;
        return result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| BridgeError::Protocol("session/new response has no sessionId".into()));
    };
    if init
        .pointer("/agentCapabilities/loadSession")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(BridgeError::Protocol("agent cannot load sessions".into()));
    }
    let params = json!({"sessionId": id, "cwd": cwd, "mcpServers": mcp_servers});
    // The agent replays the history as `session/update` notifications, then answers with null.
    conn.request("session/load", params, SETUP_TIMEOUT).await?;
    Ok(id.clone())
}

/// The agora MCP server every session gets: `<exe> --home <home> mcp` over stdio.
fn mcp_servers(opts: &AcpOptions) -> Result<Value, BridgeError> {
    Ok(json!([{
        "name": "agora",
        "command": utf8(&opts.exe, "agora executable")?,
        "args": ["--home", utf8(&opts.home, "agora home")?, "mcp"],
        "env": [],
    }]))
}

/// A path as the UTF-8 string the protocol carries.
fn utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str, BridgeError> {
    path.to_str()
        .ok_or_else(|| BridgeError::Protocol(format!("{what} is not UTF-8: {}", path.display())))
}

/// Deliveries waiting for one `session/prompt`: texts joined by a blank line, the highest `seq`,
/// the message count, and every reply sender, which are all answered together.
struct Batch {
    text: String,
    max_seq: i64,
    count: usize,
    replies: Vec<oneshot::Sender<bool>>,
}

impl Batch {
    fn from_delivery(d: Delivery) -> Self {
        Self {
            text: d.text,
            max_seq: d.max_seq,
            count: d.count,
            replies: vec![d.reply],
        }
    }

    /// The optional first prompt from the options: no delivery, so nothing to answer.
    fn initial(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            max_seq: 0,
            count: 0,
            replies: Vec::new(),
        }
    }

    fn merge(&mut self, d: Delivery) {
        self.text.push_str("\n\n");
        self.text.push_str(&d.text);
        self.max_seq = self.max_seq.max(d.max_seq);
        self.count += d.count;
        self.replies.push(d.reply);
    }
}

/// The `session/prompt` in flight: its task, and the channel on which the task reports the end.
struct Prompt {
    done: mpsc::Receiver<()>,
    task: JoinHandle<()>,
}

/// The live session: the connection, the session id, the prompt in flight (at most one), and the
/// deliveries merged while it runs.
struct Session {
    conn: RpcConn,
    session_id: String,
    prompt: Option<Prompt>,
    queued: Option<Batch>,
}

impl Session {
    /// A delivery starts a prompt when none is in flight; otherwise it joins the queued batch.
    fn submit(&mut self, d: Delivery) {
        if self.prompt.is_none() {
            self.launch(Batch::from_delivery(d));
        } else if let Some(batch) = self.queued.as_mut() {
            batch.merge(d);
        } else {
            self.queued = Some(Batch::from_delivery(d));
        }
    }

    /// Start `session/prompt` for `batch` in its own task, so the driver stays responsive.
    fn launch(&mut self, batch: Batch) {
        tracing::debug!(
            count = batch.count,
            max_seq = batch.max_seq,
            "session/prompt"
        );
        let params = json!({
            "sessionId": self.session_id,
            "prompt": text_blocks(&batch.text),
        });
        let (done_tx, done) = mpsc::channel(1);
        let task = tokio::spawn(run_prompt(
            self.conn.clone(),
            params,
            batch.replies,
            done_tx,
        ));
        self.prompt = Some(Prompt { done, task });
    }

    /// Resolves when the prompt in flight ends (its task reports, or is gone).
    async fn prompt_finished(&mut self) {
        if let Some(prompt) = self.prompt.as_mut() {
            let _ = prompt.done.recv().await;
        }
    }

    /// The prompt ended: send the queued batch, if any, as the next prompt.
    fn finish_prompt(&mut self) {
        self.prompt = None;
        if let Some(batch) = self.queued.take() {
            self.launch(batch);
        }
    }

    /// Cancel the turn in flight. Queued deliveries are dropped: their senders count as `false`,
    /// so the pump keeps them unread.
    async fn shutdown(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let params = json!({"sessionId": self.session_id});
        if let Err(e) = self.conn.notify("session/cancel", params).await {
            tracing::warn!(error = %e, "session/cancel failed");
        }
        prompt.task.abort();
    }
}

/// One `session/prompt`. Its replies are answered from its outcome: `true` when the turn ended,
/// `false` when the request failed, and `false` for a turn that ended `cancelled`, which the agent
/// may have cut short. The answer waits for the outcome, however long the turn runs: a batch acked
/// before a later error would never be redelivered, because the cursor has moved past it. Here the
/// batch stays unread and the pump retries it. `done` reports the end to the driver.
async fn run_prompt(
    conn: RpcConn,
    params: Value,
    replies: Vec<oneshot::Sender<bool>>,
    done: mpsc::Sender<()>,
) {
    let accepted = match conn.request("session/prompt", params, PROMPT_TIMEOUT).await {
        Ok(result) => match result.get("stopReason").and_then(Value::as_str) {
            Some("cancelled") => {
                tracing::warn!("session/prompt ended cancelled; the batch stays unread");
                false
            }
            stop => {
                tracing::debug!(?stop, "prompt turn ended");
                true
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "session/prompt failed; the batch stays unread");
            false
        }
    };
    answer_all(replies, accepted);
    let _ = done.send(()).await;
}

fn answer_all(replies: Vec<oneshot::Sender<bool>>, accepted: bool) {
    for reply in replies {
        let _ = reply.send(accepted);
    }
}

/// The `prompt` content of a text-only `session/prompt`.
fn text_blocks(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

/// Answer one agent-to-client request: permissions per `permission_outcome`; everything else
/// (`fs/*`, `terminal/*`, unknown methods) is refused with "method not found".
async fn answer(
    conn: &RpcConn,
    id: Value,
    method: &str,
    params: &Value,
    allow_tools: bool,
) -> Result<(), RpcError> {
    if method == "session/request_permission" {
        return conn
            .respond(id, permission_outcome(params, allow_tools))
            .await;
    }
    tracing::debug!(%method, "refusing agent request");
    conn.respond_error(id, METHOD_NOT_FOUND, UNSUPPORTED).await
}

/// Agora MCP tools that `--allow-tools` approves. `chat`, `react` and `ack` write (they are the
/// reply path), so this is not a read-only list. Nothing else is approved: no shell, file or
/// terminal tool, no other MCP server, and no agora tool that changes roles, work, channels or cards.
const AUTO_APPROVED_TOOLS: &[&str] = &[
    "chat",
    "inbox",
    "inbox_wait",
    "ack",
    "whoami",
    "read",
    "react",
    "note_search",
    "board_summary",
    "card_list",
];

/// Whether the request is for one of `AUTO_APPROVED_TOOLS`. The agent names the tool in
/// `toolCall.title`, as Copilot's `agora-<tool>` (the `agora` MCP server) or as `mcp__agora__<tool>`.
/// The name must match exactly: a title that merely mentions an agora tool does not qualify.
fn is_auto_approved(params: &Value) -> bool {
    let Some(title) = params.pointer("/toolCall/title").and_then(Value::as_str) else {
        return false;
    };
    title
        .strip_prefix("agora-")
        .or_else(|| title.strip_prefix("mcp__agora__"))
        .is_some_and(|tool| AUTO_APPROVED_TOOLS.contains(&tool))
}

/// The `session/request_permission` answer. Rejecting is the default: the first `reject_once`,
/// else the first `reject_always`. With `allow_tools` and a request for an `AUTO_APPROVED_TOOLS`
/// agora tool, the first `allow_once` is chosen; when the agent offers none, the rejection rule
/// applies. No usable option: cancelled.
fn permission_outcome(params: &Value, allow_tools: bool) -> Value {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let by_kind = |kind: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|o| o.get("optionId"))
            .and_then(Value::as_str)
    };
    let chosen = if allow_tools && is_auto_approved(params) {
        by_kind("allow_once")
    } else {
        None
    }
    .or_else(|| by_kind("reject_once"))
    .or_else(|| by_kind("reject_always"));
    match chosen {
        Some(option) => json!({"outcome": {"outcome": "selected", "optionId": option}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}
