//! Codex bridge over `codex app-server` (stdio JSONL, NO "jsonrpc" field). Protocol facts are
//! VERIFIED-LOCAL against Codex 0.157.1 (SPEC 2.1; fixtures in /tmp/agora-m6/fixtures/codex).

use crate::PREAMBLE;
use crate::jsonrpc::{Incoming, RpcConn, RpcError};
use crate::pump::{self, PumpConfig};
use crate::{BridgeError, Delivery, SessionReady};
use serde_json::{Map, Value, json};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

mod stdin;
use stdin::StdinHandle;

/// Handshake and `thread/start` or `thread/resume`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default for `CodexOptions::turn_timeout`: `turn/start` and `turn/steer`.
const TURN_TIMEOUT: Duration = Duration::from_secs(30);
/// `turn/interrupt` on shutdown.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);
/// After stdin is closed, how long the app-server may take to exit on its own before it is killed.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long the child may take to exit once it was killed.
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
const UNSUPPORTED: &str = "not supported by agora bridge";
/// Reason sent with the legacy `denied` decision.
const DENIED_REASON: &str = "declined by the agora bridge";
/// The agora MCP tools the bridge pre-approves: chat and board reads, and the chat writes (`chat`,
/// `react`, `ack`) that post or acknowledge messages. None runs a command or changes work, roles,
/// channels, cards or pools. Every other agora tool stays on `AGORA_DEFAULT_APPROVAL`, and the bridge
/// declines those prompts, so the model cannot call them unattended.
const SAFE_AGORA_TOOLS: &[&str] = &[
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
/// `default_tools_approval_mode` of the agora server: every tool not in `SAFE_AGORA_TOOLS`.
const AGORA_DEFAULT_APPROVAL: &str = "prompt";

#[derive(Debug, Clone)]
pub struct CodexOptions {
    /// The `codex` binary.
    pub bin: PathBuf,
    /// Session root; also the identity folder.
    pub cwd: PathBuf,
    /// Resume this thread (`thread/resume`) instead of `thread/start`.
    pub thread: Option<String>,
    pub model: Option<String>,
    /// `read-only` | `workspace-write` | `danger-full-access` (default `workspace-write`).
    pub sandbox: String,
    /// `never` | `on-request` | `untrusted` (default `never`).
    pub approval: String,
    /// First prompt (`turn/start`) once the thread is ready.
    pub prompt: Option<String>,
    /// Pre-approve the agora tools approved without prompting (including `chat`, `react`, `ack`,
    /// which write) in `SAFE_AGORA_TOOLS` (default true). Every other agora tool, `work_run`
    /// included, stays on prompt, which the bridge declines. Off: no agora tool is pre-approved, so
    /// under `approval = never` they all fail.
    pub approve_agora_tools: bool,
    /// Bound on each `turn/start` and `turn/steer` (default 30 s). A timed-out delivery is refused.
    pub turn_timeout: Duration,
}

impl CodexOptions {
    /// Defaults: no thread/model/prompt, sandbox `workspace-write`, approval `never`,
    /// `approve_agora_tools` true, `turn_timeout` 30 s.
    pub fn new(bin: PathBuf, cwd: PathBuf) -> Self {
        Self {
            bin,
            cwd,
            thread: None,
            model: None,
            sandbox: "workspace-write".into(),
            approval: "never".into(),
            prompt: None,
            approve_agora_tools: true,
            turn_timeout: TURN_TIMEOUT,
        }
    }
}

/// Spawn `<bin> app-server` (stdin/stdout piped, stderr inherited, kill_on_drop), handshake,
/// start/resume the thread, report the thread id (= hook `session_id` = `session_state` key) through
/// `on_session`, and run `drive` plus `pump::run` (folder = `opts.cwd`) until the child exits or
/// `shutdown` fires. Returns Ok on a clean end.
pub async fn run(
    opts: CodexOptions,
    home: PathBuf,
    shutdown: CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    let mut child = Command::new(&opts.bin)
        .arg("app-server")
        .current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Its own process group, so a kill also reaches what the app-server started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BridgeError::Spawn(format!("{}: {e}", opts.bin.display())))?;
    // `process_group(0)` makes the child's pid its group id.
    let pgid = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
    let stdin = StdinHandle::new(child.stdin.take());
    let result = serve(
        &mut child,
        stdin.clone(),
        &opts,
        home,
        &shutdown,
        on_session,
    )
    .await;
    reap(&mut child, pgid, &stdin).await;
    result
}

