#![cfg(feature = "codex")]
//! The Codex driver against a scripted app-server: the test plays the server over a duplex pipe
//! (JSON lines, no "jsonrpc" field) and checks every message the driver writes.

use agora_bridge::codex::{CodexOptions, drive, run};
use agora_bridge::jsonrpc::RpcConn;
use agora_bridge::{BridgeError, Delivery, PREAMBLE};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const T: Duration = Duration::from_secs(5);
const THREAD: &str = "thr-1";

/// The scripted server: reads what the driver writes, writes what the driver reads.
struct Server {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    out: WriteHalf<DuplexStream>,
}

impl Server {
    /// The next message from the driver. Panics on silence, a closed pipe, or a `jsonrpc` field.
    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(T, self.lines.next_line())
            .await
            .expect("driver sent nothing")
            .expect("read failed")
            .expect("driver closed the pipe");
        let msg: Value = serde_json::from_str(&line).expect("driver wrote invalid JSON");
        assert!(
            msg.get("jsonrpc").is_none(),
            "no jsonrpc field expected: {msg}"
        );
        msg
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.out
            .write_all(line.as_bytes())
            .await
            .expect("write failed");
        self.out.flush().await.expect("flush failed");
    }

    /// The next message, which must be a request for `method`; returns its id and params.
    async fn expect_request(&mut self, method: &str) -> (Value, Value) {
        let msg = self.recv().await;
        assert_eq!(msg["method"], method, "unexpected message: {msg}");
        assert!(msg.get("id").is_some(), "{method} must be a request: {msg}");
        (msg["id"].clone(), msg["params"].clone())
    }

    async fn reply(&mut self, id: &Value, result: Value) {
        self.send(json!({"id": id, "result": result})).await;
    }

    async fn fail(&mut self, id: &Value, code: i64, message: &str) {
        self.send(json!({"id": id, "error": {"code": code, "message": message}}))
            .await;
    }
}

struct Driver {
    server: Server,
    deliveries: mpsc::Sender<Delivery>,
    shutdown: CancellationToken,
    ready: oneshot::Receiver<String>,
    task: JoinHandle<Result<(), BridgeError>>,
}

fn options() -> CodexOptions {
    CodexOptions::new(
        PathBuf::from("/unused/codex"),
        PathBuf::from("/tmp/agora-project"),
    )
}

/// Start `drive` on one end of a duplex pipe; the test holds the other end.
fn start(opts: CodexOptions) -> Driver {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (client_read, client_write) = split(client_io);
    let (server_read, server_write) = split(server_io);
    let (conn, incoming) = RpcConn::spawn(client_read, client_write, false);
    let (deliveries, delivery_rx) = mpsc::channel(1);
    let (ready_tx, ready) = oneshot::channel();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { drive(conn, incoming, &opts, ready_tx, delivery_rx, shutdown).await }
    });
    Driver {
        server: Server {
            lines: BufReader::new(server_read).lines(),
            out: server_write,
        },
        deliveries,
        shutdown,
        ready,
        task,
    }
}

