//! Raw JSON-RPC over HTTP (`POST /mcp`), driven with bearer tokens against an in-process daemon.
//!
//! Covers both protocol eras on the one stateless endpoint, the error and status mapping, the
//! auth / origin / host / method / media checks in their documented order, the HTTP ban on
//! filesystem paths, concurrent clients each bound to its own identity, and the SSE progress
//! stream. Every wait is bounded, so a bug fails instead of hanging.

mod common;

use agora_client::Ctx;
use agora_daemon::{Paths, now_ms};
use agora_mcp::McpError;
use agora_mcp::engine::{Engine, EngineConfig};
use agora_mcp::http::{self, HttpOptions};
use agora_mcp::jsonrpc::MAX_MESSAGE_BYTES;
use agora_mcp::tokens::{self, TokenEntry};
use common::{HttpHarness, StdioClient, World};
use reqwest::header::{ACCESS_CONTROL_REQUEST_METHOD, HeaderMap};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Upper bound on each wait for a response.
const WAIT: Duration = Duration::from_secs(10);
/// Upper bound on a server stopping after it was asked to.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Clients in the concurrency tests.
const CLIENTS: usize = 10;
/// Tools served by `tools/list`.
const TOOL_COUNT: usize = 57;
/// A woken `inbox_wait` returns well before its 5 s timeout.
const WAKE_BOUND: Duration = Duration::from_secs(4);
/// The two protocol versions a modern `server/discover` lists.
const SUPPORTED: [&str; 2] = ["2026-07-28", "2025-11-25"];

fn rpc(id: i64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn tool_call(id: i64, name: &str, arguments: Value) -> String {
    rpc(
        id,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
}

/// A modern-era request: `params` plus the modern `_meta` (protocol 2026-07-28).
fn modern_request(id: i64, method: &str, mut params: Value) -> String {
    params["_meta"] = StdioClient::modern_meta("http-test");
    rpc(id, method, params)
}

/// The headers a modern client sends with a request for `method`.
fn modern_headers(method: &str) -> [(&str, &str); 2] {
    [
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", method),
    ]
}

/// A response body as JSON; panics with the body when it is not JSON.
fn json_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"))
}

/// The `result` of a JSON-RPC response; panics with the body when there is an error.
fn result_of(body: &str) -> Value {
    let response = json_body(body);
    assert!(response.get("error").is_none(), "unexpected error: {body}");
    response["result"].clone()
}

fn assert_tool_ok(result: &Value) {
    assert_eq!(result["isError"], false, "expected success, got {result}");
}

fn assert_tool_error(result: &Value, code: &str, message: &str) {
    assert_eq!(
        result["isError"], true,
        "expected a tool error, got {result}"
    );
    assert_eq!(
        result["structuredContent"]["error"]["code"], code,
        "{result}"
    );
    assert_eq!(
        result["structuredContent"]["error"]["message"], message,
        "{result}"
    );
}

/// The text of each message in a tool result's `structuredContent.messages`, in order.
fn message_texts(result: &Value) -> Vec<String> {
    result["structuredContent"]["messages"]
        .as_array()
        .expect("structuredContent has a messages array")
        .iter()
        .map(|m| m["text"].as_str().expect("a message has text").to_owned())
        .collect()
}

/// The `data` payloads of an SSE body. Every event must be `event: message` with one data line.
fn sse_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| {
            let data = event
                .strip_prefix("event: message\ndata: ")
                .unwrap_or_else(|| panic!("not an SSE message event: {event:?}"));
            json_body(data)
        })
        .collect()
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn channel_of(i: usize) -> String {
    format!("c{i}")
}

fn token_file(world: &World) -> PathBuf {
    tokens::default_path(world.paths())
}

/// The token entry of bearer token `index` of `harness`.
fn token_entry(world: &World, harness: &HttpHarness, index: usize) -> TokenEntry {
    let file = tokens::load(&token_file(world)).expect("load the token file");
    tokens::verify(&file, harness.token(index))
        .expect("the token is in the token file")
        .clone()
}

/// The client context bound to bearer token `index`, for in-process posts and membership.
fn token_ctx(world: &World, harness: &HttpHarness, index: usize) -> Ctx {
    let entry = token_entry(world, harness, index);
    let identity = tokens::identity_of(&entry).expect("the token's identity");
    Ctx {
        paths: world.paths().clone(),
        identity,
        label: format!("token-{index}"),
    }
}

