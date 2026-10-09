#![cfg(feature = "acp")]
//! The ACP driver against a scripted agent: the test plays `copilot --acp --stdio` over a duplex
//! pipe, writing NDJSON with the `"jsonrpc":"2.0"` field, and checks every message the driver writes.

use agora_bridge::acp::{AcpOptions, drive};
use agora_bridge::jsonrpc::RpcConn;
use agora_bridge::{BridgeError, Delivery};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const T: Duration = Duration::from_secs(5);
const SESSION: &str = "sess-1";
/// Capacity of the deliveries channel; `drained` waits for all of it to be free again.
const SLOTS: usize = 4;

/// The scripted agent: reads what the driver writes, writes what the driver reads.
struct Server {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    out: WriteHalf<DuplexStream>,
}

impl Server {
    /// The next message from the driver. Panics on silence, a closed pipe, or a missing `jsonrpc`.
    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(T, self.lines.next_line())
            .await
            .expect("driver sent nothing")
            .expect("read failed")
            .expect("driver closed the pipe");
        let msg: Value = serde_json::from_str(&line).expect("driver wrote invalid JSON");
        assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
        msg
    }

    /// Writes `msg` with the `jsonrpc` field this agent speaks.
    async fn send(&mut self, mut msg: Value) {
        msg["jsonrpc"] = json!("2.0");
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

fn options() -> AcpOptions {
    AcpOptions {
        bin: PathBuf::from("/unused/copilot"),
        cwd: PathBuf::from("/tmp/agora-project"),
        session: None,
        prompt: None,
        allow_tools: false,
        exe: PathBuf::from("/usr/local/bin/agora"),
        home: PathBuf::from("/tmp/agora-home"),
    }
}

/// Start `drive` on one end of a duplex pipe; the test holds the other end.
fn start(opts: AcpOptions) -> Driver {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (client_read, client_write) = split(client_io);
    let (server_read, server_write) = split(server_io);
    let (conn, incoming) = RpcConn::spawn(client_read, client_write, true);
    let (deliveries, delivery_rx) = mpsc::channel(SLOTS);
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

/// Answers `initialize` and checks the bridge's handshake; `load_session` is the agent's
/// `agentCapabilities.loadSession`.
async fn handshake(server: &mut Server, load_session: bool) {
    let (id, params) = server.expect_request("initialize").await;
    assert_eq!(params["protocolVersion"], 1, "{params}");
    assert_eq!(
        params["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
    );
    assert_eq!(params["clientInfo"]["name"], "agora_bridge");
    assert_eq!(params["clientInfo"]["title"], "Agora bridge");
    assert_eq!(params["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
    let result = json!({
        "protocolVersion": 1,
        "agentCapabilities": {"loadSession": load_session},
        "authMethods": [],
    });
    server.reply(&id, result).await;
}

async fn ready(d: &mut Driver) -> String {
    tokio::time::timeout(T, &mut d.ready)
        .await
        .expect("ready not sent")
        .expect("ready dropped")
}

/// Start the driver, complete the handshake and `session/new`, and check `ready` carries the id.
async fn open_new(opts: AcpOptions) -> Driver {
    let mut d = start(opts);
    handshake(&mut d.server, false).await;
    let (id, _) = d.server.expect_request("session/new").await;
    d.server.reply(&id, json!({"sessionId": SESSION})).await;
    assert_eq!(ready(&mut d).await, SESSION);
    d
}

/// Cancels the driver's shutdown token and returns what the driver ended with.
async fn stop(d: Driver) -> Result<(), BridgeError> {
    d.shutdown.cancel();
    tokio::time::timeout(T, d.task)
        .await
        .expect("driver did not stop")
        .expect("driver panicked")
}

/// Queue one delivery; the returned receiver yields the driver's answer.
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

async fn answered(answer: oneshot::Receiver<bool>) -> bool {
    tokio::time::timeout(T, answer)
        .await
        .expect("no reply to the delivery")
        .expect("reply dropped")
}

/// Waits until the driver has taken every queued delivery. The driver merges a delivery in the
/// same step that receives it, so once the slots are back, nothing is left unmerged.
async fn drained(d: &Driver) {
    tokio::time::timeout(T, async {
        while d.deliveries.capacity() < SLOTS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("driver did not take the deliveries");
}

/// Sends a server request and returns the driver's answer to it.
async fn ask(server: &mut Server, id: i64, method: &str, params: Value) -> Value {
    server
        .send(json!({"id": id, "method": method, "params": params}))
        .await;
    let reply = server.recv().await;
    assert_eq!(reply["id"], id, "{reply}");
    reply
}

fn text_blocks(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn permission(options: Value) -> Value {
    permission_for("Run a command", options)
}

/// A permission request for the tool titled `title`.
fn permission_for(title: &str, options: Value) -> Value {
    json!({
        "sessionId": SESSION,
        "toolCall": {"toolCallId": "call-1", "title": title},
        "options": options,
    })
}

/// One allow option and one reject option, the shape of a permission request.
fn allow_or_reject() -> Value {
    json!([
        {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
        {"optionId": "no", "name": "Reject", "kind": "reject_once"},
    ])
}

fn selected(option: &str) -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": option}})
}

/// Delivers `text` to an idle session and reads the `session/prompt` it starts.
async fn start_prompt(d: &mut Driver, text: &str) -> (Value, oneshot::Receiver<bool>) {
    let answer = deliver(d, text).await;
    let (id, params) = d.server.expect_request("session/prompt").await;
    assert_eq!(params["sessionId"], SESSION, "{params}");
    assert_eq!(params["prompt"], text_blocks(text), "{params}");
    (id, answer)
}

#[tokio::test]
async fn handshake_and_session_new_send_the_agora_mcp_server() {
    let mut d = start(options());
    handshake(&mut d.server, false).await;
    let (id, params) = d.server.expect_request("session/new").await;
    assert_eq!(params["cwd"], "/tmp/agora-project");
    assert_eq!(
        params["mcpServers"],
        json!([{
            "name": "agora",
            "command": "/usr/local/bin/agora",
            "args": ["--home", "/tmp/agora-home", "mcp"],
            "env": [],
        }])
    );
    d.server.reply(&id, json!({"sessionId": SESSION})).await;
    assert_eq!(ready(&mut d).await, SESSION);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn session_load_is_used_when_the_agent_can_load_sessions() {
    let opts = AcpOptions {
        session: Some("sess-9".into()),
        ..options()
    };
    let mut d = start(opts);
    handshake(&mut d.server, true).await;
    let (id, params) = d.server.expect_request("session/load").await;
    assert_eq!(params["sessionId"], "sess-9", "{params}");
    assert_eq!(params["cwd"], "/tmp/agora-project");
    assert_eq!(params["mcpServers"][0]["name"], "agora", "{params}");
    d.server.reply(&id, Value::Null).await;
    assert_eq!(ready(&mut d).await, "sess-9");
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn session_load_without_the_capability_is_an_error() {
    let opts = AcpOptions {
        session: Some("sess-9".into()),
        ..options()
    };
    let mut d = start(opts);
    handshake(&mut d.server, false).await;
    let outcome = tokio::time::timeout(T, d.task)
        .await
        .expect("driver did not end")
        .expect("driver panicked");
    match outcome {
        Err(BridgeError::Protocol(message)) => {
            assert!(message.contains("cannot load sessions"), "{message}");
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_protocol_version_other_than_one_is_an_error() {
    let mut d = start(options());
    let (id, _) = d.server.expect_request("initialize").await;
    d.server
        .reply(&id, json!({"protocolVersion": 2, "agentCapabilities": {}}))
        .await;
    let outcome = tokio::time::timeout(T, d.task)
        .await
        .expect("driver did not end")
        .expect("driver panicked");
    assert!(
        matches!(outcome, Err(BridgeError::Protocol(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn idle_delivery_sends_one_prompt_and_is_accepted() {
    let mut d = open_new(options()).await;
    let (id, answer) = start_prompt(&mut d, "hello").await;
    d.server.reply(&id, json!({"stopReason": "end_turn"})).await;
    assert!(answered(answer).await);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn deliveries_during_a_prompt_merge_into_one_following_prompt() {
    let mut d = open_new(options()).await;
    let (first_id, first) = start_prompt(&mut d, "A").await;
    let second = deliver(&d, "B").await;
    let third = deliver(&d, "C").await;
    drained(&d).await;
    d.server
        .reply(&first_id, json!({"stopReason": "end_turn"}))
        .await;
    assert!(answered(first).await);
    // B and C go out as one prompt, after the first one ended.
    let (second_id, params) = d.server.expect_request("session/prompt").await;
    assert_eq!(params["prompt"], text_blocks("B\n\nC"), "{params}");
    d.server
        .reply(&second_id, json!({"stopReason": "end_turn"}))
        .await;
    assert!(answered(second).await);
    assert!(answered(third).await);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn refused_prompt_replies_false() {
    let mut d = open_new(options()).await;
    let (id, answer) = start_prompt(&mut d, "hello").await;
    d.server.fail(&id, -32602, "no such session").await;
    assert!(!answered(answer).await);
    assert!(stop(d).await.is_ok());
}

/// A batch is answered from its prompt's outcome, not from a timer: a turn that runs long keeps its
/// reply open until the turn ends.
#[tokio::test(start_paused = true)]
async fn a_batch_is_not_answered_while_its_prompt_runs() {
    let mut d = open_new(options()).await;
    let (id, mut answer) = start_prompt(&mut d, "work").await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(
        matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "answered while the turn was still running"
    );
    d.server.reply(&id, json!({"stopReason": "end_turn"})).await;
    assert!(answered(answer).await);
    assert!(stop(d).await.is_ok());
}

/// A prompt that fails after a long turn leaves the batch unacked, so the pump offers it again.
#[tokio::test(start_paused = true)]
async fn a_late_prompt_error_replies_false_and_the_batch_is_sent_again() {
    let mut d = open_new(options()).await;
    let (id, mut answer) = start_prompt(&mut d, "hello").await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(
        matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "answered while the turn was still running"
    );
    d.server.fail(&id, -32603, "rate limited").await;
    assert!(!answered(answer).await);
    let retry = deliver(&d, "hello").await;
    let (retry_id, params) = d.server.expect_request("session/prompt").await;
    assert_eq!(params["prompt"], text_blocks("hello"), "{params}");
    d.server
        .reply(&retry_id, json!({"stopReason": "end_turn"}))
        .await;
    assert!(answered(retry).await);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn a_cancelled_turn_replies_false() {
    let mut d = open_new(options()).await;
    let (id, answer) = start_prompt(&mut d, "hello").await;
    d.server
        .reply(&id, json!({"stopReason": "cancelled"}))
        .await;
    assert!(!answered(answer).await);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn permission_requests_are_rejected_unless_tools_are_allowed() {
    let mut d = open_new(options()).await;
    let chosen = ask(
        &mut d.server,
        1,
        "session/request_permission",
        permission(json!([
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "never", "name": "Never", "kind": "reject_always"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"},
        ])),
    )
    .await;
    assert_eq!(chosen["result"], selected("no"), "{chosen}");
    // Without reject_once, reject_always is the rejection.
    let always = ask(
        &mut d.server,
        2,
        "session/request_permission",
        permission(json!([
            {"optionId": "never", "name": "Never", "kind": "reject_always"},
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
        ])),
    )
    .await;
    assert_eq!(always["result"], selected("never"), "{always}");
    // Nothing to reject with: cancelled.
    let cancelled = ask(
        &mut d.server,
        3,
        "session/request_permission",
        permission(json!([{"optionId": "allow", "name": "Allow", "kind": "allow_once"}])),
    )
    .await;
    assert_eq!(
        cancelled["result"],
        json!({"outcome": {"outcome": "cancelled"}}),
        "{cancelled}"
    );
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn permission_requests_allow_once_when_tools_are_allowed() {
    let opts = AcpOptions {
        allow_tools: true,
        ..options()
    };
    let mut d = open_new(opts).await;
    let chosen = ask(
        &mut d.server,
        1,
        "session/request_permission",
        permission_for("agora-chat", allow_or_reject()),
    )
    .await;
    assert_eq!(chosen["result"], selected("allow"), "{chosen}");
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn a_shell_permission_is_rejected_even_when_tools_are_allowed() {
    let opts = AcpOptions {
        allow_tools: true,
        ..options()
    };
    let mut d = open_new(opts).await;
    let chosen = ask(
        &mut d.server,
        1,
        "session/request_permission",
        permission(allow_or_reject()),
    )
    .await;
    assert_eq!(chosen["result"], selected("no"), "{chosen}");
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn listed_agora_tools_are_allowed_in_both_title_forms() {
    let opts = AcpOptions {
        allow_tools: true,
        ..options()
    };
    let mut d = open_new(opts).await;
    let titles = [
        "agora-chat",
        "mcp__agora__inbox_wait",
        "agora-card_list",
        "agora-ack",
    ];
    for (id, title) in (1..).zip(titles) {
        let chosen = ask(
            &mut d.server,
            id,
            "session/request_permission",
            permission_for(title, allow_or_reject()),
        )
        .await;
        assert_eq!(chosen["result"], selected("allow"), "{title}: {chosen}");
    }
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn agora_tools_outside_the_list_are_rejected_when_tools_are_allowed() {
    let opts = AcpOptions {
        allow_tools: true,
        ..options()
    };
    let mut d = open_new(opts).await;
    let titles = [
        "agora-work_run",
        "agora-card_move",
        "mcp__agora__role_grant",
        "agora-revoke",
        "agora-channel_add",
        "agora-fetch_file",
    ];
    for (id, title) in (1..).zip(titles) {
        let chosen = ask(
            &mut d.server,
            id,
            "session/request_permission",
            permission_for(title, allow_or_reject()),
        )
        .await;
        assert_eq!(chosen["result"], selected("no"), "{title}: {chosen}");
    }
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn unrecognised_permission_shapes_are_rejected_when_tools_are_allowed() {
    let opts = AcpOptions {
        allow_tools: true,
        ..options()
    };
    let mut d = open_new(opts).await;
    let shapes = [
        json!({"sessionId": SESSION, "toolCall": {"toolCallId": "call-1"}, "options": allow_or_reject()}),
        json!({"sessionId": SESSION, "toolCall": {"toolCallId": "call-1", "title": null}, "options": allow_or_reject()}),
        json!({"sessionId": SESSION, "options": allow_or_reject()}),
        permission_for("agora-chat extra", allow_or_reject()),
        permission_for("Run agora-chat", allow_or_reject()),
        permission_for("agora-", allow_or_reject()),
        permission_for("other-chat", allow_or_reject()),
    ];
    for (id, params) in (1..).zip(shapes) {
        let chosen = ask(
            &mut d.server,
            id,
            "session/request_permission",
            params.clone(),
        )
        .await;
        assert_eq!(chosen["result"], selected("no"), "{params}: {chosen}");
    }
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn fs_and_terminal_requests_are_refused_while_a_prompt_runs() {
    let mut d = open_new(options()).await;
    let (prompt_id, answer) = start_prompt(&mut d, "work").await;
    // The driver keeps answering while the prompt is in flight.
    let read = ask(
        &mut d.server,
        10,
        "fs/read_text_file",
        json!({"sessionId": SESSION, "path": "/tmp/x"}),
    )
    .await;
    assert_eq!(read["error"]["code"], -32601, "{read}");
    let terminal = ask(
        &mut d.server,
        11,
        "terminal/create",
        json!({"sessionId": SESSION, "command": "ls"}),
    )
    .await;
    assert_eq!(terminal["error"]["code"], -32601, "{terminal}");
    d.server
        .reply(&prompt_id, json!({"stopReason": "end_turn"}))
        .await;
    assert!(answered(answer).await);
    assert!(stop(d).await.is_ok());
}

#[tokio::test]
async fn shutdown_during_a_prompt_cancels_the_turn() {
    let mut d = open_new(options()).await;
    let (_id, _answer) = start_prompt(&mut d, "work").await;
    d.shutdown.cancel();
    let cancel = d.server.recv().await;
    assert_eq!(cancel["method"], "session/cancel", "{cancel}");
    assert_eq!(cancel["params"], json!({"sessionId": SESSION}));
    assert!(cancel.get("id").is_none(), "{cancel}");
    let outcome = tokio::time::timeout(T, d.task)
        .await
        .expect("driver did not stop")
        .expect("driver panicked");
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn shutdown_during_setup_ends_the_driver_cleanly() {
    let mut d = start(options());
    // The agent never answers `initialize`; shutdown must not wait for it.
    let (_id, _params) = d.server.expect_request("initialize").await;
    let outcome = stop(d).await;
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn end_of_stream_ends_the_driver_cleanly() {
    let Driver { server, task, .. } = open_new(options()).await;
    drop(server);
    let outcome = tokio::time::timeout(T, task)
        .await
        .expect("driver did not end")
        .expect("driver panicked");
    assert!(outcome.is_ok(), "{outcome:?}");
}
