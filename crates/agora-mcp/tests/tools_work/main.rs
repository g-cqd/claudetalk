//! Work-queue, pool, quiet-window, policy, metrics and card tools of M5 (SPEC §7.4) driven through stdio sessions
//! against an in-process daemon. Identities come from fixed seeds: 1 is the owner, 2 (A) and 3 (B) are agents that
//! start as workers. The HTTP refusals of the local-only calls (D16) are checked over `HttpHarness`.
//!
//! `work_run` starts a job through the daemon's supervisor. The supervisor binary is `agora-supervise` (agora-daemon),
//! which this crate's tests cannot reach, so the two tests that run a job are ignored; every other test is live.

#[path = "../common/mod.rs"]
mod common;

use agora_mcp::engine::EngineConfig;
use agora_mcp::framing::UNTRUSTED_DATA_NOTICE;
use common::{HttpHarness, StdioClient, World};
use serde_json::{Value, json};
use std::path::Path;

mod board;
mod catalogue;
mod http;
mod plan;
mod policy_pool;
mod queue;
mod schedule;
mod wait_and_run;

const OWNER: u8 = 1;
const ALICE: u8 = 2;
const BOB: u8 = 3;

/// The 24 tools M5 adds, in the §7.4 table order, with (read_only, destructive, idempotent).
const NEW_TOOLS: &[(&str, bool, bool, bool)] = &[
    ("work_scope", false, false, true),
    ("work_add", false, false, false),
    ("work_show", true, false, true),
    ("work_next", false, false, false),
    ("work_claim", false, false, false),
    ("work_accept", false, false, true),
    ("work_run", false, true, false),
    ("work_wait", true, false, true),
    ("work_status", true, false, true),
    ("work_cancel", false, true, false),
    ("work_finish", false, false, false),
    ("work_schedule", false, false, false),
    ("work_settings", false, false, true),
    ("work_plan", true, false, true),
    ("pool_define", false, false, true),
    ("pool_lease", false, false, false),
    ("pool_release", false, false, true),
    ("quiet_window", false, false, false),
    ("watchdog_report", true, false, true),
    ("reclaim", false, true, false),
    ("escalate", false, false, false),
    ("policy_set", false, false, true),
    ("metrics", true, false, true),
    ("card_update", false, false, true),
];

/// The `plan_two_agents_basic` ordering fixture (differential fixture of agora-sched), compiled in.
const PLAN_INPUT: &str =
    include_str!("../../../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.input.json");
const PLAN_EXPECTED: &str = include_str!(
    "../../../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.expected.json"
);

/// A stdio session for the identity `seed`, initialized as a mock client, with channel push off.
async fn session(world: &World, seed: u8) -> StdioClient {
    let cfg = EngineConfig {
        channel_push: false,
        ..EngineConfig::stdio()
    };
    let mut client = StdioClient::start(world, world.ctx(seed), cfg).await;
    client.initialize("mock-client").await;
    client
}

/// The owner (seed 1) and two agents, A (seed 2) and B (seed 3), all registered.
async fn board_of_three(world: &World) {
    world.owner().await;
    world.agent(ALICE).await;
    world.agent(BOB).await;
}

/// The whole result of a call that must succeed.
async fn call_ok(client: &mut StdioClient, tool: &str, args: Value) -> Value {
    let result = client.call_tool(tool, args).await;
    assert_eq!(result["isError"], json!(false), "{tool} failed: {result}");
    result
}

/// The structuredContent of a call that must succeed.
async fn ok(client: &mut StdioClient, tool: &str, args: Value) -> Value {
    call_ok(client, tool, args).await["structuredContent"].clone()
}

/// The `error` object (code and message) of a call that must be refused.
async fn refused(client: &mut StdioClient, tool: &str, args: Value) -> Value {
    let result = client.call_tool(tool, args).await;
    assert_eq!(
        result["isError"],
        json!(true),
        "{tool} should be refused: {result}"
    );
    result["structuredContent"]["error"].clone()
}

/// The text of the first content block of a tool result.
fn text_of(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result has no text block: {result}"))
        .to_owned()
}

/// `work_add` arguments of the task `t1`, which runs `sh -c "echo hi"` in `dir`.
fn echo_task(dir: &Path) -> Value {
    json!({
        "id": "t1",
        "command": ["sh", "-c", "echo hi"],
        "cwd": dir.to_str().expect("a UTF-8 temp path"),
    })
}

/// The digest `work_show` prints for a task, as a string.
async fn shown_digest(client: &mut StdioClient, id: &str) -> String {
    let shown = ok(client, "work_show", json!({"id": id})).await;
    shown["digest"]
        .as_str()
        .unwrap_or_else(|| panic!("work_show of {id} has no digest: {shown}"))
        .to_owned()
}

/// A digest is 64 lowercase hex digits.
fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A JSON-RPC `tools/call` body for `name` with `arguments`.
fn tool_call_body(name: &str, arguments: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
    .to_string()
}