/// Registers clients `0..count` through their tokens (a whoami, which is each identity's first
/// tools/call), and gives client `i` the channel `c<i>` whose only members are the owner and
/// client `i`.
async fn bind_clients(world: &World, owner: &Ctx, harness: &HttpHarness, count: usize) {
    for i in 0..count {
        let (status, _, body) = harness
            .post(
                Some(harness.token(i)),
                &tool_call(1, "whoami", json!({})),
                &[],
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_tool_ok(&result_of(&body));
        let client = token_ctx(world, harness, i);
        world.channel(owner, &channel_of(i), &[&client]).await;
    }
}

/// Joins every spawned call within `WAIT` each, keyed by client index. Each must answer 200.
async fn join_calls(calls: &mut JoinSet<(usize, u16, String)>) -> BTreeMap<usize, Value> {
    let mut results = BTreeMap::new();
    while let Some(joined) = timeout(WAIT, calls.join_next())
        .await
        .expect("every call answers within the wait bound")
    {
        let (i, status, body) = joined.expect("the call task does not panic");
        assert_eq!(status, 200, "client {i}: {body}");
        results.insert(i, result_of(&body));
    }
    results
}

/// A legacy `initialize` with `clientInfo.name = claude-code` (the harness that would receive
/// channel push over stdio) must still get no session and no channel capability over HTTP.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn legacy_initialize_mints_no_session_and_declares_no_channel() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": "1.0"},
        }),
    );
    let (status, headers, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{text}");
    assert!(
        header_text(&headers, "mcp-session-id").is_none(),
        "HTTP never mints a session"
    );
    let result = result_of(&text);
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert_eq!(
        result["capabilities"],
        json!({"tools": {"listChanged": false}}),
        "HTTP never pushes, so no experimental claude/channel"
    );
    harness.shutdown().await;
    world.shutdown().await;
}

/// A legacy `tools/list` with the version header and no session gets the full tool table.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn legacy_tools_list_needs_no_session() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "tools/list", json!({})),
            &[("MCP-Protocol-Version", "2025-11-25")],
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let result = result_of(&text);
    let tools = result["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), TOOL_COUNT);
    harness.shutdown().await;
    world.shutdown().await;
}

/// Modern `server/discover` is answered complete, listing the supported versions.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn modern_discover_is_complete_and_lists_the_supported_versions() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = modern_request(1, "server/discover", json!({}));
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &body,
            &modern_headers("server/discover"),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let result = result_of(&text);
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"], json!(SUPPORTED));
    assert_eq!(
        result["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    harness.shutdown().await;
    world.shutdown().await;
}

/// Modern requests are tolerated without the `MCP-Protocol-Version` and `Mcp-Method` headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn modern_request_without_method_headers_is_still_answered() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = modern_request(1, "server/discover", json!({}));
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(result_of(&text)["resultType"], "complete");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A modern `whoami` reports the HTTP transport and server-held signing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn modern_whoami_reports_http_transport_and_server_held_signing() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = modern_request(1, "tools/call", json!({"name": "whoami", "arguments": {}}));
    let (status, _, text) = harness
        .post(Some(harness.token(0)), &body, &modern_headers("tools/call"))
        .await;
    assert_eq!(status, 200, "{text}");
    let result = result_of(&text);
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["structuredContent"]["signing"], "server-held");
    assert_eq!(result["structuredContent"]["transport"], "http");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A modern request with a version outside the modern list is 400 -32022, listing what is
/// supported and echoing what was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unsupported_modern_version_is_400_with_the_supported_list() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = rpc(
        1,
        "tools/list",
        json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "1999-01-01",
            "io.modelcontextprotocol/clientCapabilities": {},
        }}),
    );
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 400, "{text}");
    let error = json_body(&text)["error"].clone();
    assert_eq!(error["code"], -32022);
    assert_eq!(error["data"]["supported"], json!(SUPPORTED));
    assert_eq!(error["data"]["requested"], "1999-01-01");
    harness.shutdown().await;
    world.shutdown().await;
}

/// The `MCP-Protocol-Version` header must agree with the modern `_meta` version: 400 -32020.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn protocol_version_header_must_match_the_modern_meta() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = modern_request(1, "tools/list", json!({}));
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &body,
            &[("MCP-Protocol-Version", "2025-11-25")],
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(json_body(&text)["error"]["code"], -32020);
    harness.shutdown().await;
    world.shutdown().await;
}