/// Answers `initialize` and checks the `initialized` notification carries no params.
async fn handshake(server: &mut Server) {
    let (id, params) = server.expect_request("initialize").await;
    assert_eq!(params["clientInfo"]["name"], "agora_bridge");
    assert_eq!(params["clientInfo"]["title"], "Agora bridge");
    assert_eq!(params["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
    server.reply(&id, json!({"userAgent": "fake"})).await;
    let note = server.recv().await;
    assert_eq!(note["method"], "initialized");
    assert!(
        note.get("id").is_none() && note.get("params").is_none(),
        "{note}"
    );
}

/// Handshake, then answer the thread request (`thread/start` or `thread/resume`) with THREAD.
/// Returns the request params and the id the driver reported on `ready`.
async fn open(d: &mut Driver, method: &str) -> (Value, String) {
    handshake(&mut d.server).await;
    let (id, params) = d.server.expect_request(method).await;
    d.server.reply(&id, json!({"thread": {"id": THREAD}})).await;
    let ready = tokio::time::timeout(T, &mut d.ready)
        .await
        .expect("ready not sent")
        .expect("ready dropped");
    (params, ready)
}

/// Queue one delivery; the returned receiver yields the driver's accept or refuse.
async fn deliver(d: &Driver, text: &str) -> oneshot::Receiver<bool> {
    let (reply, answer) = oneshot::channel();
    let delivery = Delivery {
        text: text.to_owned(),
        max_seq: 1,
        count: 1,
        reply,
    };
    d.deliveries
        .send(delivery)
        .await
        .expect("driver stopped taking deliveries");
    answer
}

async fn accepted(answer: oneshot::Receiver<bool>) -> bool {
    tokio::time::timeout(T, answer)
        .await
        .expect("no reply to the delivery")
        .expect("reply dropped")
}

fn notification(method: &str, params: Value) -> Value {
    json!({"method": method, "params": params})
}

fn turn_started(turn: &str) -> Value {
    notification(
        "turn/started",
        json!({"threadId": THREAD, "turn": {"id": turn, "status": "inProgress"}}),
    )
}

fn turn_completed(turn: &str) -> Value {
    notification(
        "turn/completed",
        json!({"threadId": THREAD, "turn": {"id": turn, "status": "completed"}}),
    )
}

fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

/// Sends a request the driver does not handle and waits for its refusal. The driver handles
/// incoming messages in order, so every message the test sent before this one has been handled
/// when the refusal arrives.
async fn barrier(server: &mut Server) {
    let id = json!("barrier");
    server
        .send(json!({"id": id, "method": "test/barrier"}))
        .await;
    let reply = server.recv().await;
    assert_eq!(reply["id"], id);
    assert_eq!(reply["error"]["code"], -32601, "{reply}");
}

/// Send a server request and return the driver's answer to it.
async fn ask(server: &mut Server, id: i64, method: &str, params: Value) -> Value {
    server
        .send(json!({"id": id, "method": method, "params": params}))
        .await;
    let reply = server.recv().await;
    assert_eq!(reply["id"], id, "{reply}");
    reply
}

#[tokio::test]
async fn handshake_and_thread_start_params() {
    let mut d = start(options());
    handshake(&mut d.server).await;
    let (id, params) = d.server.expect_request("thread/start").await;
    assert_eq!(params["cwd"], "/tmp/agora-project");
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandbox"], "workspace-write");
    assert!(
        params.get("model").is_none(),
        "model is omitted when unset: {params}"
    );
    assert_agora_approval(&params);
    assert_eq!(params["developerInstructions"], PREAMBLE.trim_end());
    d.server.reply(&id, json!({"thread": {"id": THREAD}})).await;
    assert_eq!(ready_id(&mut d).await, THREAD);
}

/// The thread id the driver reported on `ready`.
async fn ready_id(d: &mut Driver) -> String {
    tokio::time::timeout(T, &mut d.ready)
        .await
        .expect("ready not sent")
        .expect("ready dropped")
}

#[tokio::test]
async fn thread_start_sends_model_and_omits_config_when_not_asked() {
    let mut opts = options();
    opts.model = Some("gpt-test".into());
    opts.approve_agora_tools = false;
    let mut d = start(opts);
    handshake(&mut d.server).await;
    let (id, params) = d.server.expect_request("thread/start").await;
    assert_eq!(params["model"], "gpt-test");
    assert!(params.get("config").is_none(), "{params}");
    d.server.reply(&id, json!({"thread": {"id": THREAD}})).await;
    assert_eq!(ready_id(&mut d).await, THREAD);
}

#[tokio::test]
async fn thread_resume_is_used_when_a_thread_is_given() {
    let mut opts = options();
    opts.thread = Some("thr-old".into());
    let mut d = start(opts);
    handshake(&mut d.server).await;
    let (id, params) = d.server.expect_request("thread/resume").await;
    assert_eq!(params["threadId"], "thr-old");
    assert_eq!(params["cwd"], "/tmp/agora-project");
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandbox"], "workspace-write");
    assert_eq!(params["developerInstructions"], PREAMBLE.trim_end());
    assert_agora_approval(&params);
    d.server
        .reply(&id, json!({"thread": {"id": "thr-old"}}))
        .await;
    assert_eq!(ready_id(&mut d).await, "thr-old");
}

#[tokio::test]
async fn delivery_while_idle_starts_a_turn() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    let answer = deliver(&d, "hello").await;
    let (id, params) = d.server.expect_request("turn/start").await;
    assert_eq!(params["threadId"], THREAD);
    assert_eq!(params["input"], text_input("hello"));
    d.server
        .reply(
            &id,
            json!({"turn": {"id": "turn-1", "status": "inProgress"}}),
        )
        .await;
    assert!(accepted(answer).await);
}

#[tokio::test]
async fn delivery_during_a_turn_steers_it() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    barrier(&mut d.server).await;
    let answer = deliver(&d, "more").await;
    let (id, params) = d.server.expect_request("turn/steer").await;
    assert_eq!(params["threadId"], THREAD);
    assert_eq!(params["expectedTurnId"], "turn-1");
    assert_eq!(params["input"], text_input("more"));
    d.server.reply(&id, json!({"turnId": "turn-1"})).await;
    assert!(accepted(answer).await);
}