/// Everything `run` does once the child is up: the driver and the pump, until one of them ends.
async fn serve(
    child: &mut Child,
    stdin: StdinHandle,
    opts: &CodexOptions,
    home: PathBuf,
    shutdown: &CancellationToken,
    on_session: Option<SessionReady>,
) -> Result<(), BridgeError> {
    let Some(stdout) = child.stdout.take() else {
        return Err(BridgeError::Protocol(
            "app-server stdio is not piped".into(),
        ));
    };
    let (conn, incoming) = RpcConn::spawn(stdout, stdin, false);
    let stop = shutdown.child_token();
    let (ready_tx, mut ready_rx) = oneshot::channel();
    let (delivery_tx, delivery_rx) = mpsc::channel(1);
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
    let thread_id = tokio::select! {
        ready = &mut ready_rx => ready.ok(),
        _ = shutdown.cancelled() => None,
    };
    let Some(thread_id) = thread_id else {
        // The driver dropped `ready` (it returned, or it was told to stop during setup).
        stop.cancel();
        return match joined(driver.await) {
            Ok(()) if shutdown.is_cancelled() => Ok(()),
            Ok(()) => Err(BridgeError::Protocol(
                "codex app-server ended before the thread was ready".into(),
            )),
            Err(e) => Err(e),
        };
    };
    let mut pump_task = tokio::spawn(pump::run(
        PumpConfig::new(home, opts.cwd.clone(), thread_id.clone())
            .with_harness(agora_deliver::Harness::Codex),
        delivery_tx,
        stop.clone(),
    ));
    if let Some(tx) = on_session {
        let _ = tx.send(thread_id);
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

/// Close the child's stdin, give it `CHILD_EXIT_GRACE` to exit, then kill its process group; the
/// wait after the kill is bounded by `CHILD_EXIT_TIMEOUT`.
async fn reap(child: &mut Child, pgid: Option<libc::pid_t>, stdin: &StdinHandle) {
    stdin.close();
    if tokio::time::timeout(CHILD_EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        kill_group(child, pgid);
    }
    match tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => tracing::debug!(%status, "codex app-server exited"),
        Ok(Err(e)) => tracing::warn!(error = %e, "waiting for codex app-server failed"),
        Err(_) => tracing::warn!("codex app-server did not exit after kill"),
    }
}

/// Kills the process group `pgid`: the app-server and every process it started in that group. Without
/// a pid, only the child itself is killed.
fn kill_group(child: &mut Child, pgid: Option<libc::pid_t>) {
    let Some(pgid) = pgid else {
        let _ = child.start_kill();
        return;
    };
    // SAFETY: `killpg` only sends a signal and touches no memory. `pgid` is the pid of a child this
    // bridge spawned as its own group leader, so it names that group.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let err = io::Error::last_os_error();
        // ESRCH: the group is already gone, which is the state a kill wants.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(error = %err, "could not kill codex app-server's process group");
        }
    }
}

/// A task's result as the bridge's result; a panicked or cancelled task is a protocol error.
fn joined<T>(outcome: Result<Result<T, BridgeError>, JoinError>) -> Result<T, BridgeError> {
    match outcome {
        Ok(result) => result,
        Err(e) => Err(BridgeError::Protocol(format!("bridge task failed: {e}"))),
    }
}

/// Protocol state machine, testable without a process: handshake (`initialize
/// {clientInfo:{name:"agora_bridge",title:"Agora bridge",version:<crate version>}}` then
/// `initialized`), `thread/start` or `thread/resume` (same params: cwd, approvalPolicy, sandbox,
/// model?, developerInstructions, config = agora tool approval; resume adds threadId); setup races
/// `shutdown`; the thread id goes out on `ready`; optional initial
/// `turn/start` with `prompt`; then select! over: incoming (turn/started -> active = Some(id);
/// turn/completed of the active turn -> None; thread/status/changed idle -> None; server requests:
/// commandExecution/fileChange approval -> `{"decision":"decline"}`, legacy approval ->
/// `{"decision":{"denied":{"rejection":..}}}`, mcpServer/elicitation/request -> `{"action":"decline"}`,
/// other -> error -32601 "not supported by agora bridge"), deliveries (active Some(t) ->
/// `turn/steer {threadId, expectedTurnId:t, input:[{type:"text",text}]}`, on a Remote error ->
/// `turn/start`; None -> `turn/start`; success -> reply true, failure -> reply false), shutdown ->
/// `turn/interrupt` if active, return. A closed connection ends the driver with Ok.
pub async fn drive(
    conn: RpcConn,
    incoming: mpsc::Receiver<Incoming>,
    opts: &CodexOptions,
    ready: oneshot::Sender<String>,
    deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    match drive_session(conn, incoming, opts, ready, deliveries, shutdown).await {
        // The app-server closed its side: the normal end of a session, not an error.
        Err(BridgeError::Rpc(RpcError::Closed)) => Ok(()),
        other => other,
    }
}