/// The `Mcp-Method` header must agree with the body method: 400 -32020.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn mcp_method_header_must_match_the_body_method() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = modern_request(1, "tools/list", json!({}));
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &body,
            &[("Mcp-Method", "tools/call")],
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(json_body(&text)["error"]["code"], -32020);
    harness.shutdown().await;
    world.shutdown().await;
}

/// A legacy `MCP-Protocol-Version` that is neither legacy nor modern is 400 -32022.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unknown_legacy_protocol_header_is_400_unsupported_version() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "tools/list", json!({})),
            &[("MCP-Protocol-Version", "1999-01-01")],
        )
        .await;
    assert_eq!(status, 400, "{text}");
    let error = json_body(&text)["error"].clone();
    assert_eq!(error["code"], -32022);
    assert_eq!(error["data"]["requested"], "1999-01-01");
    harness.shutdown().await;
    world.shutdown().await;
}

/// An unknown request method is 404 with -32601.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unknown_method_is_404_method_not_found() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "resources/list", json!({})),
            &[],
        )
        .await;
    assert_eq!(status, 404, "{text}");
    assert_eq!(json_body(&text)["error"]["code"], -32601);
    harness.shutdown().await;
    world.shutdown().await;
}

/// Malformed JSON is 400 -32700 with a null id.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn malformed_json_is_400_parse_error_with_null_id() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            "{\"jsonrpc\": \"2.0\", \"id\":",
            &[],
        )
        .await;
    assert_eq!(status, 400, "{text}");
    let response = json_body(&text);
    assert_eq!(response["error"]["code"], -32700);
    assert!(response["id"].is_null(), "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A notification is accepted with 202 and no body.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn notification_is_202_with_an_empty_body() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body =
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}).to_string();
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 202);
    assert!(text.is_empty(), "{text:?}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// No Authorization header is 401 with a Bearer challenge and the JSON error.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn missing_credentials_are_401_with_a_bearer_challenge() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, headers, text) = harness.post(None, &rpc(1, "ping", json!({})), &[]).await;
    assert_eq!(status, 401, "{text}");
    assert_eq!(
        header_text(&headers, "www-authenticate"),
        Some("Bearer realm=\"agora\"")
    );
    assert_eq!(json_body(&text)["error"], "unauthorized");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A bearer token that matches no entry is 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unknown_bearer_token_is_401() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let forged = format!("agora_{}", "A".repeat(43));
    let (status, _, text) = harness
        .post(Some(forged.as_str()), &rpc(1, "ping", json!({})), &[])
        .await;
    assert_eq!(status, 401, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A foreign Origin is 403, even with a valid token.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn foreign_origin_is_403_even_with_a_valid_token() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "ping", json!({})),
            &[("Origin", "http://evil.example")],
        )
        .await;
    assert_eq!(status, 403, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A loopback Origin with any port (the Vite dev server) is allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn loopback_origin_with_a_port_is_allowed() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "ping", json!({})),
            &[("Origin", "http://localhost:5173")],
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(result_of(&text), json!({}));
    harness.shutdown().await;
    world.shutdown().await;
}

/// A Host header that is not loopback is 403 (DNS rebinding guard), even with a valid token.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn foreign_host_header_is_403() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "ping", json!({})),
            &[("Host", "evil.example")],
        )
        .await;
    assert_eq!(status, 403, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// GET and DELETE on `/mcp` are 405 and name the one allowed method.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_and_delete_are_405_with_allow_post() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let response = timeout(
            WAIT,
            client
                .request(method.clone(), harness.url())
                .bearer_auth(harness.token(0))
                .send(),
        )
        .await
        .expect("the server answers in time")
        .expect("the request is sent");
        assert_eq!(response.status(), 405, "{method}");
        assert_eq!(
            response
                .headers()
                .get("allow")
                .and_then(|v| v.to_str().ok()),
            Some("POST"),
            "{method}"
        );
    }
    harness.shutdown().await;
    world.shutdown().await;
}

