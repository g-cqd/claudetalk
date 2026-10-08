//! Behavior of the chat tools (chat, attach, read, react, fetch_file, inbox, inbox_wait, ack) and
//! the diagnostics tools (whoami, discover, delivery_status), driven as `tools/call` over a stdio
//! session of an in-process daemon.
//!
//! Other members are read in-process through `api::chat`, so each assertion sees what a peer
//! sees, not what the stdio session reports about itself.

mod common;

use agora_client::Ctx;
use agora_client::api::chat::{self, AttachmentInput, ChatParams, InboxParams};
use agora_client::api::org::{self, ChannelCreateParams};
use agora_crypto::pseudonym;
use agora_mcp::engine::EngineConfig;
use agora_mcp::framing::UNTRUSTED_NOTICE;
use agora_store::hex;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{StdioClient, World};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::time::Instant;

/// Bytes that are not UTF-8 and contain a newline, so a text round trip would corrupt them.
const BLOB: &[u8] = &[0x00, 0xff, 0x10, b'\n', 0x7f, 0x80];

/// Channel push off, filesystem paths on, progress every 10 s.
fn legacy_config() -> EngineConfig {
    EngineConfig {
        channel_push: false,
        ..EngineConfig::stdio()
    }
}

/// A stdio session for the registered identity `seed`, initialized as `client_name`.
async fn session(world: &World, seed: u8, cfg: EngineConfig, client_name: &str) -> StdioClient {
    let mut client = StdioClient::start(world, world.ctx(seed), cfg).await;
    client.initialize(client_name).await;
    client
}

fn pk_of(ctx: &Ctx) -> String {
    hex(&ctx.identity.public())
}

fn assert_tool_ok(result: &Value) {
    assert_eq!(result["isError"], false, "expected success, got {result}");
}

fn assert_tool_error(result: &Value, code: &str) {
    assert_eq!(
        result["isError"], true,
        "expected a tool error, got {result}"
    );
    assert_eq!(
        result["structuredContent"]["error"]["code"], code,
        "{result}"
    );
}

/// The unacked messages `reader` sees across its channels, from the start, as api JSON.
async fn inbox_messages(reader: &Ctx) -> Vec<Value> {
    let out = chat::inbox(
        reader,
        InboxParams {
            wait: None,
            after: 0,
            limit: 50,
        },
    )
    .await
    .expect("read the inbox in-process");
    out.json["messages"]
        .as_array()
        .cloned()
        .expect("inbox has a messages array")
}

/// The first attachment of the newest message `reader` sees, as (hash, name).
async fn newest_attachment(reader: &Ctx) -> (String, String) {
    let messages = inbox_messages(reader).await;
    let newest = messages
        .last()
        .expect("a message is waiting for the reader");
    let attachment = &newest["attachments"][0];
    (
        attachment["hash"]
            .as_str()
            .expect("attachment has a hash")
            .to_owned(),
        attachment["name"]
            .as_str()
            .expect("attachment has a name")
            .to_owned(),
    )
}

/// Post `bytes` as one attachment named `name` from `from`, in-process.
async fn post_blob(from: &Ctx, channel: &str, name: &str, bytes: &[u8]) {
    chat::chat(
        from,
        ChatParams {
            channel: channel.to_owned(),
            text: "blob".to_owned(),
            thread_of: None,
            attachments: vec![AttachmentInput {
                name: name.to_owned(),
                bytes: bytes.to_vec(),
            }],
        },
    )
    .await
    .expect("post the attachment in-process");
}

/// Read from `client` until the response to request `id` arrives, bounded by `within`. Returns
/// the notifications read on the way and the response's `result`.
async fn await_result(client: &mut StdioClient, id: i64, within: Duration) -> (Vec<Value>, Value) {
    let collect = async {
        let mut notes = Vec::new();
        loop {
            let msg = client.read_message(within).await.unwrap_or_else(|| {
                panic!("no message within {within:?} while waiting for id {id}")
            });
            match msg.get("id") {
                None => notes.push(msg),
                Some(got) if *got == json!(id) => {
                    let result = msg
                        .get("result")
                        .cloned()
                        .unwrap_or_else(|| panic!("request {id} failed: {msg}"));
                    return (notes, result);
                }
                Some(got) => panic!("unexpected response id {got} while waiting for {id}"),
            }
        }
    };
    tokio::time::timeout(within, collect)
        .await
        .unwrap_or_else(|_| panic!("request {id} did not finish within {within:?}"))
}

