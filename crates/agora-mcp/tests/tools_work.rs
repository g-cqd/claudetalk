//! Work-queue, pool, quiet-window, policy, metrics and card tools of M5 (SPEC §7.4) driven through stdio sessions
//! against an in-process daemon. Identities come from fixed seeds: 1 is the owner, 2 (A) and 3 (B) are agents that
//! start as workers. The HTTP refusals of the local-only calls (D16) are checked over `HttpHarness`.
//!
//! `work_run` starts a job through the daemon's supervisor. The supervisor binary is `agora-supervise` (agora-daemon),
//! which this crate's tests cannot reach, so the two tests that run a job are ignored; every other test is live.

mod common;

use agora_mcp::engine::EngineConfig;
use agora_mcp::framing::UNTRUSTED_DATA_NOTICE;
use common::{HttpHarness, StdioClient, World};
use serde_json::{Value, json};
use std::path::Path;

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
    include_str!("../../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.input.json");
const PLAN_EXPECTED: &str =
    include_str!("../../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.expected.json");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_list_serves_all_57_tools() {
    let world = World::start().await;
    let mut client = session(&world, ALICE).await;

    let response = client.request("tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list has no tools array: {response}"));

    assert_eq!(tools.len(), 57);
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_and_scheduling_tools_carry_the_spec_annotations() {
    let world = World::start().await;
    let mut client = session(&world, ALICE).await;

    let response = client.request("tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list has no tools array: {response}"))
        .clone();
    for (name, read_only, destructive, idempotent) in NEW_TOOLS {
        let tool = tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("tools/list has no tool named {name}"));
        let annotations = &tool["annotations"];
        assert_eq!(annotations["readOnlyHint"], json!(read_only), "{name}");
        assert_eq!(annotations["destructiveHint"], json!(destructive), "{name}");
        assert_eq!(annotations["idempotentHint"], json!(idempotent), "{name}");
    }
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_add_queues_the_task_with_the_documented_defaults() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;

    let out = ok(&mut alice, "work_add", echo_task(dir.path())).await;

    let task = &out["task"];
    assert_eq!(task["id"], json!("t1"));
    assert_eq!(task["state"], json!("queued"));
    assert_eq!(task["kind"], json!("other"));
    assert_eq!(task["size"], json!(1.0));
    assert_eq!(task["value"], json!(1.0));
    assert_eq!(task["weight"], json!(1.0));
    assert_eq!(task["command"], json!(["sh", "-c", "echo hi"]));
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_next_claims_the_queued_command_for_the_other_agent() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;

    let out = ok(&mut bob, "work_next", json!({})).await;

    let claimed = &out["claimed"];
    assert_eq!(claimed["id"], json!("t1"));
    assert_eq!(claimed["state"], json!("claimed"));
    // A command claimed by someone other than its author waits for the claimant's acceptance.
    assert_eq!(claimed["needs_acceptance"], json!(true));
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_show_frames_the_command_as_untrusted_data_and_reports_its_digest() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;
    ok(&mut bob, "work_next", json!({})).await;

    let result = call_ok(&mut bob, "work_show", json!({"id": "t1"})).await;

    let text = text_of(&result);
    assert!(
        text.starts_with(UNTRUSTED_DATA_NOTICE),
        "show text starts with the notice: {text}"
    );
    assert!(text.contains("<untrusted-data>"), "{text}");
    assert!(text.contains("echo hi"), "the command is shown: {text}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["notice"], json!(UNTRUSTED_DATA_NOTICE));
    let digest = structured["digest"].as_str().expect("show has a digest");
    assert!(is_digest(digest), "digest is 64 lowercase hex: {digest}");
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_accept_by_the_other_agent_with_the_shown_digest_is_the_first_acceptance() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;
    ok(&mut bob, "work_next", json!({})).await;
    let digest = shown_digest(&mut bob, "t1").await;

    let out = ok(
        &mut bob,
        "work_accept",
        json!({"id": "t1", "digest": digest}),
    )
    .await;

    assert_eq!(out["first"], json!(true));
    assert_eq!(out["task"]["needs_acceptance"], json!(false));
    assert_eq!(out["digest"], json!(digest));
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_accept_with_a_wrong_digest_is_refused_and_the_command_stays_unaccepted() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;
    ok(&mut bob, "work_next", json!({})).await;
    let wrong = "0".repeat(64);

    let error = refused(
        &mut bob,
        "work_accept",
        json!({"id": "t1", "digest": wrong}),
    )
    .await;
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("the digest does not match the command of task t1"),
        "{error}"
    );

    let shown = ok(&mut bob, "work_show", json!({"id": "t1"})).await;
    assert_eq!(shown["task"]["needs_acceptance"], json!(true));
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_author_cannot_accept_its_own_command() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;
    ok(&mut alice, "work_claim", json!({"id": "t1"})).await;
    let digest = shown_digest(&mut alice, "t1").await;

    let error = refused(
        &mut alice,
        "work_accept",
        json!({"id": "t1", "digest": digest}),
    )
    .await;

    // The client prefixes a remote refusal with its kind ("Invalid: "); the store's sentence follows it.
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.ends_with("a command must be accepted by an identity other than its author"),
        "{error}"
    );
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_wait_on_an_unknown_task_is_refused() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut alice = session(&world, ALICE).await;

    let error = refused(
        &mut alice,
        "work_wait",
        json!({"tasks": ["no-such-task"], "timeout_secs": 1}),
    )
    .await;

    assert_eq!(error["code"], json!("not_found"), "{error}");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains("task no-such-task"), "{error}");
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn local_only_calls_are_refused_over_http_with_their_spec_messages() {
    let world = World::start().await;
    let harness = HttpHarness::start(&world, 1).await;
    let cases: [(&str, Value, &str); 9] = [
        (
            "work_run",
            json!({"command": ["true"]}),
            "running a command is not available over HTTP",
        ),
        (
            "work_add",
            json!({"command": ["true"]}),
            "a command in work_add is not available over HTTP",
        ),
        (
            "work_add",
            json!({"title": "x", "cwd": "/tmp"}),
            "cwd and files in work_add is not available over HTTP",
        ),
        (
            "work_add",
            json!({"title": "x", "files": ["src/main.rs"]}),
            "cwd and files in work_add is not available over HTTP",
        ),
        (
            "work_wait",
            json!({"pid": [4242]}),
            "waiting for a pid is not available over HTTP",
        ),
        (
            "work_wait",
            json!({"file": ["/tmp/agora-tools-work-file"]}),
            "waiting for a file is not available over HTTP",
        ),
        (
            "work_wait",
            json!({"gone": ["/tmp/agora-tools-work-gone"]}),
            "waiting for a path to disappear is not available over HTTP",
        ),
        (
            "work_wait",
            json!({"log_path": "/tmp/agora-tools-work.log", "pattern": "ok"}),
            "watching a log is not available over HTTP",
        ),
        (
            "pool_lease",
            json!({"path": "/tmp/agora-tools-work-path"}),
            "a path lease is not available over HTTP",
        ),
    ];

    for (tool, args, message) in cases {
        assert_refused_over_http(&harness, tool, args, message).await;
    }
    harness.shutdown().await;
    world.shutdown().await;
}