async fn drive_session(
    conn: RpcConn,
    mut incoming: mpsc::Receiver<Incoming>,
    opts: &CodexOptions,
    ready: oneshot::Sender<String>,
    mut deliveries: mpsc::Receiver<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    // Setup is not bounded by anything but `SETUP_TIMEOUT`, so a shutdown must be able to cut it
    // short; a clean stop before the thread exists is an Ok end with no `ready`.
    let thread = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        opened = open_session(&conn, opts) => opened?,
    };
    let _ = ready.send(thread.clone());
    let mut session = Session {
        conn,
        thread,
        active: None,
        turn_timeout: opts.turn_timeout,
    };
    if let Some(prompt) = &opts.prompt {
        session.start_turn(prompt).await?;
    }
    let mut deliveries_open = true;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                session.interrupt().await;
                return Ok(());
            }
            item = incoming.recv() => match item {
                None => return Ok(()),
                Some(Incoming::Notification { method, params }) => session.observe(&method, &params),
                Some(Incoming::Request { id, method, .. }) => answer(&session.conn, id, &method).await?,
            },
            delivery = deliveries.recv(), if deliveries_open => match delivery {
                None => deliveries_open = false,
                Some(d) => {
                    let accepted = match session.deliver(&d.text).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(error = %e, "codex delivery failed");
                            false
                        }
                    };
                    let _ = d.reply.send(accepted);
                }
            },
        }
    }
}

