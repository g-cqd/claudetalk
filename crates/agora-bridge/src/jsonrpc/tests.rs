use super::*;
use std::collections::HashMap;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};

const T: Duration = Duration::from_secs(5);

/// A client connection plus the peer's end of the duplex pipe.
struct Harness {
    conn: RpcConn,
    incoming: mpsc::Receiver<Incoming>,
    /// Lines the client wrote, read by the test acting as the peer.
    from_client: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    /// The peer's write end, feeding the client's reader.
    to_client: WriteHalf<DuplexStream>,
}

fn harness_with(jsonrpc: bool, cap: usize) -> Harness {
    let (client_io, peer_io) = tokio::io::duplex(1 << 16);
    let (client_read, client_write) = split(client_io);
    let (peer_read, peer_write) = split(peer_io);
    let (conn, incoming) = spawn_with_cap(client_read, client_write, jsonrpc, cap, WRITE_TIMEOUT);
    Harness {
        conn,
        incoming,
        from_client: BufReader::new(peer_read).lines(),
        to_client: peer_write,
    }
}

fn harness(jsonrpc: bool) -> Harness {
    harness_with(jsonrpc, MAX_LINE)
}

impl Harness {
    /// The next line the client wrote, parsed.
    async fn next_from_client(&mut self) -> Value {
        let line = self
            .from_client
            .next_line()
            .await
            .expect("read from client")
            .expect("client wrote a line");
        serde_json::from_str(&line).expect("client line is JSON")
    }

    async fn send_to_client(&mut self, line: &str) {
        self.to_client
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write to client");
    }
}

async fn round_trip(jsonrpc: bool) {
    let mut h = harness(jsonrpc);
    let conn = h.conn.clone();
    let call = tokio::spawn(async move {
        conn.request("initialize", json!({"clientInfo": {"name": "x"}}), T)
            .await
    });
    let sent = h.next_from_client().await;
    assert_eq!(sent["method"], "initialize");
    assert_eq!(sent["id"], 1);
    assert_eq!(sent.get("jsonrpc").cloned(), jsonrpc.then(|| json!("2.0")));
    h.send_to_client(&json!({"id": 1, "result": {"ok": true}}).to_string())
        .await;
    assert_eq!(call.await.expect("join"), Ok(json!({"ok": true})));

    // A notification with null params has no `params` member and the same jsonrpc rule.
    h.conn
        .notify("initialized", Value::Null)
        .await
        .expect("notify");
    let note = h.next_from_client().await;
    assert_eq!(
        note,
        if jsonrpc {
            json!({"method": "initialized", "jsonrpc": "2.0"})
        } else {
            json!({"method": "initialized"})
        }
    );
}

#[tokio::test]
async fn round_trip_without_jsonrpc_field() {
    round_trip(false).await;
}

#[tokio::test]
async fn round_trip_with_jsonrpc_field() {
    round_trip(true).await;
}

#[tokio::test]
async fn concurrent_requests_match_out_of_order_responses() {
    let mut h = harness(false);
    let (first, second) = (h.conn.clone(), h.conn.clone());
    let call_first = tokio::spawn(async move { first.request("first", json!({}), T).await });
    let call_second = tokio::spawn(async move { second.request("second", json!({}), T).await });
    let mut ids = HashMap::new();
    for _ in 0..2 {
        let sent = h.next_from_client().await;
        let method = sent["method"].as_str().expect("method").to_owned();
        ids.insert(method, sent["id"].clone());
    }
    assert_ne!(ids["first"], ids["second"]);
    h.send_to_client(&json!({"id": ids["second"], "result": "B"}).to_string())
        .await;
    h.send_to_client(&json!({"id": ids["first"], "result": "A"}).to_string())
        .await;
    assert_eq!(call_first.await.expect("join"), Ok(json!("A")));
    assert_eq!(call_second.await.expect("join"), Ok(json!("B")));
}

#[tokio::test]
async fn remote_error_object_maps_to_remote() {
    let mut h = harness(false);
    let conn = h.conn.clone();
    let call = tokio::spawn(async move { conn.request("nope", json!({}), T).await });
    let sent = h.next_from_client().await;
    h.send_to_client(
        &json!({"id": sent["id"], "error": {"code": -32601, "message": "method not found"}})
            .to_string(),
    )
    .await;
    assert_eq!(
        call.await.expect("join"),
        Err(RpcError::Remote {
            code: -32601,
            message: "method not found".to_owned(),
        })
    );
}

#[tokio::test]
async fn timeout_removes_pending_and_late_response_is_ignored() {
    let mut h = harness(false);
    let outcome = h
        .conn
        .request("slow", json!({}), Duration::from_millis(50))
        .await;
    assert_eq!(outcome, Err(RpcError::Timeout));
    assert!(locked(&h.conn.inner.pending).is_empty());

    let sent = h.next_from_client().await;
    h.send_to_client(&json!({"id": sent["id"], "result": "late"}).to_string())
        .await;
    // The stale answer goes nowhere; the next thing on the channel is the notification.
    h.send_to_client(&json!({"method": "ping", "params": {}}).to_string())
        .await;
    assert_eq!(
        h.incoming.recv().await,
        Some(Incoming::Notification {
            method: "ping".to_owned(),
            params: json!({}),
        })
    );
}