/// A `tools/call` over HTTP that must be refused: a usage error whose message is `message`.
async fn assert_refused_over_http(harness: &HttpHarness, tool: &str, args: Value, message: &str) {
    let body = tool_call_body(tool, args);
    let (status, _, text) = harness.post(Some(harness.token(0)), &body, &[]).await;
    assert_eq!(status, 200, "{tool}: {text}");
    let response: Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: not JSON ({e}): {text}"));
    let result = &response["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "{tool} should be refused: {text}"
    );
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        json!("usage"),
        "{tool}: {text}"
    );
    assert_eq!(
        result["structuredContent"]["error"]["message"],
        json!(message),
        "{tool}: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn work_accept_of_a_command_is_refused_over_http() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut alice = session(&world, ALICE).await;
    ok(&mut alice, "work_add", echo_task(dir.path())).await;
    let digest = shown_digest(&mut alice, "t1").await;
    let harness = HttpHarness::start(&world, 1).await;

    assert_refused_over_http(
        &harness,
        "work_accept",
        json!({"id": "t1", "digest": digest}),
        "accepting a command is not available over HTTP",
    )
    .await;

    harness.shutdown().await;
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_plan_with_given_tasks_returns_the_fixture_plan() {
    let world = World::start().await;
    let mut alice = session(&world, ALICE).await;
    let input: Value = serde_json::from_str(PLAN_INPUT).expect("the input fixture is JSON");
    let expected: Value =
        serde_json::from_str(PLAN_EXPECTED).expect("the expected fixture is JSON");
    assert_eq!(expected["exit_code"], json!(0));
    let expected_plan: Value = serde_json::from_str(
        expected["stdout"]
            .as_str()
            .expect("the expected fixture has stdout text"),
    )
    .expect("the expected stdout is the plan JSON");

    let out = ok(
        &mut alice,
        "work_plan",
        json!({"tasks": input["tasks"], "agents": 2}),
    )
    .await;

    assert_eq!(out["plan"], expected_plan);
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_plan_refuses_more_agents_than_the_cap() {
    let world = World::start().await;
    let mut alice = session(&world, ALICE).await;

    let error = refused(&mut alice, "work_plan", json!({"agents": 300})).await;

    assert_eq!(error["code"], json!("usage"));
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_report_is_structured_and_empty_on_an_idle_board() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;

    let out = ok(&mut owner, "watchdog_report", json!({})).await;

    assert_eq!(out["report"]["stale"], json!([]));
    assert_eq!(out["report"]["expiring_leases"], json!([]));
    owner.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_reports_the_window_it_was_asked_for() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;

    let out = ok(&mut owner, "metrics", json!({"since_ms": 1000})).await;

    assert_eq!(out["report"]["since_ms"], json!(1000));
    assert_eq!(out["report"]["cards_done"], json!(0));
    owner.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_set_by_a_worker_is_refused_as_forbidden() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut alice = session(&world, ALICE).await;

    let error = refused(
        &mut alice,
        "policy_set",
        json!({"team": "core", "lane": "dev", "wip_limit": 2}),
    )
    .await;

    assert_eq!(error["code"], json!("forbidden"));
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_set_by_the_owner_appears_in_the_policy_list() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;
    ok(
        &mut owner,
        "policy_set",
        json!({"team": "core", "lane": "dev", "wip_limit": 3}),
    )
    .await;

    let out = ok(&mut owner, "policy_set", json!({"list": true})).await;

    let policies = out["policies"].as_array().expect("a policies array");
    let entry = policies
        .iter()
        .find(|p| p["team"] == json!("core") && p["lane"] == json!("dev"))
        .unwrap_or_else(|| panic!("the core/dev policy is listed: {out}"));
    assert_eq!(entry["policy"]["wip_limit"], json!(3));
    owner.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_lease_beyond_the_pool_capacity_is_refused() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(
        &mut owner,
        "pool_define",
        json!({"name": "gpu", "capacity": 1}),
    )
    .await;
    ok(
        &mut alice,
        "pool_lease",
        json!({"pool": "gpu", "amount": 1}),
    )
    .await;

    let error = refused(&mut bob, "pool_lease", json!({"pool": "gpu", "amount": 1})).await;

    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains("pool gpu has 0 of 1"), "{error}");
    owner.close().await;
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_release_frees_the_units_for_the_next_lease() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;
    let mut alice = session(&world, ALICE).await;
    let mut bob = session(&world, BOB).await;
    ok(
        &mut owner,
        "pool_define",
        json!({"name": "gpu", "capacity": 1}),
    )
    .await;
    let held = ok(
        &mut alice,
        "pool_lease",
        json!({"pool": "gpu", "amount": 1}),
    )
    .await;
    let lease_id = held["lease"]["id"].as_str().expect("the lease has an id");
    ok(&mut alice, "pool_release", json!({"lease": lease_id})).await;

    let next = ok(&mut bob, "pool_lease", json!({"pool": "gpu", "amount": 1})).await;

    assert_eq!(next["lease"]["amount"], json!(1.0));
    owner.close().await;
    alice.close().await;
    bob.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiet_window_opened_by_the_owner_is_listed_until_it_is_closed() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;
    let opened = ok(
        &mut owner,
        "quiet_window",
        json!({"action": "open", "for_secs": 600, "note": "release build"}),
    )
    .await;
    let lease_id = opened["lease"]["id"]
        .as_str()
        .expect("the window has a lease id");

    let listed = ok(&mut owner, "quiet_window", json!({"action": "status"})).await;
    let ids: Vec<&str> = listed["leases"]
        .as_array()
        .expect("a leases array")
        .iter()
        .filter_map(|l| l["id"].as_str())
        .collect();
    assert_eq!(ids, vec![lease_id]);

    ok(
        &mut owner,
        "quiet_window",
        json!({"action": "close", "lease": lease_id}),
    )
    .await;
    let after = ok(&mut owner, "quiet_window", json!({"action": "status"})).await;
    assert_eq!(after["leases"], json!([]));
    owner.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escalate_flags_the_card_and_the_board_summary_lists_it_as_needing_its_owner() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;
    let mut alice = session(&world, ALICE).await;
    ok(
        &mut alice,
        "card_create",
        json!({"id": "c1", "title": "fix the build"}),
    )
    .await;

    ok(
        &mut owner,
        "escalate",
        json!({"card": "c1", "owner_action": "pick a lane"}),
    )
    .await;

    let summary = ok(&mut owner, "board_summary", json!({})).await;
    assert_eq!(
        summary["extras"]["needs_owner"],
        json!([{"card": "c1", "title": "fix the build", "owner_action": "pick a lane"}])
    );
    owner.close().await;
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_update_sets_the_priority_of_a_card_the_worker_owns() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut alice = session(&world, ALICE).await;
    ok(
        &mut alice,
        "card_create",
        json!({"id": "c1", "title": "fix the build"}),
    )
    .await;

    let out = ok(
        &mut alice,
        "card_update",
        json!({"card": "c1", "priority": 7}),
    )
    .await;

    let cards = out["cards"].as_array().expect("a cards array");
    assert_eq!(cards.len(), 1, "{out}");
    assert_eq!(cards[0]["id"], json!("c1"));
    assert_eq!(cards[0]["priority"], json!(7));
    assert_eq!(cards[0]["needs_owner"], json!(false));
    alice.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board_summary_carries_the_scheduling_extras_of_an_idle_board() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut owner = session(&world, OWNER).await;

    let summary = ok(&mut owner, "board_summary", json!({})).await;

    let extras = &summary["extras"];
    assert!(extras.is_object(), "extras is an object: {summary}");
    assert_eq!(extras["needs_owner"], json!([]));
    assert_eq!(extras["wip"], json!([]));
    assert_eq!(extras["running_jobs"], json!(0));
    assert_eq!(extras["waiting_jobs"], json!(0));
    assert_eq!(extras["queued_tasks"], json!(0));
    assert_eq!(extras["open_leases"], json!(0));
    owner.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_schedule_create_list_and_delete_round_trip() {
    let world = World::start().await;
    board_of_three(&world).await;
    let mut alice = session(&world, ALICE).await;

    let created = ok(
        &mut alice,
        "work_schedule",
        json!({"action": "create", "id": "nightly", "when": "every:3600", "title": "nightly check"}),
    )
    .await;
    assert_eq!(created["timer"]["id"], json!("nightly"));

    let listed = ok(&mut alice, "work_schedule", json!({"action": "list"})).await;
    let ids: Vec<&str> = listed["timers"]
        .as_array()
        .expect("a timers array")
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["nightly"]);

    ok(
        &mut alice,
        "work_schedule",
        json!({"action": "delete", "id": "nightly"}),
    )
    .await;
    let after = ok(&mut alice, "work_schedule", json!({"action": "list"})).await;
    assert_eq!(after["timers"], json!([]));
    alice.close().await;
    world.shutdown().await;
}

/// Alice queues `t1`; Bob claims it, reads its digest and accepts it. Returns Bob's session.
async fn accepted_echo(world: &World, dir: &Path) -> StdioClient {
    let mut alice = session(world, ALICE).await;
    let mut bob = session(world, BOB).await;
    ok(&mut alice, "work_add", echo_task(dir)).await;
    ok(&mut bob, "work_next", json!({})).await;
    let digest = shown_digest(&mut bob, "t1").await;
    ok(
        &mut bob,
        "work_accept",
        json!({"id": "t1", "digest": digest}),
    )
    .await;
    alice.close().await;
    bob
}

#[ignore = "covered by crates/agora-cli/tests/mcp_work_e2e.rs (needs the agora binary's _supervise)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_run_of_an_accepted_task_exits_with_status_zero() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut bob = accepted_echo(&world, dir.path()).await;

    let out = ok(
        &mut bob,
        "work_run",
        json!({"task": "t1", "timeout_secs": 30}),
    )
    .await;

    assert_eq!(out["wait"]["status"], json!(0));
    assert_eq!(out["wait"]["timed_out"], json!(false));
    bob.close().await;
    world.shutdown().await;
}

#[ignore = "covered by crates/agora-cli/tests/mcp_work_e2e.rs (needs the agora binary's _supervise)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_run_reports_the_job_output_in_the_wait_tail() {
    let world = World::start().await;
    board_of_three(&world).await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut bob = accepted_echo(&world, dir.path()).await;

    let out = ok(
        &mut bob,
        "work_run",
        json!({"task": "t1", "timeout_secs": 30}),
    )
    .await;

    let tail = out["wait"]["tail"].as_array().expect("the wait has a tail");
    assert!(
        tail.iter().any(|line| line == &json!("hi")),
        "the tail carries the job output: {out}"
    );
    bob.close().await;
    world.shutdown().await;
}