/// A body that is not `application/json` is 415.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn non_json_content_type_is_415() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "ping", json!({})),
            &[("Content-Type", "text/plain")],
        )
        .await;
    assert_eq!(status, 415, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// An Accept header that names neither JSON, event streams nor `*/*` is 406.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unacceptable_accept_header_is_406() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            Some(harness.token(0)),
            &rpc(1, "ping", json!({})),
            &[("Accept", "text/html")],
        )
        .await;
    assert_eq!(status, 406, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A raw `POST /mcp` connection with a valid token, for tests that must control how much of
/// the body is written. `head_extra` is the framing header (`Content-Length` or
/// `Transfer-Encoding`).
async fn raw_post(harness: &HttpHarness, head_extra: &str) -> TcpStream {
    let addr = harness
        .url()
        .trim_start_matches("http://")
        .trim_end_matches("/mcp")
        .to_owned();
    let mut stream = timeout(WAIT, TcpStream::connect(&addr))
        .await
        .expect("connect in time")
        .expect("connect to the server");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nAccept: application/json\r\n{head_extra}\
         Connection: close\r\n\r\n",
        harness.token(0)
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("send the headers");
    stream
}

/// A declared body over the 8 MiB cap is 413 before any of it is read. The request is written
/// by hand: the server answers on the headers alone and closes with the body unread, so a client
/// that uploads the whole body can see a reset instead of the reply (the race that made the
/// reqwest version of this test flaky). The 1 KiB prefix is all that is sent; a server that
/// buffered the body would wait for the other 8 MiB and the read would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn body_over_eight_mebibytes_is_413() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let mut stream = raw_post(
        &harness,
        &format!("Content-Length: {}\r\n", MAX_MESSAGE_BYTES + 1),
    )
    .await;
    // The server may already have replied and closed: a failed write is expected.
    let _ = stream.write_all(&[b'x'; 1024]).await;
    let reply = timeout(WAIT, read_head(&mut stream))
        .await
        .expect("the reply arrives in time");
    assert!(reply.starts_with("HTTP/1.1 413 "), "{reply}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A chunked body (no Content-Length) is counted while it is read, and refused with 413 once it
/// passes the cap. Exactly one byte over is sent, with the terminating chunk in the same write,
/// so the server has consumed the whole request when it replies and closes: no unread upload
/// is left to turn the close into a reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chunked_body_over_eight_mebibytes_is_413() {
    const PIECE: usize = 64 * 1024;
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let mut stream = raw_post(&harness, "Transfer-Encoding: chunked\r\n").await;
    let mut remaining = MAX_MESSAGE_BYTES + 1;
    let piece = vec![b'x'; PIECE];
    while remaining > 0 {
        let n = remaining.min(PIECE);
        let mut frame = format!("{n:x}\r\n").into_bytes();
        frame.extend_from_slice(&piece[..n]);
        frame.extend_from_slice(b"\r\n");
        remaining -= n;
        if remaining == 0 {
            frame.extend_from_slice(b"0\r\n\r\n");
        }
        // The server may reply and close as soon as the cap is passed: stop writing then.
        if stream.write_all(&frame).await.is_err() {
            break;
        }
    }
    let reply = timeout(WAIT, read_head(&mut stream))
        .await
        .expect("the reply arrives in time");
    assert!(reply.starts_with("HTTP/1.1 413 "), "{reply}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// A body of exactly the cap is read, not refused: it fails as a parse error instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn body_of_exactly_eight_mebibytes_is_read_not_refused() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let at_cap = "x".repeat(MAX_MESSAGE_BYTES);
    let (status, _, text) = harness.post(Some(harness.token(0)), &at_cap, &[]).await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(json_body(&text)["error"]["code"], -32700);
    harness.shutdown().await;
    world.shutdown().await;
}

/// Revoking a token refuses its next request with 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn revoked_token_is_401_at_its_next_request() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = rpc(1, "ping", json!({}));
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{text}");

    let path = token_file(&world);
    let entry = token_entry(&world, &harness, 0);
    let before = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("the token file has an mtime");
    assert!(
        tokens::revoke(&path, &entry.id).expect("revoke the token"),
        "the token was in the file"
    );
    let after = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("the token file has an mtime");
    assert_ne!(
        before, after,
        "the revoke rewrites the file, moving its mtime"
    );

    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 401, "{text}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// Over HTTP a chat attachment `path` is refused as a usage error, even for a readable file.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chat_attachment_with_a_path_is_refused_over_http() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"hello").expect("write the attachment");
    let path = file.to_str().expect("a UTF-8 path");
    let body = tool_call(
        1,
        "chat",
        json!({
            "channel": "c0",
            "text": "hi",
            "attachments": [{"name": "note.txt", "path": path}],
        }),
    );
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{text}");
    assert_tool_error(
        &result_of(&text),
        "usage",
        "filesystem paths are disabled over HTTP; use base64",
    );
    harness.shutdown().await;
    world.shutdown().await;
}