#[tokio::test]
async fn server_request_and_notification_reach_the_channel() {
    let mut h = harness(false);
    h.send_to_client(
        &json!({
            "id": "req-7",
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "t1"},
        })
        .to_string(),
    )
    .await;
    h.send_to_client(
        &json!({"method": "thread/started", "params": {"thread": {"id": "t1"}}}).to_string(),
    )
    .await;
    assert_eq!(
        h.incoming.recv().await,
        Some(Incoming::Request {
            id: json!("req-7"),
            method: "item/commandExecution/requestApproval".to_owned(),
            params: json!({"threadId": "t1"}),
        })
    );
    assert_eq!(
        h.incoming.recv().await,
        Some(Incoming::Notification {
            method: "thread/started".to_owned(),
            params: json!({"thread": {"id": "t1"}}),
        })
    );
}

#[tokio::test]
async fn respond_and_respond_error_echo_the_id_exactly() {
    let mut h = harness(false);
    h.conn
        .respond(json!("req-7"), json!({"decision": "decline"}))
        .await
        .expect("respond");
    h.conn
        .respond_error(json!(42), -32601, "method not found")
        .await
        .expect("respond_error");
    assert_eq!(
        h.next_from_client().await,
        json!({"id": "req-7", "result": {"decision": "decline"}})
    );
    assert_eq!(
        h.next_from_client().await,
        json!({"id": 42, "error": {"code": -32601, "message": "method not found"}})
    );
}

#[tokio::test]
async fn malformed_line_is_skipped_and_later_messages_still_arrive() {
    let mut h = harness(false);
    h.send_to_client("this is not json").await;
    h.send_to_client(&json!({"id": 99, "result": 1}).to_string())
        .await;
    h.send_to_client(&json!({"method": "after"}).to_string())
        .await;
    assert_eq!(
        h.incoming.recv().await,
        Some(Incoming::Notification {
            method: "after".to_owned(),
            params: Value::Null,
        })
    );
}

#[tokio::test]
async fn blank_lines_are_skipped_and_later_messages_still_arrive() {
    let mut h = harness(false);
    h.send_to_client("").await;
    h.send_to_client("   \t").await;
    h.send_to_client("\r").await;
    h.send_to_client(&json!({"method": "after"}).to_string())
        .await;
    // The first item on the channel is the notification: the blank lines produced nothing.
    assert_eq!(
        h.incoming.recv().await,
        Some(Incoming::Notification {
            method: "after".to_owned(),
            params: Value::Null,
        })
    );
}

#[tokio::test]
async fn dropped_request_future_removes_its_pending_entry() {
    let mut h = harness(false);
    let conn = h.conn.clone();
    let call = tokio::spawn(async move { conn.request("hang", json!({}), T).await });
    // Reading the line means the entry was registered before the write.
    let _sent = h.next_from_client().await;
    assert_eq!(h.conn.pending_len(), 1);
    call.abort();
    assert!(call.await.expect_err("aborted task").is_cancelled());
    assert_eq!(h.conn.pending_len(), 0);
}

#[tokio::test]
async fn respond_on_a_writer_that_never_drains_times_out() {
    // An 8-byte pipe whose peer never reads: the writer blocks on its first line, the outgoing
    // queue fills behind it, and the first respond with no room gives up.
    let (client_io, _peer_io) = tokio::io::duplex(8);
    let (client_read, client_write) = split(client_io);
    let (conn, _incoming) = spawn_with_cap(
        client_read,
        client_write,
        false,
        MAX_LINE,
        Duration::from_millis(50),
    );
    let mut outcome = Ok(());
    for n in 0..2 * OUTGOING_CAP {
        outcome = conn.respond(json!(n), json!({})).await;
        if outcome.is_err() {
            break;
        }
    }
    assert_eq!(outcome, Err(RpcError::Timeout));
}

/// A JSON notification for `method`, padded with trailing spaces to exactly `len` bytes.
fn padded(len: usize, method: &str) -> String {
    let base = json!({"method": method}).to_string();
    format!("{base}{}", " ".repeat(len - base.len()))
}

#[tokio::test]
async fn over_long_line_is_skipped_and_a_line_at_the_cap_is_kept() {
    const CAP: usize = 64;
    let mut h = harness_with(false, CAP);
    // Spans several reader buffers, so the skip runs across chunks.
    h.send_to_client(&padded(20_000, "too-long")).await;
    h.send_to_client(&padded(CAP, "at-cap")).await;
    h.send_to_client(&json!({"method": "after"}).to_string())
        .await;
    let method_of = |item: Option<Incoming>| match item {
        Some(Incoming::Notification { method, .. }) => method,
        other => panic!("expected a notification, got {other:?}"),
    };
    assert_eq!(method_of(h.incoming.recv().await), "at-cap");
    assert_eq!(method_of(h.incoming.recv().await), "after");
}

#[tokio::test]
async fn eof_fails_pending_requests_and_closes_the_channel() {
    let mut h = harness(false);
    let conn = h.conn.clone();
    let call = tokio::spawn(async move { conn.request("hang", json!({}), T).await });
    // The request is registered before its line is written, so reading the line means it is
    // pending.
    let _sent = h.next_from_client().await;
    let Harness {
        conn,
        mut incoming,
        from_client,
        to_client,
    } = h;
    drop(from_client);
    drop(to_client);

    assert_eq!(call.await.expect("join"), Err(RpcError::Closed));
    assert_eq!(incoming.recv().await, None);
    assert_eq!(
        conn.request("after-close", json!({}), T).await,
        Err(RpcError::Closed)
    );
    assert_eq!(
        conn.notify("after-close", Value::Null).await,
        Err(RpcError::Closed)
    );
}
