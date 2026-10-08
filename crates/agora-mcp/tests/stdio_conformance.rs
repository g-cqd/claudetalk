//! Raw JSON-RPC conformance of the stdio transport: the protocol facts of SPEC Part 3 are
//! checked on the wire (newline-delimited JSON, no typed client in between), one behavior per
//! test, each test on a fresh in-process daemon.

mod common;

use agora_mcp::McpError;
use agora_mcp::engine::{Engine, EngineConfig, Peer, Transport};
use agora_mcp::stdio;
use common::{StdioClient, World};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Upper bound on each wait for a message the server must send.
const WAIT: Duration = Duration::from_secs(10);
/// How long a silence must last before we accept that nothing more will come.
const SILENCE: Duration = Duration::from_secs(2);
/// The number of tools in the Part 3 table.
const TOOL_COUNT: usize = 57;

/// The `result` of a response; panics when the response has none.
fn result_of(response: &Value) -> &Value {
    match response.get("result") {
        Some(result) => result,
        None => panic!("response has no result: {response}"),
    }
}

/// The error code of a response; panics when the response has no error.
fn error_code(response: &Value) -> i64 {
    match response["error"]["code"].as_i64() {
        Some(code) => code,
        None => panic!("response has no error code: {response}"),
    }
}

/// The next message that carries an id, skipping notifications; None when `within` passes first.
async fn next_response(client: &mut StdioClient, within: Duration) -> Option<Value> {
    tokio::time::timeout(within, first_with_id(client, within))
        .await
        .unwrap_or_default()
}

/// Reads until a message with an id arrives; None when the stream stalls or ends.
async fn first_with_id(client: &mut StdioClient, within: Duration) -> Option<Value> {
    loop {
        let msg = client.read_message(within).await?;
        if msg.get("id").is_some() {
            return Some(msg);
        }
    }
}

/// A stdio client for the agent (seed 2) on `world`, with the owner bootstrapped first.
async fn stdio_client(world: &World, cfg: EngineConfig) -> StdioClient {
    world.owner().await;
    let ctx = world.agent(2).await;
    StdioClient::start(world, ctx, cfg).await
}

/// The `tools` array of a legacy `tools/list`; panics when the response has none.
async fn tools_of(client: &mut StdioClient) -> Vec<Value> {
    let response = client.request("tools/list", json!({})).await;
    match result_of(&response)["tools"].as_array() {
        Some(tools) => tools.clone(),
        None => panic!("tools/list result has no tools array: {response}"),
    }
}

/// The tool named `name`; panics when the list has no such tool.
fn tool_named<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    match tools.iter().find(|t| t["name"] == json!(name)) {
        Some(tool) => tool,
        None => panic!("tools/list has no tool named {name}"),
    }
}

/// Raw `initialize` with the given protocol version and client name; returns the full response.
async fn initialize_raw(client: &mut StdioClient, version: &str, client_name: &str) -> Value {
    client
        .request(
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": client_name, "version": "1.0"},
            }),
        )
        .await
}

// 1. initialize-first (2025-11-25)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_answers_2025_11_25_with_identity_and_instructions() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = initialize_raw(&mut client, "2025-11-25", "mock").await;
    let result = result_of(&response);

    assert_eq!(result["protocolVersion"], json!("2025-11-25"));
    assert_eq!(result["serverInfo"]["name"], json!("agora"));
    assert!(
        result["capabilities"].get("tools").is_some(),
        "capabilities advertise tools: {result}"
    );
    let instructions = result["instructions"].as_str().unwrap_or_default();
    assert!(!instructions.is_empty(), "instructions are sent: {result}");
    assert!(
        instructions.contains("inbox_wait"),
        "instructions name inbox_wait: {instructions}"
    );
    assert!(
        instructions.contains("untrusted"),
        "instructions state the untrusted-data rule: {instructions}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_list_works_after_initialize() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let response = client.request("tools/list", json!({})).await;

    assert!(
        response.get("error").is_none(),
        "tools/list after initialize is not an error: {response}"
    );
    assert!(
        result_of(&response)["tools"].is_array(),
        "tools/list returns a tools array: {response}"
    );
    drop(client);
    world.shutdown().await;
}