/// Over HTTP `fetch_file` refuses `save_to` as a usage error and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn fetch_file_save_to_is_refused_over_http() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let target = dir.path().join("blob.bin");
    let target_text = target.to_str().expect("a UTF-8 path");
    let body = tool_call(
        1,
        "fetch_file",
        json!({"hash": "0".repeat(64), "save_to": target_text}),
    );
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{text}");
    assert_tool_error(
        &result_of(&text),
        "usage",
        "filesystem paths are disabled over HTTP; use base64",
    );
    assert!(!target.exists(), "nothing is written over HTTP");
    harness.shutdown().await;
    world.shutdown().await;
}

/// Ten clients call `inbox` at once; each sees exactly its own channel's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_inbox_calls_each_see_only_their_own_message() {
    let world = World::start().await;
    let owner = world.owner().await;
    let harness = HttpHarness::start(&world, CLIENTS).await;
    bind_clients(&world, &owner, &harness, CLIENTS).await;
    for i in 0..CLIENTS {
        world
            .post(&owner, &channel_of(i), &format!("secret-{i}"))
            .await;
    }

    let shared = Arc::new(harness);
    let mut calls = JoinSet::new();
    for i in 0..CLIENTS {
        let shared = Arc::clone(&shared);
        let token = shared.token(i).to_owned();
        calls.spawn(async move {
            let (status, _, body) = shared
                .post(Some(token.as_str()), &tool_call(2, "inbox", json!({})), &[])
                .await;
            (i, status, body)
        });
    }
    let results = join_calls(&mut calls).await;

    let seen: BTreeMap<usize, Vec<String>> = results
        .iter()
        .map(|(i, result)| (*i, message_texts(result)))
        .collect();
    let expected: BTreeMap<usize, Vec<String>> = (0..CLIENTS)
        .map(|i| (i, vec![format!("secret-{i}")]))
        .collect();
    assert_eq!(seen, expected);

    let harness = Arc::into_inner(shared).expect("every call has finished");
    harness.shutdown().await;
    world.shutdown().await;
}

/// Ten concurrent `inbox_wait`s, each from a cursor past its own old message: each is woken by
/// the message posted to its channel and by no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_inbox_waits_each_wake_only_for_their_own_channel() {
    let world = World::start().await;
    let owner = world.owner().await;
    let harness = HttpHarness::start(&world, CLIENTS).await;
    bind_clients(&world, &owner, &harness, CLIENTS).await;
    let mut cursors = Vec::with_capacity(CLIENTS);
    for i in 0..CLIENTS {
        cursors.push(
            world
                .post(&owner, &channel_of(i), &format!("secret-{i}"))
                .await,
        );
    }

    let shared = Arc::new(harness);
    let mut waits = JoinSet::new();
    for (i, cursor) in cursors.iter().copied().enumerate() {
        let shared = Arc::clone(&shared);
        let token = shared.token(i).to_owned();
        waits.spawn(async move {
            let args = json!({"cursor": cursor, "timeout_secs": 5});
            let (status, _, body) = shared
                .post(Some(token.as_str()), &tool_call(2, "inbox_wait", args), &[])
                .await;
            (i, status, body)
        });
    }
    for i in 0..CLIENTS {
        world
            .post(&owner, &channel_of(i), &format!("wake-{i}"))
            .await;
    }
    let results = join_calls(&mut waits).await;

    for i in 0..CLIENTS {
        let result = results.get(&i).expect("every wait answered");
        assert_eq!(
            message_texts(result),
            vec![format!("wake-{i}")],
            "client {i}"
        );
        assert_eq!(
            result["structuredContent"]["timed_out"], false,
            "client {i}"
        );
    }

    let harness = Arc::into_inner(shared).expect("every wait has finished");
    harness.shutdown().await;
    world.shutdown().await;
}

/// An `inbox_wait` with a progress token and an SSE Accept, but no progress before it answers, is
/// answered as plain JSON: its first message is already the final response. The harness engine
/// progresses every 10 s, so a 2 s wait sends no progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn inbox_wait_over_sse_without_progress_is_answered_as_json() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let body = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "inbox_wait",
            "arguments": {"timeout_secs": 2},
            "_meta": {"progressToken": "p"},
        },
    })
    .to_string();
    let (status, headers, text) = harness
        .post(
            Some(harness.token(0)),
            &body,
            &[("Accept", "application/json, text/event-stream")],
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        header_text(&headers, "content-type"),
        Some("application/json")
    );
    assert!(header_text(&headers, "x-accel-buffering").is_none());
    let response = json_body(&text);
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["structuredContent"]["timed_out"], true);
    harness.shutdown().await;
    world.shutdown().await;
}