#[tokio::test]
async fn refused_steer_falls_back_to_turn_start() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    barrier(&mut d.server).await;
    let answer = deliver(&d, "late").await;
    let (id, _) = d.server.expect_request("turn/steer").await;
    d.server.fail(&id, -32600, "no active turn to steer").await;
    let (id, params) = d.server.expect_request("turn/start").await;
    assert_eq!(params["input"], text_input("late"));
    d.server
        .reply(
            &id,
            json!({"turn": {"id": "turn-2", "status": "inProgress"}}),
        )
        .await;
    assert!(accepted(answer).await);
}

#[tokio::test]
async fn completed_turn_means_delivery_starts_a_turn() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    d.server.send(turn_completed("turn-1")).await;
    barrier(&mut d.server).await;
    let answer = deliver(&d, "next").await;
    let (id, params) = d.server.expect_request("turn/start").await;
    assert_eq!(params["input"], text_input("next"));
    d.server
        .reply(
            &id,
            json!({"turn": {"id": "turn-2", "status": "inProgress"}}),
        )
        .await;
    assert!(accepted(answer).await);
}

#[tokio::test]
async fn failed_turn_start_replies_false_and_keeps_running() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    let answer = deliver(&d, "lost").await;
    let (id, _) = d.server.expect_request("turn/start").await;
    d.server.fail(&id, -32600, "thread not loaded").await;
    assert!(!accepted(answer).await);
    barrier(&mut d.server).await;
}

#[tokio::test]
async fn command_and_file_approvals_are_declined() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    for (n, method) in [
        (1, "item/commandExecution/requestApproval"),
        (2, "item/fileChange/requestApproval"),
    ] {
        let reply = ask(&mut d.server, n, method, json!({"threadId": THREAD})).await;
        assert_eq!(reply["result"], json!({"decision": "decline"}), "{method}");
    }
}

#[tokio::test]
async fn legacy_approvals_are_denied_without_stopping_the_turn() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    for (n, method) in [(3, "execCommandApproval"), (4, "applyPatchApproval")] {
        let reply = ask(&mut d.server, n, method, json!({})).await;
        let rejection = &reply["result"]["decision"]["denied"]["rejection"];
        assert!(
            rejection.as_str().is_some_and(|s| !s.is_empty()),
            "{method}: {reply}"
        );
    }
}

#[tokio::test]
async fn elicitation_is_declined() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    let reply = ask(&mut d.server, 5, "mcpServer/elicitation/request", json!({})).await;
    assert_eq!(reply["result"], json!({"action": "decline"}));
}

#[tokio::test]
async fn unknown_server_request_gets_method_not_found() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    let reply = ask(&mut d.server, 6, "some/unknown/request", json!({})).await;
    assert_eq!(reply["error"]["code"], -32601);
    assert_eq!(reply["error"]["message"], "not supported by agora bridge");
}