// 2. Version negotiation on initialize

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_echoes_2025_06_18() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = initialize_raw(&mut client, "2025-06-18", "mock").await;

    assert_eq!(result_of(&response)["protocolVersion"], json!("2025-06-18"));
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_echoes_2025_03_26() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = initialize_raw(&mut client, "2025-03-26", "mock").await;

    assert_eq!(result_of(&response)["protocolVersion"], json!("2025-03-26"));
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_counter_offers_2025_11_25_for_the_modern_version() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = initialize_raw(&mut client, "2026-07-28", "mock").await;

    assert_eq!(result_of(&response)["protocolVersion"], json!("2025-11-25"));
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_counter_offers_2025_11_25_for_an_unknown_version() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = initialize_raw(&mut client, "1.0.0", "mock").await;

    assert_eq!(result_of(&response)["protocolVersion"], json!("2025-11-25"));
    drop(client);
    world.shutdown().await;
}

// 3. The experimental claude/channel capability

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_declares_claude_channel_for_claude_code_on_stdio() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let result = client.initialize("claude-code").await;

    assert_eq!(
        result["capabilities"]["experimental"],
        json!({"claude/channel": {}}),
        "claude-code on stdio gets the channel capability: {result}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_omits_claude_channel_for_another_harness() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let result = client.initialize("codex-mcp-client").await;

    assert!(
        result["capabilities"].get("experimental").is_none(),
        "a codex client gets no experimental capability: {result}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_omits_claude_channel_when_channel_push_is_off() {
    let world = World::start().await;
    let cfg = EngineConfig {
        channel_push: false,
        ..EngineConfig::stdio()
    };
    let mut client = stdio_client(&world, cfg).await;

    let result = client.initialize("claude-code").await;

    assert!(
        result["capabilities"].get("experimental").is_none(),
        "with channel_push off, claude-code gets no experimental capability: {result}"
    );
    drop(client);
    world.shutdown().await;
}

// 4. Discover-first (modern era)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_discover_describes_the_modern_server() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client
        .request(
            "server/discover",
            json!({"_meta": StdioClient::modern_meta("claude-code")}),
        )
        .await;
    let result = result_of(&response);

    assert_eq!(result["resultType"], json!("complete"));
    assert_eq!(
        result["supportedVersions"],
        json!(["2026-07-28", "2025-11-25"])
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("agora")
    );
    assert!(
        result["capabilities"].get("tools").is_some(),
        "discover advertises tools: {result}"
    );
    let instructions = result["instructions"].as_str().unwrap_or_default();
    assert!(
        !instructions.is_empty(),
        "discover carries instructions: {result}"
    );
    assert!(
        result["capabilities"].get("experimental").is_none(),
        "a modern session never declares claude/channel: {result}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modern_tools_list_works_without_initialize() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client
        .request(
            "tools/list",
            json!({"_meta": StdioClient::modern_meta("mock-modern")}),
        )
        .await;
    let result = result_of(&response);

    assert_eq!(result["resultType"], json!("complete"));
    assert!(
        result["tools"].is_array(),
        "modern tools/list returns tools: {response}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_tools_list_without_initialize_is_refused() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client.request("tools/list", json!({})).await;

    assert_eq!(
        error_code(&response),
        -32600,
        "legacy request before initialize: {response}"
    );
    drop(client);
    world.shutdown().await;
}

// 5. Antigravity-style fallback

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_methods_are_method_not_found_and_the_session_recovers() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    for method in ["resources/list", "prompts/list", "logging/setLevel"] {
        let response = client.request(method, json!({})).await;
        assert_eq!(
            error_code(&response),
            -32601,
            "{method} is not implemented: {response}"
        );
    }
    client.initialize("mock").await;
    let tools = tools_of(&mut client).await;

    assert!(
        !tools.is_empty(),
        "tools/list works after the unknown methods"
    );
    drop(client);
    world.shutdown().await;
}

// 6. Modern request validation

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_discover_without_meta_is_invalid_params() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client.request("server/discover", json!({})).await;

    assert_eq!(
        error_code(&response),
        -32602,
        "discover needs modern _meta: {response}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modern_request_with_an_unsupported_version_lists_the_supported_ones() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client
        .request(
            "tools/list",
            json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2099-01-01",
                "io.modelcontextprotocol/clientCapabilities": {},
            }}),
        )
        .await;

    assert_eq!(
        error_code(&response),
        -32022,
        "unsupported version: {response}"
    );
    assert_eq!(
        response["error"]["data"]["supported"],
        json!(["2026-07-28", "2025-11-25"])
    );
    assert_eq!(response["error"]["data"]["requested"], json!("2099-01-01"));
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modern_request_without_client_capabilities_is_invalid_params() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client
        .request(
            "tools/list",
            json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}),
        )
        .await;

    assert_eq!(
        error_code(&response),
        -32602,
        "clientCapabilities is required: {response}"
    );
    drop(client);
    world.shutdown().await;
}