/// Progress events for a progress token come before the final response, in increasing order.
/// The harness fixes a 10 s interval, so this test runs its own server with a 50 ms interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn inbox_wait_streams_progress_before_the_final_response() {
    let world = World::start().await;
    let (token, _entry) =
        tokens::add(&token_file(&world), "progress", now_ms()).expect("create a bearer token");
    let listener = http::bind("127.0.0.1:0".parse().expect("address"), false)
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local address");
    let options = HttpOptions {
        paths: world.paths().clone(),
        token_file: token_file(&world),
        allowed_origins: Vec::new(),
    };
    let config = EngineConfig {
        progress_interval_ms: 50,
        ..EngineConfig::http()
    };
    let cancel = CancellationToken::new();
    let serve = tokio::spawn(http::serve(
        listener,
        options,
        Engine::new(config),
        cancel.clone(),
    ));

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "inbox_wait",
            "arguments": {"timeout_secs": 1},
            "_meta": {"progressToken": "p"},
        },
    })
    .to_string();
    let response = timeout(
        WAIT,
        client
            .post(format!("http://{addr}/mcp"))
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body)
            .send(),
    )
    .await
    .expect("the server answers in time")
    .expect("the request is sent");
    assert_eq!(response.status(), 200);
    let text = timeout(WAIT, response.text())
        .await
        .expect("the stream ends in time")
        .expect("the stream is readable");
    drop(client);

    cancel.cancel();
    timeout(STOP_WAIT, serve)
        .await
        .expect("the server stops in time")
        .expect("the server task does not panic")
        .expect("serve returns Ok");

    let events = sse_events(&text);
    let (last, progress) = events.split_last().expect("at least the final response");
    assert_eq!(last["id"], 7);
    assert_eq!(last["result"]["structuredContent"]["timed_out"], true);
    assert!(
        !progress.is_empty(),
        "no progress before the response: {text}"
    );
    let mut previous = f64::NEG_INFINITY;
    for note in progress {
        assert_eq!(note["method"], "notifications/progress", "{note}");
        assert_eq!(note["params"]["progressToken"], "p", "{note}");
        let value = note["params"]["progress"]
            .as_f64()
            .expect("progress is a number");
        assert!(value > previous, "progress must increase: {note}");
        previous = value;
    }
    world.shutdown().await;
}

/// A client's `inbox_wait` started before a message lands in its channel returns that message
/// well before its 5 s timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn inbox_wait_returns_when_a_message_lands_in_its_channel() {
    let world = World::start().await;
    let owner = world.owner().await;
    let harness = HttpHarness::start(&world, 1).await;
    bind_clients(&world, &owner, &harness, 1).await;

    let shared = Arc::new(harness);
    let waiting = {
        let shared = Arc::clone(&shared);
        let token = shared.token(0).to_owned();
        let body = tool_call(3, "inbox_wait", json!({"timeout_secs": 5}));
        tokio::spawn(async move { shared.post(Some(token.as_str()), &body, &[]).await })
    };
    let started = Instant::now();
    world.post(&owner, &channel_of(0), "wake").await;
    let (status, _, text) = timeout(WAIT, waiting)
        .await
        .expect("the wait answers in time")
        .expect("the wait task does not panic");
    let elapsed = started.elapsed();

    assert_eq!(status, 200, "{text}");
    let result = result_of(&text);
    assert_eq!(message_texts(&result), vec!["wake".to_owned()]);
    assert_eq!(result["structuredContent"]["timed_out"], false);
    assert!(
        elapsed < WAKE_BOUND,
        "the wait returned after {elapsed:?}, not on the message"
    );

    let harness = Arc::into_inner(shared).expect("the wait has finished");
    harness.shutdown().await;
    world.shutdown().await;
}

/// The one origin the CORS tests allow.
const APP_ORIGIN: &str = "https://app.example";

/// A server of its own: the given engine config and allowed origins, and one token.
struct OwnServer {
    url: String,
    token: String,
    cancel: CancellationToken,
    serve: JoinHandle<Result<(), McpError>>,
}