#[tokio::test]
async fn shutdown_interrupts_the_active_turn() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    barrier(&mut d.server).await;
    d.shutdown.cancel();
    let (id, params) = d.server.expect_request("turn/interrupt").await;
    assert_eq!(params["threadId"], THREAD);
    assert_eq!(params["turnId"], "turn-1");
    d.server.reply(&id, json!({})).await;
    let result = tokio::time::timeout(T, d.task)
        .await
        .expect("driver hung")
        .expect("driver panicked");
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn closed_stream_ends_drive_cleanly() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    drop(d.server);
    let result = tokio::time::timeout(T, d.task)
        .await
        .expect("driver hung")
        .expect("driver panicked");
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn run_names_the_binary_when_it_cannot_start() {
    let bin = "/nonexistent/agora-test/codex";
    let opts = CodexOptions::new(PathBuf::from(bin), PathBuf::from("/tmp"));
    let err = run(
        opts,
        PathBuf::from("/tmp/home"),
        CancellationToken::new(),
        None,
    )
    .await
    .expect_err("a missing binary cannot start");
    assert!(
        matches!(&err, BridgeError::Spawn(msg) if msg.contains(bin)),
        "{err:?}"
    );
}

#[tokio::test]
async fn run_fails_when_the_app_server_exits_before_ready() {
    // `false` starts, exits at once and closes its stdout without answering `initialize`.
    let opts = CodexOptions::new(PathBuf::from("/usr/bin/false"), PathBuf::from("/tmp"));
    let err = run(
        opts,
        PathBuf::from("/tmp/home"),
        CancellationToken::new(),
        None,
    )
    .await
    .expect_err("an exited server never becomes ready");
    assert!(matches!(err, BridgeError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn run_kills_the_app_servers_grandchildren_on_shutdown() {
    // `sh app-server` runs the script from the working directory, so the script needs no execute
    // bit. The script never reads stdin, so only the kill ends it.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("app-server"),
        "sleep 600 &\necho $! > grandchild.tmp && mv grandchild.tmp grandchild.pid\nwait\n",
    )
    .expect("write the fake app-server");
    let opts = CodexOptions::new(PathBuf::from("/bin/sh"), dir.path().to_path_buf());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run(
        opts,
        PathBuf::from("/tmp/home"),
        shutdown.clone(),
        None,
    ));
    let grandchild = grandchild_pid(&dir.path().join("grandchild.pid")).await;
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("run hung")
        .expect("run panicked");
    assert!(result.is_ok(), "{result:?}");
    assert!(
        gone(grandchild).await,
        "grandchild {grandchild} survived the kill"
    );
}

/// The read-only agora tools the bridge pre-approves. Every other agora tool stays on `prompt`.
fn assert_agora_approval(params: &Value) {
    let agora = &params["config"]["mcp_servers"]["agora"];
    assert_eq!(agora["default_tools_approval_mode"], "prompt", "{params}");
    let tools = agora["tools"].as_object().expect("per-tool approvals");
    let approved: BTreeSet<&str> = tools
        .iter()
        .filter(|(_, tool)| tool["approval_mode"] == "approve")
        .map(|(name, _)| name.as_str())
        .collect();
    let expected = BTreeSet::from([
        "ack",
        "board_summary",
        "card_list",
        "chat",
        "inbox",
        "inbox_wait",
        "note_search",
        "react",
        "read",
        "whoami",
    ]);
    assert_eq!(approved, expected, "{params}");
    assert_eq!(tools.len(), expected.len(), "{params}");
}

/// The pid the fake app-server wrote for its grandchild, once the file exists.
async fn grandchild_pid(file: &Path) -> libc::pid_t {
    for _ in 0..400 {
        if let Some(pid) = std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the fake app-server never wrote {}", file.display());
}

/// Whether `pid` is gone. A killed process is reaped by its new parent, so this polls briefly.
async fn gone(pid: libc::pid_t) -> bool {
    for _ in 0..400 {
        // SAFETY: signal 0 sends nothing; it only asks whether the pid exists.
        let missing = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if missing {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[path = "codex_mock/turns.rs"]
mod turns;