// 7. JSON-RPC framing and errors

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_json_line_is_a_parse_error_with_null_id() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client.send_raw("{not json").await;
    let response = next_response(&mut client, WAIT)
        .await
        .expect("a parse error response");

    assert_eq!(error_code(&response), -32700, "{response}");
    assert_eq!(response.get("id"), Some(&Value::Null), "{response}");
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_array_batch_is_refused_with_null_id() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client
        .send_raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#)
        .await;
    let response = next_response(&mut client, WAIT)
        .await
        .expect("a batch error response");

    assert_eq!(error_code(&response), -32600, "{response}");
    assert_eq!(response.get("id"), Some(&Value::Null), "{response}");
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_jsonrpc_version_is_an_invalid_request_that_echoes_the_id() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client
        .send_raw(r#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#)
        .await;
    let response = next_response(&mut client, WAIT)
        .await
        .expect("an invalid request response");

    assert_eq!(error_code(&response), -32600, "{response}");
    assert_eq!(response.get("id"), Some(&json!(5)), "{response}");
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_known_notification_gets_no_response() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client.notify("notifications/initialized", json!({})).await;
    client
        .send_raw(r#"{"jsonrpc":"2.0","id":300,"method":"ping","params":{}}"#)
        .await;
    let next = next_response(&mut client, WAIT)
        .await
        .expect("the ping is answered");

    assert_eq!(
        next["id"],
        json!(300),
        "the first message after the notification is the ping's response: {next}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_notification_is_ignored() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client
        .notify("notifications/made_up_by_a_client", json!({}))
        .await;
    client
        .send_raw(r#"{"jsonrpc":"2.0","id":301,"method":"ping","params":{}}"#)
        .await;
    let next = next_response(&mut client, WAIT)
        .await
        .expect("the ping is answered");

    assert_eq!(
        next["id"],
        json!(301),
        "an unknown notification writes nothing: {next}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_response_object_is_ignored() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    client
        .send_raw(r#"{"jsonrpc":"2.0","id":77,"result":{}}"#)
        .await;
    client
        .send_raw(r#"{"jsonrpc":"2.0","id":302,"method":"ping","params":{}}"#)
        .await;
    let next = next_response(&mut client, WAIT)
        .await
        .expect("the ping is answered");

    assert_eq!(
        next["id"],
        json!(302),
        "a client response writes nothing: {next}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_is_answered_before_initialize() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;

    let response = client.request("ping", json!({})).await;

    assert_eq!(
        result_of(&response),
        &json!({}),
        "ping answers with an empty result: {response}"
    );
    drop(client);
    world.shutdown().await;
}

// 8. tools/list schema validity

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_list_has_57_tools_with_unique_names() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let tools = tools_of(&mut client).await;
    let names: HashSet<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert_eq!(tools.len(), TOOL_COUNT, "the tool table has 57 entries");
    assert_eq!(names.len(), TOOL_COUNT, "every tool name appears once");
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tool_has_a_well_formed_input_schema() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let tools = tools_of(&mut client).await;
    for tool in &tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(!description.trim().is_empty(), "{name} has a description");

        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], json!("object"), "{name} takes an object");
        assert!(
            schema.get("$schema").is_none(),
            "{name} carries no $schema key"
        );
        let Some(properties) = schema["properties"].as_object() else {
            panic!("{name} inputSchema has no properties object: {schema}");
        };
        if let Some(required) = schema.get("required") {
            let required = required
                .as_array()
                .unwrap_or_else(|| panic!("{name} required is not an array"));
            for key in required {
                let key = key
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} required entry is not a string"));
                assert!(
                    properties.contains_key(key),
                    "{name} requires {key}, which is not a property"
                );
            }
        }
    }
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tool_declares_complete_boolean_annotations() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let tools = tools_of(&mut client).await;
    for tool in &tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        let annotations = &tool["annotations"];
        for hint in ["readOnlyHint", "destructiveHint", "idempotentHint"] {
            assert!(
                annotations[hint].is_boolean(),
                "{name} declares {hint} as a boolean: {annotations}"
            );
        }
        assert_eq!(
            annotations["openWorldHint"],
            json!(false),
            "{name} is not open-world"
        );
    }
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn annotations_match_the_effect_of_representative_tools() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let tools = tools_of(&mut client).await;

    assert_eq!(
        tool_named(&tools, "inbox_wait")["annotations"]["readOnlyHint"],
        json!(true)
    );
    assert_eq!(
        tool_named(&tools, "channel_remove")["annotations"]["destructiveHint"],
        json!(true)
    );
    assert_eq!(
        tool_named(&tools, "ack")["annotations"]["idempotentHint"],
        json!(true)
    );
    assert_eq!(
        tool_named(&tools, "chat")["annotations"]["readOnlyHint"],
        json!(false),
        "posting a message is not read-only"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_takes_exactly_cursor_timeout_and_limit() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let tools = tools_of(&mut client).await;
    let properties = tool_named(&tools, "inbox_wait")["inputSchema"]["properties"]
        .as_object()
        .expect("inbox_wait has a properties object");
    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
    keys.sort_unstable();

    assert_eq!(keys, vec!["cursor", "limit", "timeout_secs"]);
    drop(client);
    world.shutdown().await;
}

// 9. tools/call errors

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_call_with_an_unknown_name_is_invalid_params() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let response = client
        .request(
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
        )
        .await;

    assert_eq!(
        error_code(&response),
        -32602,
        "unknown tool is a protocol error: {response}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_call_with_a_wrong_argument_type_is_an_invalid_arguments_tool_error() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let result = client
        .call_tool("chat", json!({"channel": 5, "text": "hello"}))
        .await;

    assert_eq!(result["isError"], json!(true), "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        json!("invalid_arguments"),
        "{result}"
    );
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_call_with_an_unknown_argument_field_is_an_invalid_arguments_tool_error() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let result = client.call_tool("whoami", json!({"surprise": true})).await;

    assert_eq!(result["isError"], json!(true), "{result}");
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        json!("invalid_arguments"),
        "{result}"
    );
    drop(client);
    world.shutdown().await;
}

// 10. Concurrency: a long call does not hold up a short one

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_call_is_answered_while_a_long_call_waits() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    client
        .send_raw(
            &json!({"jsonrpc": "2.0", "id": 100, "method": "tools/call",
                    "params": {"name": "inbox_wait", "arguments": {"timeout_secs": 5}}})
            .to_string(),
        )
        .await;
    client
        .send_raw(
            &json!({"jsonrpc": "2.0", "id": 101, "method": "tools/call",
                    "params": {"name": "whoami", "arguments": {}}})
            .to_string(),
        )
        .await;

    let first = next_response(&mut client, WAIT)
        .await
        .expect("a first response");
    assert_eq!(
        first["id"],
        json!(101),
        "whoami answers before the 5 s inbox_wait: {first}"
    );
    let second = next_response(&mut client, WAIT)
        .await
        .expect("the inbox_wait response");
    assert_eq!(second["id"], json!(100), "{second}");
    drop(client);
    world.shutdown().await;
}