/// Read until the first progress notification arrives. Progress is sent only while the tool
/// call `id` is still waiting, so this confirms the wait is parked. Panics if the response
/// arrives first, because then the wait never parked.
async fn await_first_progress(client: &mut StdioClient, id: i64, within: Duration) {
    let first = async {
        loop {
            let msg = client.read_message(within).await.unwrap_or_else(|| {
                panic!("no message within {within:?} while waiting for id {id}")
            });
            if msg.get("id").is_some() {
                panic!("request {id} finished before any progress tick, so it never parked: {msg}");
            }
            if msg["method"] == "notifications/progress" {
                return;
            }
        }
    };
    tokio::time::timeout(within, first)
        .await
        .unwrap_or_else(|_| panic!("request {id} sent no progress within {within:?}"));
}

/// Call `discover` until the caller's own key is listed as online, at most 2 s.
async fn discover_until_online(client: &mut StdioClient, caller_pk: &str) -> Value {
    let attempts = async {
        loop {
            let result = client.call_tool("discover", json!({})).await;
            let listed = result["structuredContent"]["online"]
                .as_array()
                .is_some_and(|online| online.iter().any(|entry| entry["pk"] == caller_pk));
            if listed {
                return result;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), attempts)
        .await
        .expect("the caller is listed as online within 2 s")
}

/// The channel object with `id` in a discover result, if listed.
fn listed_channel<'a>(result: &'a Value, id: &str) -> Option<&'a Value> {
    result["structuredContent"]["channels"]
        .as_array()?
        .iter()
        .find(|channel| channel["id"] == id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_posts_text_that_another_member_reads_in_its_inbox() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("chat", json!({"channel": "general", "text": "hello team"}))
        .await;

    assert_tool_ok(&result);
    let posted = &result["structuredContent"];
    assert!(
        posted["seq"].as_i64().is_some_and(|seq| seq > 0),
        "{posted}"
    );
    assert_eq!(posted["channel"], "general");
    let messages = inbox_messages(&reader).await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["seq"], posted["seq"]);
    assert_eq!(messages[0]["id"], posted["id"]);
    assert_eq!(messages[0]["text"], "hello team");
    assert_eq!(messages[0]["sender"], pk_of(&poster));
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_to_a_channel_the_caller_is_not_in_is_refused_and_stores_nothing() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.agent(4).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 4, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("chat", json!({"channel": "general", "text": "let me in"}))
        .await;

    assert_tool_error(&result, "not_member");
    assert!(inbox_messages(&member).await.is_empty());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_base64_attachment_round_trips_through_fetch_file() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let encoded = STANDARD.encode(BLOB);

    let posted = client
        .call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "see attached",
                "attachments": [{"name": "notes.bin", "base64": encoded}],
            }),
        )
        .await;
    assert_tool_ok(&posted);
    let (hash, name) = newest_attachment(&reader).await;
    assert_eq!(name, "notes.bin");
    let fetched = client.call_tool("fetch_file", json!({"hash": hash})).await;

    assert_tool_ok(&fetched);
    assert_eq!(fetched["structuredContent"]["base64"], encoded.as_str());
    assert_eq!(fetched["structuredContent"]["bytes"], BLOB.len());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_attachment_is_read_from_disk_and_round_trips() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("report.bin");
    std::fs::write(&file, BLOB).expect("write the attachment file");
    let path = file.to_str().expect("utf-8 path");

    let posted = client
        .call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "report",
                "attachments": [{"name": "report.bin", "path": path}],
            }),
        )
        .await;
    assert_tool_ok(&posted);
    let (hash, name) = newest_attachment(&reader).await;
    assert_eq!(name, "report.bin");
    let fetched = client.call_tool("fetch_file", json!({"hash": hash})).await;

    assert_tool_ok(&fetched);
    assert_eq!(
        fetched["structuredContent"]["base64"],
        STANDARD.encode(BLOB).as_str()
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_attachment_with_both_path_and_base64_is_a_usage_error() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("both.bin");
    std::fs::write(&file, BLOB).expect("write the attachment file");
    let path = file.to_str().expect("utf-8 path");

    let result = client
        .call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "ambiguous",
                "attachments": [{"name": "both.bin", "path": path, "base64": STANDARD.encode(BLOB)}],
            }),
        )
        .await;

    assert_tool_error(&result, "usage");
    assert!(inbox_messages(&reader).await.is_empty());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_attachment_is_refused_when_paths_are_disabled() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let cfg = EngineConfig {
        allow_paths: false,
        ..legacy_config()
    };
    let mut client = session(&world, 2, cfg, "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("secret.bin");
    std::fs::write(&file, BLOB).expect("write the attachment file");
    let path = file.to_str().expect("utf-8 path");

    let result = client
        .call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "from disk",
                "attachments": [{"name": "secret.bin", "path": path}],
            }),
        )
        .await;

    assert_tool_error(&result, "usage");
    assert!(inbox_messages(&reader).await.is_empty());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_attachment_inside_the_agora_home_is_refused() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let file = world.paths().home.join("identity-copy.bin");
    std::fs::write(&file, BLOB).expect("write a file inside the home");
    let path = file.to_str().expect("utf-8 path");

    let result = client
        .call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "from the home",
                "attachments": [{"name": "identity-copy.bin", "path": path}],
            }),
        )
        .await;

    assert_tool_error(&result, "usage");
    let message = result["structuredContent"]["error"]["message"]
        .as_str()
        .expect("an error message");
    assert!(
        message.contains("refusing to attach files from the agora home"),
        "{message}"
    );
    assert!(inbox_messages(&reader).await.is_empty());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_attachment_that_is_a_fifo_is_refused_without_hanging() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let pipe = dir.path().join("pipe.bin");
    let made = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo failed");
    let path = pipe.to_str().expect("utf-8 path");

    // A FIFO without a writer blocks a plain open; the refusal must come back at once.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.call_tool(
            "chat",
            json!({
                "channel": "general",
                "text": "from a pipe",
                "attachments": [{"name": "pipe.bin", "path": path}],
            }),
        ),
    )
    .await
    .expect("refusing a FIFO must not block the call");

    assert_tool_error(&result, "usage");
    let message = result["structuredContent"]["error"]["message"]
        .as_str()
        .expect("an error message");
    assert!(message.ends_with("is not a regular file"), "{message}");
    assert!(inbox_messages(&reader).await.is_empty());
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_posts_one_base64_file_with_the_default_empty_text() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&poster, &reader]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool(
            "attach",
            json!({
                "channel": "general",
                "name": "data.bin",
                "base64": STANDARD.encode(BLOB),
            }),
        )
        .await;

    assert_tool_ok(&result);
    let messages = inbox_messages(&reader).await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["text"], "");
    let attachments = messages[0]["attachments"]
        .as_array()
        .expect("attachments array");
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0]["name"], "data.bin");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_with_base64_that_does_not_decode_is_a_usage_error() {
    let world = World::start().await;
    let owner = world.owner().await;
    let poster = world.agent(2).await;
    world.channel(&owner, "general", &[&poster]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool(
            "attach",
            json!({"channel": "general", "name": "bad.bin", "base64": "not base64!!"}),
        )
        .await;

    assert_tool_error(&result, "usage");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_frames_each_message_and_escapes_its_body() {
    let world = World::start().await;
    let owner = world.owner().await;
    let reader = world.agent(3).await;
    world.channel(&owner, "general", &[&reader]).await;
    let seq = world
        .post(&owner, "general", "<b>hi</b> & \"quotes\"")
        .await;
    let mut client = session(&world, 3, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("read", json!({"channel": "general"}))
        .await;

    assert_tool_ok(&result);
    let structured = &result["structuredContent"];
    assert_eq!(structured["notice"], UNTRUSTED_NOTICE);
    let messages = structured["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "{structured}");
    assert_eq!(messages[0]["seq"], seq);
    assert_eq!(messages[0]["sender"], pk_of(&owner));
    assert_eq!(
        messages[0]["pseudonym"],
        pseudonym(&owner.identity.public())
    );
    assert_eq!(messages[0]["verified"], true);
    assert_eq!(messages[0]["epoch"], 0);
    let text = result["content"][0]["text"].as_str().expect("text output");
    assert!(text.contains("<untrusted-message"), "{text}");
    assert!(
        text.contains("&lt;b&gt;hi&lt;/b&gt; &amp; &quot;quotes&quot;"),
        "{text}"
    );
    assert!(!text.contains("<b>hi</b>"), "{text}");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_on_a_channel_the_caller_is_not_in_is_refused() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.agent(4).await;
    world.channel(&owner, "general", &[&member]).await;
    world.post(&owner, "general", "private").await;
    let mut client = session(&world, 4, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("read", json!({"channel": "general"}))
        .await;

    assert_tool_error(&result, "not_member");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_lists_unread_messages_with_the_cursor_of_the_newest() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let seq = world.post(&owner, "general", "first").await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("inbox", json!({})).await;

    assert_tool_ok(&result);
    let messages = result["structuredContent"]["messages"]
        .as_array()
        .expect("messages array");
    let seqs: Vec<&Value> = messages.iter().map(|m| &m["seq"]).collect();
    assert_eq!(seqs, vec![&json!(seq)]);
    assert_eq!(result["structuredContent"]["cursor"], seq);
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_with_a_non_numeric_cursor_is_invalid_arguments() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("inbox", json!({"after": "5"})).await;

    assert_tool_error(&result, "invalid_arguments");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_marks_the_channel_read_so_the_inbox_empties() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let seq = world.post(&owner, "general", "handle me").await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let acked = client
        .call_tool("ack", json!({"channel": "general", "seq": seq}))
        .await;

    assert_tool_ok(&acked);
    let inbox = client.call_tool("inbox", json!({})).await;
    assert_eq!(inbox["structuredContent"]["messages"], json!([]), "{inbox}");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_on_a_channel_the_caller_is_not_in_is_refused() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.agent(4).await;
    world.channel(&owner, "general", &[&member]).await;
    let seq = world.post(&owner, "general", "not yours").await;
    let mut client = session(&world, 4, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("ack", json!({"channel": "general", "seq": seq}))
        .await;

    assert_tool_error(&result, "not_member");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_returns_at_once_when_unread_messages_exist() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    world.post(&owner, "general", "already waiting").await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let started = Instant::now();
    let result = client
        .call_tool("inbox_wait", json!({"cursor": 0, "timeout_secs": 3}))
        .await;

    assert_tool_ok(&result);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "an inbox with unread messages must not wait out its timeout"
    );
    assert_eq!(result["structuredContent"]["timed_out"], false);
    assert_eq!(
        result["structuredContent"]["messages"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_wakes_when_another_agent_posts_into_its_channel() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let cfg = EngineConfig {
        progress_interval_ms: 20,
        ..legacy_config()
    };
    let mut client = session(&world, 2, cfg, "mock-client").await;
    client
        .send_raw(
            &json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/call",
                "params": {
                    "name": "inbox_wait",
                    "arguments": {"cursor": 0, "timeout_secs": 5},
                    "_meta": {"progressToken": "wake"},
                },
            })
            .to_string(),
        )
        .await;

    // Progress is sent only while the wait is parked, so this post lands in a waiting inbox.
    await_first_progress(&mut client, 42, Duration::from_secs(4)).await;
    world.post(&owner, "general", "wake up").await;
    let (_, result) = await_result(&mut client, 42, Duration::from_secs(4)).await;

    assert_tool_ok(&result);
    assert_eq!(result["structuredContent"]["timed_out"], false);
    let messages = result["structuredContent"]["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages.len(), 1, "{result}");
    assert_eq!(messages[0]["text"], "wake up");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_times_out_with_no_messages_and_the_cursor_it_was_given() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let started = Instant::now();
    let result = client
        .call_tool("inbox_wait", json!({"cursor": 7, "timeout_secs": 1}))
        .await;

    assert_tool_ok(&result);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the wait must hold for its whole timeout before it reports the timeout"
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["timed_out"], true);
    assert_eq!(structured["messages"], json!([]));
    assert_eq!(structured["cursor"], 7);
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_sends_progress_under_its_token_before_the_result() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let cfg = EngineConfig {
        progress_interval_ms: 200,
        ..legacy_config()
    };
    let mut client = session(&world, 2, cfg, "mock-client").await;
    client
        .send_raw(
            &json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "tools/call",
                "params": {
                    "name": "inbox_wait",
                    "arguments": {"cursor": 0, "timeout_secs": 2},
                    "_meta": {"progressToken": "p1"},
                },
            })
            .to_string(),
        )
        .await;

    let (notes, result) = await_result(&mut client, 77, Duration::from_secs(5)).await;

    assert_tool_ok(&result);
    assert_eq!(result["structuredContent"]["timed_out"], true);
    let progress: Vec<&Value> = notes
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .collect();
    assert!(
        progress.len() >= 2,
        "expected at least 2 progress notifications, got {}",
        progress.len()
    );
    assert!(
        progress
            .iter()
            .all(|n| n["params"]["progressToken"] == "p1"),
        "{progress:?}"
    );
    assert!(
        progress
            .iter()
            .all(|n| n["params"]["total"].as_f64() == Some(2.0)),
        "{progress:?}"
    );
    let values: Vec<f64> = progress
        .iter()
        .filter_map(|n| n["params"]["progress"].as_f64())
        .collect();
    assert_eq!(values.len(), progress.len(), "{progress:?}");
    assert!(
        values.windows(2).all(|pair| pair[0] < pair[1]),
        "progress must strictly increase: {values:?}"
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_with_a_string_timeout_is_invalid_arguments() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("inbox_wait", json!({"timeout_secs": "5"}))
        .await;

    assert_tool_error(&result, "invalid_arguments");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn react_adds_then_removes_a_reaction_on_a_message() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let posted = client
        .call_tool("chat", json!({"channel": "general", "text": "react to me"}))
        .await;
    let id = posted["structuredContent"]["id"].clone();

    let added = client
        .call_tool("react", json!({"target": id, "emoji": "+1"}))
        .await;
    let removed = client
        .call_tool(
            "react",
            json!({"target": id, "emoji": "+1", "remove": true}),
        )
        .await;

    assert_tool_ok(&added);
    assert_eq!(added["structuredContent"]["remove"], false, "{added}");
    assert_eq!(added["structuredContent"]["kind"], "message");
    assert_eq!(added["structuredContent"]["target"], id);
    assert_tool_ok(&removed);
    assert_eq!(removed["structuredContent"]["remove"], true, "{removed}");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn react_with_an_unknown_kind_is_a_usage_error() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool(
            "react",
            json!({"target": "0".repeat(64), "emoji": "+1", "kind": "bogus"}),
        )
        .await;

    assert_tool_error(&result, "usage");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_file_saves_a_new_path_as_a_private_file_with_the_blob_bytes() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    post_blob(&owner, "general", "blob.bin", BLOB).await;
    let (hash, _) = newest_attachment(&member).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("saved.bin");
    let saved_to = target.to_str().expect("utf-8 path");

    let result = client
        .call_tool("fetch_file", json!({"hash": hash, "save_to": saved_to}))
        .await;

    assert_tool_ok(&result);
    assert_eq!(result["structuredContent"]["saved_to"], saved_to);
    assert_eq!(result["structuredContent"]["bytes"], BLOB.len());
    assert_eq!(std::fs::read(&target).expect("read the saved file"), BLOB);
    let mode = std::fs::metadata(&target)
        .expect("stat the saved file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_file_never_overwrites_an_existing_path() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    post_blob(&owner, "general", "blob.bin", BLOB).await;
    let (hash, _) = newest_attachment(&member).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let existing = dir.path().join("keep.bin");
    std::fs::write(&existing, b"keep me").expect("write the existing file");
    let path = existing.to_str().expect("utf-8 path");

    let result = client
        .call_tool("fetch_file", json!({"hash": hash, "save_to": path}))
        .await;

    assert_tool_error(&result, "usage");
    assert_eq!(std::fs::read(&existing).expect("read the file"), b"keep me");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_file_with_an_unknown_hash_is_an_error() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("fetch_file", json!({"hash": "0".repeat(64)}))
        .await;

    assert_tool_error(&result, "not_found");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_file_save_to_is_refused_when_paths_are_disabled() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let cfg = EngineConfig {
        allow_paths: false,
        ..legacy_config()
    };
    let mut client = session(&world, 2, cfg, "mock-client").await;
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("refused.bin");
    let saved_to = target.to_str().expect("utf-8 path");

    let result = client
        .call_tool(
            "fetch_file",
            json!({"hash": "0".repeat(64), "save_to": saved_to}),
        )
        .await;

    assert_tool_error(&result, "usage");
    assert!(!target.exists(), "a refused save must not create the file");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_reports_the_signing_key_and_an_unknown_harness_for_mock_client() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("whoami", json!({})).await;

    assert_tool_ok(&result);
    let structured = &result["structuredContent"];
    assert_eq!(structured["pubkey"], pk_of(&member));
    assert_eq!(structured["client"]["name"], "mock-client");
    assert_eq!(structured["signing"], "local");
    assert_eq!(structured["harness"], "unknown");
    assert_eq!(structured["ladder"], json!(["wait_tool"]));
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_for_claude_code_with_channel_push_starts_the_ladder_with_channel_push() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, EngineConfig::stdio(), "claude-code").await;

    let result = client.call_tool("whoami", json!({})).await;

    assert_tool_ok(&result);
    let structured = &result["structuredContent"];
    assert_eq!(structured["client"]["name"], "claude-code");
    assert_eq!(structured["harness"], "claude_code");
    assert_eq!(structured["push_enabled"], true);
    assert_eq!(structured["ladder"][0], "channel_push");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_with_an_unknown_argument_is_invalid_arguments() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("whoami", json!({"verbose": true})).await;

    assert_tool_error(&result, "invalid_arguments");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discover_lists_only_the_channels_the_caller_belongs_to() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    let other = world.agent(3).await;
    world.channel(&owner, "general", &[&member]).await;
    world.channel(&owner, "secret", &[&other]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("discover", json!({})).await;

    assert_tool_ok(&result);
    let ids: Vec<&Value> = result["structuredContent"]["channels"]
        .as_array()
        .expect("channels array")
        .iter()
        .map(|c| &c["id"])
        .collect();
    assert_eq!(ids, vec![&json!("general")], "{result}");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discover_lists_members_with_pseudonym_role_and_online_flag() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    let other = world.agent(3).await;
    world.channel(&owner, "general", &[&member, &other]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = discover_until_online(&mut client, &pk_of(&member)).await;

    assert_tool_ok(&result);
    let general = listed_channel(&result, "general").expect("general is listed");
    let members = general["members"].as_array().expect("members array");
    assert_eq!(members.len(), 3, "{general}");
    let expect = |ctx: &Ctx, role: &str, online: bool| {
        json!({
            "pk": pk_of(ctx),
            "pseudonym": pseudonym(&ctx.identity.public()),
            "role": role,
            "online": online,
        })
    };
    assert!(
        members.contains(&expect(&owner, "admin", false)),
        "{general}"
    );
    assert!(
        members.contains(&expect(&member, "member", true)),
        "{general}"
    );
    assert!(
        members.contains(&expect(&other, "member", false)),
        "{general}"
    );
    assert_eq!(
        result["structuredContent"]["online"],
        json!([{
            "pk": pk_of(&member),
            "pseudonym": pseudonym(&member.identity.public()),
        }]),
        "{result}"
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discover_with_a_non_boolean_include_members_is_invalid_arguments() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("discover", json!({"include_members": "yes"}))
        .await;

    assert_tool_error(&result, "invalid_arguments");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_status_reports_the_tracker_shape_for_a_session_with_no_messages() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client.call_tool("delivery_status", json!({})).await;

    assert_tool_ok(&result);
    let status = &result["structuredContent"];
    assert_eq!(status["ladder"], json!(["wait_tool"]));
    assert_eq!(status["push_enabled"], false);
    assert_eq!(status["pending"], json!([]));
    assert_eq!(status["escalations"], json!([]));
    assert_eq!(status["pushed"], 0);
    assert_eq!(status["coalesced"], 0);
    assert_eq!(status["acked"], 0);
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_status_with_an_unknown_argument_is_invalid_arguments() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    world.channel(&owner, "general", &[&member]).await;
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;

    let result = client
        .call_tool("delivery_status", json!({"verbose": true}))
        .await;

    assert_tool_error(&result, "invalid_arguments");
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_channel_text_posted_through_chat_is_decrypted_by_read() {
    let world = World::start().await;
    let owner = world.owner().await;
    let member = world.agent(2).await;
    let other = world.agent(3).await;
    org::channel_create(
        &owner,
        ChannelCreateParams {
            id: "vault".to_owned(),
            kind: "group".to_owned(),
            name: "vault".to_owned(),
            parent: None,
            members: vec![pk_of(&member), pk_of(&other)],
            encrypt: true,
        },
    )
    .await
    .expect("create the encrypted channel");
    let mut client = session(&world, 2, legacy_config(), "mock-client").await;
    let posted = client
        .call_tool(
            "chat",
            json!({"channel": "vault", "text": "classified words"}),
        )
        .await;
    assert_tool_ok(&posted);

    let result = client.call_tool("read", json!({"channel": "vault"})).await;

    assert_tool_ok(&result);
    let messages = result["structuredContent"]["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages.len(), 1, "{result}");
    assert_eq!(messages[0]["encrypted"], true);
    assert_eq!(messages[0]["text"], "classified words");
    client.close().await;
    world.shutdown().await;
}