impl OwnServer {
    async fn start(world: &World, config: EngineConfig, allowed_origins: Vec<String>) -> OwnServer {
        let token_file = token_file(world);
        let (token, _entry) =
            tokens::add(&token_file, "own", now_ms()).expect("create a bearer token");
        let listener = http::bind("127.0.0.1:0".parse().expect("address"), false)
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local address");
        let options = HttpOptions {
            paths: world.paths().clone(),
            token_file,
            allowed_origins,
        };
        let cancel = CancellationToken::new();
        let serve = tokio::spawn(http::serve(
            listener,
            options,
            Engine::new(config),
            cancel.clone(),
        ));
        OwnServer {
            url: format!("http://{addr}/mcp"),
            token,
            cancel,
            serve,
        }
    }

    /// A client with no proxy, so loopback requests go straight to the server.
    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client")
    }

    async fn stop(self) {
        self.cancel.cancel();
        timeout(STOP_WAIT, self.serve)
            .await
            .expect("the server stops in time")
            .expect("the server task does not panic")
            .expect("serve returns Ok");
    }
}

/// Sends `request`; returns its status, headers and body, each wait bounded.
async fn send(request: reqwest::RequestBuilder) -> (u16, HeaderMap, String) {
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
    (status, headers, body)
}

/// A bad Host is refused before authentication: a request with no token gets 403, not 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn bad_host_without_a_token_is_403_not_401() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let (status, _, text) = harness
        .post(
            None,
            &rpc(1, "ping", json!({})),
            &[("Host", "evil.example")],
        )
        .await;
    assert_eq!(status, 403, "{text}");
    assert_eq!(json_body(&text)["error"], "forbidden host");
    harness.shutdown().await;
    world.shutdown().await;
}

/// The size cap is read after authentication: an unauthenticated 9 MiB body is 401 and is
/// refused without being buffered. The request is written by hand: the server answers before it
/// reads the body and closes, so a client that uploads the whole body can see a reset instead of
/// the reply. This sends the headers and a 1 KiB prefix, then reads the status line.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn unauthenticated_nine_mebibyte_body_is_401() {
    const DECLARED: usize = 9 * 1024 * 1024;
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let addr = harness
        .url()
        .trim_start_matches("http://")
        .trim_end_matches("/mcp")
        .to_owned();
    let mut stream = timeout(WAIT, TcpStream::connect(&addr))
        .await
        .expect("connect in time")
        .expect("connect to the server");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {DECLARED}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("send the headers");
    // Once the server has replied it may close with the body unread: a failed write is expected.
    let _ = stream.write_all(&[b'x'; 1024]).await;
    let reply = timeout(WAIT, read_head(&mut stream))
        .await
        .expect("the reply arrives in time");
    assert!(reply.starts_with("HTTP/1.1 401 "), "{reply}");
    harness.shutdown().await;
    world.shutdown().await;
}

/// The status line and headers of a response, read up to the blank line that ends them, or to
/// the end of the connection: a reset after the reply does not lose it.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 512];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A client that drops an SSE stream mid-call (after its headers arrive) leaves the server
/// healthy: a request on a new connection is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn dropped_sse_stream_leaves_the_server_healthy() {
    let world = World::start().await;
    let config = EngineConfig {
        progress_interval_ms: 50,
        ..EngineConfig::http()
    };
    let server = OwnServer::start(&world, config, Vec::new()).await;
    let body = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "inbox_wait",
            "arguments": {"timeout_secs": 5},
            "_meta": {"progressToken": "p"},
        },
    })
    .to_string();
    let client = OwnServer::client();
    // The headers come with the first progress notification; the stream is then abandoned.
    let response = timeout(
        WAIT,
        client
            .post(&server.url)
            .bearer_auth(&server.token)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body)
            .send(),
    )
    .await
    .expect("the stream starts in time")
    .expect("the request is sent");
    assert_eq!(response.status(), 200);
    assert_eq!(
        header_text(response.headers(), "content-type"),
        Some("text/event-stream")
    );
    drop(response);
    drop(client);

    let fresh = OwnServer::client();
    let (status, _, text) = send(
        fresh
            .post(&server.url)
            .bearer_auth(&server.token)
            .header("content-type", "application/json")
            .body(rpc(1, "ping", json!({}))),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(json_body(&text)["result"], json!({}));
    drop(fresh);
    server.stop().await;
    world.shutdown().await;
}