// 11. Cancellation

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_call_gets_no_response() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    // A 1 s wait: an uncancelled call would answer inside the SILENCE window below, so the
    // silence check can tell a cancelled call from one that is merely still waiting.
    client
        .send_raw(
            &json!({"jsonrpc": "2.0", "id": 200, "method": "tools/call",
                    "params": {"name": "inbox_wait", "arguments": {"timeout_secs": 1}}})
            .to_string(),
        )
        .await;
    client
        .notify("notifications/cancelled", json!({"requestId": 200}))
        .await;
    client
        .send_raw(r#"{"jsonrpc":"2.0","id":201,"method":"ping","params":{}}"#)
        .await;

    let next = next_response(&mut client, WAIT)
        .await
        .expect("the ping is answered");
    assert_eq!(
        next["id"],
        json!(201),
        "the cancelled call is silent; the ping answers: {next}"
    );
    if let Some(late) = client.read_message(SILENCE).await {
        panic!("no message may follow the ping after a cancellation, got {late}");
    }
    drop(client);
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_and_its_cancel_in_one_write_get_no_response() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    // One write carries the call, its cancellation and a ping. The 1 s wait is shorter than
    // SILENCE, so a call that escaped the cancel would answer inside the silence window.
    let call = json!({"jsonrpc": "2.0", "id": 300, "method": "tools/call",
                      "params": {"name": "inbox_wait", "arguments": {"timeout_secs": 1}}});
    let cancel = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": 300}});
    let ping = json!({"jsonrpc": "2.0", "id": 301, "method": "ping", "params": {}});
    client.send_raw(&format!("{call}\n{cancel}\n{ping}")).await;

    let next = next_response(&mut client, WAIT)
        .await
        .expect("the ping is answered");
    assert_eq!(
        next["id"],
        json!(301),
        "the cancelled call is silent; the ping answers: {next}"
    );
    if let Some(late) = client.read_message(SILENCE).await {
        panic!("no message may follow the ping after a cancellation, got {late}");
    }
    drop(client);
    world.shutdown().await;
}