/// `initialize`, then the `initialized` notification (no params).
async fn handshake(conn: &RpcConn) -> Result<(), RpcError> {
    let params = json!({
        "clientInfo": {
            "name": "agora_bridge",
            "title": "Agora bridge",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    conn.request("initialize", params, SETUP_TIMEOUT).await?;
    conn.notify("initialized", Value::Null).await
}

/// Handshake, then `open_thread`; returns the thread id.
async fn open_session(conn: &RpcConn, opts: &CodexOptions) -> Result<String, BridgeError> {
    handshake(conn).await?;
    open_thread(conn, opts).await
}

/// `thread/resume` for `opts.thread`, else `thread/start`; returns the thread id.
async fn open_thread(conn: &RpcConn, opts: &CodexOptions) -> Result<String, BridgeError> {
    let (method, params) = match &opts.thread {
        Some(id) => ("thread/resume", thread_params(opts, Some(id.as_str()))?),
        None => ("thread/start", thread_params(opts, None)?),
    };
    let result = conn
        .request(
            method,
            with_agora_tool_approval(params, opts),
            SETUP_TIMEOUT,
        )
        .await?;
    result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| BridgeError::Protocol(format!("{method} response has no thread.id")))
}

/// Params shared by `thread/start` and `thread/resume`: cwd, approvalPolicy, sandbox, `model` when
/// set, and `developerInstructions` = the preamble trimmed (the untrusted-inbound rule, which the
/// owner's instructions override). Resuming adds `threadId`.
fn thread_params(opts: &CodexOptions, resume: Option<&str>) -> Result<Value, BridgeError> {
    let cwd = opts.cwd.to_str().ok_or_else(|| {
        BridgeError::Protocol(format!("cwd is not UTF-8: {}", opts.cwd.display()))
    })?;
    let mut params = json!({
        "cwd": cwd,
        "approvalPolicy": opts.approval,
        "sandbox": opts.sandbox,
        "developerInstructions": PREAMBLE.trim_end(),
    });
    if let Some(model) = &opts.model {
        params["model"] = json!(model);
    }
    if let Some(id) = resume {
        params["threadId"] = json!(id);
    }
    Ok(params)
}

/// Adds the agora MCP approval to `params` when `approve_agora_tools` is set: each tool in
/// `SAFE_AGORA_TOOLS` gets `approval_mode = "approve"`, and the server default is
/// `AGORA_DEFAULT_APPROVAL`, so every other agora tool still prompts. The schema types `config` as a
/// free-form object. Nested form, verified on 0.157.1: the app-server loader parses it to the same
/// `mcp_servers.agora.*` keys as the dotted form (a bad value is reported at
/// `mcp_servers.<name>.<key>`), and the CLI override layer merges it with the user's other servers.
/// Per-tool keys are `tools.<name>.approval_mode` (`McpServerToolConfig`); the modes are `prompt`,
/// `writes` and `approve`.
fn with_agora_tool_approval(mut params: Value, opts: &CodexOptions) -> Value {
    if opts.approve_agora_tools {
        let tools: Map<String, Value> = SAFE_AGORA_TOOLS
            .iter()
            .map(|name| ((*name).to_owned(), json!({"approval_mode": "approve"})))
            .collect();
        params["config"] = json!({"mcp_servers": {"agora": {
            "default_tools_approval_mode": AGORA_DEFAULT_APPROVAL,
            "tools": tools,
        }}});
    }
    params
}

/// The live thread: the connection, the thread id, and the turn running now, if any.
struct Session {
    conn: RpcConn,
    thread: String,
    active: Option<String>,
    turn_timeout: Duration,
}

impl Session {
    /// `turn/start`; the server answers with the new turn, which becomes the active one.
    async fn start_turn(&mut self, text: &str) -> Result<(), RpcError> {
        let params = json!({"threadId": self.thread, "input": text_input(text)});
        let result = self
            .conn
            .request("turn/start", params, self.turn_timeout)
            .await?;
        self.active = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(())
    }

    /// Deliver one prompt: steer the running turn when there is one, else start a turn. A steer the
    /// server refuses (the turn ended meanwhile) falls back to `turn/start`; any other failure is
    /// final.
    async fn deliver(&mut self, text: &str) -> Result<(), RpcError> {
        if let Some(turn) = self.active.clone() {
            let params = json!({
                "threadId": self.thread,
                "expectedTurnId": turn,
                "input": text_input(text),
            });
            match self
                .conn
                .request("turn/steer", params, self.turn_timeout)
                .await
            {
                Ok(_) => return Ok(()),
                Err(RpcError::Remote { .. }) => self.active = None,
                Err(e) => return Err(e),
            }
        }
        self.start_turn(text).await
    }

    /// Track the running turn from the notifications of our thread. `turn/completed` clears only
    /// the turn that is active, so a late completion of an earlier turn cannot hide a newer one.
    fn observe(&mut self, method: &str, params: &Value) {
        if params.get("threadId").and_then(Value::as_str) != Some(self.thread.as_str()) {
            return;
        }
        let turn_id = params.pointer("/turn/id").and_then(Value::as_str);
        match method {
            "turn/started" => {
                if let Some(id) = turn_id {
                    self.active = Some(id.to_owned());
                }
            }
            "turn/completed" => {
                if turn_id.is_some() && self.active.as_deref() == turn_id {
                    self.active = None;
                }
            }
            "thread/status/changed"
                if params.pointer("/status/type").and_then(Value::as_str) == Some("idle") =>
            {
                self.active = None;
            }
            _ => {}
        }
    }

    /// `turn/interrupt` for the running turn, bounded; a failure is logged, not fatal.
    async fn interrupt(&mut self) {
        let Some(turn) = self.active.take() else {
            return;
        };
        let params = json!({"threadId": self.thread, "turnId": turn});
        if let Err(e) = self
            .conn
            .request("turn/interrupt", params, INTERRUPT_TIMEOUT)
            .await
        {
            tracing::warn!(error = %e, "turn/interrupt failed");
        }
    }
}

fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

/// Answer one server request. Approvals are declined. A permission request gets an empty grant
/// (`GrantedPermissionProfile` has only optional fields, so `{}` adds no file or network access),
/// and a user-input request gets no answers (`answers` is a map, so `{}` is valid). Anything else
/// is refused with "method not found".
async fn answer(conn: &RpcConn, id: Value, method: &str) -> Result<(), RpcError> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            conn.respond(id, json!({"decision": "decline"})).await
        }
        "item/permissions/requestApproval" => conn.respond(id, json!({"permissions": {}})).await,
        "item/tool/requestUserInput" => conn.respond(id, json!({"answers": {}})).await,
        // Legacy approval requests. Their plain `denied` value is an object with a rejection
        // reason; `denied` ends this command and lets the turn go on.
        "execCommandApproval" | "applyPatchApproval" => {
            conn.respond(
                id,
                json!({"decision": {"denied": {"rejection": DENIED_REASON}}}),
            )
            .await
        }
        "mcpServer/elicitation/request" => conn.respond(id, json!({"action": "decline"})).await,
        _ => conn.respond_error(id, METHOD_NOT_FOUND, UNSUPPORTED).await,
    }
}

#[cfg(test)]
mod tests;