/// A CORS preflight from an allowed origin is 204 with the CORS headers, and needs no token.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cors_preflight_from_an_allowed_origin_is_204() {
    let world = World::start().await;
    let server = OwnServer::start(&world, EngineConfig::http(), vec![APP_ORIGIN.to_owned()]).await;
    let client = OwnServer::client();
    let (status, headers, body) = send(
        client
            .request(reqwest::Method::OPTIONS, &server.url)
            .header("origin", APP_ORIGIN)
            .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                "access-control-request-headers",
                "authorization, content-type",
            ),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    assert!(body.is_empty(), "{body:?}");
    assert_eq!(
        header_text(&headers, "access-control-allow-origin"),
        Some(APP_ORIGIN)
    );
    assert_eq!(
        header_text(&headers, "access-control-allow-methods"),
        Some("POST")
    );
    assert_eq!(
        header_text(&headers, "access-control-allow-headers"),
        Some("authorization, content-type, accept, mcp-protocol-version, mcp-method")
    );
    assert_eq!(header_text(&headers, "access-control-max-age"), Some("600"));
    assert_eq!(header_text(&headers, "vary"), Some("Origin"));
    drop(client);
    server.stop().await;
    world.shutdown().await;
}

/// A CORS preflight from an origin that is not allowed is 403, with no CORS headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cors_preflight_from_another_origin_is_403() {
    let world = World::start().await;
    let server = OwnServer::start(&world, EngineConfig::http(), vec![APP_ORIGIN.to_owned()]).await;
    let client = OwnServer::client();
    let (status, headers, body) = send(
        client
            .request(reqwest::Method::OPTIONS, &server.url)
            .header("origin", "https://evil.example")
            .header(ACCESS_CONTROL_REQUEST_METHOD, "POST"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(header_text(&headers, "access-control-allow-origin").is_none());
    drop(client);
    server.stop().await;
    world.shutdown().await;
}

/// Every response to a request from an allowed origin carries the origin, 401 included.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn allowed_origin_is_echoed_on_every_response() {
    let world = World::start().await;
    let server = OwnServer::start(&world, EngineConfig::http(), vec![APP_ORIGIN.to_owned()]).await;
    let client = OwnServer::client();
    let (status, headers, body) = send(
        client
            .post(&server.url)
            .header("origin", APP_ORIGIN)
            .header("content-type", "application/json")
            .body(rpc(1, "ping", json!({}))),
    )
    .await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(
        header_text(&headers, "access-control-allow-origin"),
        Some(APP_ORIGIN)
    );
    assert_eq!(header_text(&headers, "vary"), Some("Origin"));
    drop(client);
    server.stop().await;
    world.shutdown().await;
}

/// Shutdown ends a request in flight on a keep-alive connection: within 4 s it answers 503 or its
/// connection closes, and the server stops. Either outcome is correct; a hang is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn shutdown_ends_an_in_flight_request_on_a_keep_alive_connection() {
    let world = World::start().await;
    let server = OwnServer::start(&world, EngineConfig::http(), Vec::new()).await;
    let client = OwnServer::client();
    // This exchange opens the keep-alive connection that the next request reuses.
    let (status, _, text) = send(
        client
            .post(&server.url)
            .bearer_auth(&server.token)
            .header("content-type", "application/json")
            .body(rpc(1, "ping", json!({}))),
    )
    .await;
    assert_eq!(status, 200, "{text}");

    let request = client
        .post(&server.url)
        .bearer_auth(&server.token)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(tool_call(2, "inbox_wait", json!({"timeout_secs": 30})))
        .send();
    let in_flight = tokio::spawn(request);
    server.cancel.cancel();
    let outcome = timeout(Duration::from_secs(4), in_flight)
        .await
        .expect("the request ends within 4 s")
        .expect("the request task does not panic");
    if let Ok(response) = outcome {
        assert_eq!(response.status().as_u16(), 503);
    }
    drop(client);
    server.stop().await;
    world.shutdown().await;
}

/// `null` and `*` are refused as allowed origins: `serve` returns a configuration error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn null_and_wildcard_allowed_origins_are_a_config_error() {
    let dir = tempfile::tempdir().expect("a temp dir");
    for origin in ["null", "*"] {
        let listener = http::bind("127.0.0.1:0".parse().expect("address"), false)
            .await
            .expect("bind 127.0.0.1:0");
        let options = HttpOptions {
            paths: Paths::new(dir.path()),
            token_file: dir.path().join("mcp-tokens.json"),
            allowed_origins: vec![origin.to_owned()],
        };
        let result = timeout(
            WAIT,
            http::serve(
                listener,
                options,
                Engine::new(EngineConfig::http()),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("serve refuses the configuration in time");
        assert!(
            matches!(result, Err(McpError::Config(_))),
            "{origin:?}: {result:?}"
        );
    }
}