// 12. Both eras on one process

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_modern_call_works_on_a_stdio_process_that_initialized_as_legacy() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    let result = client.call_tool_modern("whoami", json!({})).await;

    assert_eq!(result["resultType"], json!("complete"), "{result}");
    drop(client);
    world.shutdown().await;
}

// 13. EOF

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_input_ends_the_server() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;

    client.close().await;
    world.shutdown().await;
}

/// A stdio server on an in-memory pipe that the test writes to and reads from by hand, and can
/// half-close (the input side of `StdioClient` is not readable after `close`).
struct RawSession {
    writer: WriteHalf<DuplexStream>,
    reader: Lines<BufReader<ReadHalf<DuplexStream>>>,
    serve: JoinHandle<Result<(), McpError>>,
}

/// A raw session with `cfg`; the owner is bootstrapped and the agent (seed 2) is the caller.
async fn raw_session(world: &World, cfg: EngineConfig) -> RawSession {
    world.owner().await;
    let ctx = world.agent(2).await;
    let (client_side, server_side) = tokio::io::duplex(1 << 20);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let engine = Engine::new(cfg);
    let peer = Arc::new(Peer::new(Arc::new(ctx), Transport::Stdio, None));
    let serve = tokio::spawn(stdio::serve(
        engine,
        peer,
        server_read,
        server_write,
        CancellationToken::new(),
    ));
    RawSession {
        writer: client_write,
        reader: BufReader::new(client_read).lines(),
        serve,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ping_followed_by_eof_in_one_write_is_still_answered() {
    let world = World::start().await;
    let mut session = raw_session(&world, EngineConfig::stdio()).await;

    // One write: the ping line, and the input closes right behind it.
    session
        .writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{}}\n")
        .await
        .expect("write the ping");
    session.writer.shutdown().await.expect("close the input");

    let line = tokio::time::timeout(WAIT, session.reader.next_line())
        .await
        .expect("the ping is answered")
        .expect("read the output")
        .expect("the output has a line");
    let reply: Value = serde_json::from_str(&line).expect("the reply is JSON");
    assert_eq!(reply["id"], json!(1), "{reply}");
    assert!(reply.get("result").is_some(), "{reply}");

    let ended = tokio::time::timeout(WAIT, session.serve)
        .await
        .expect("serve ends after EOF");
    ended
        .expect("serve did not panic")
        .expect("serve returned Ok");
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eof_during_a_long_wait_ends_the_server_within_two_seconds() {
    let world = World::start().await;
    let mut client = stdio_client(&world, EngineConfig::stdio()).await;
    client.initialize("mock").await;
    // A 55 s wait that is still running when the input closes.
    client
        .send_raw(
            &json!({"jsonrpc": "2.0", "id": 500, "method": "tools/call",
                    "params": {"name": "inbox_wait", "arguments": {"timeout_secs": 55}}})
            .to_string(),
        )
        .await;

    let started = Instant::now();
    client.close().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "serve took {elapsed:?} to end after EOF"
    );
    world.shutdown().await;
}
