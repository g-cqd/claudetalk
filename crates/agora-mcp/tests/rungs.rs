//! Delivery rungs of a stdio session (SPEC M4 "Heartbeats", "Capabilities file", "Escalation in
//! the shim"): which rungs the ladder reports for a Claude Code session from `capabilities.json`
//! and the heartbeats of its identity, that a non-Claude harness counts only its own heartbeats,
//! and how the tracker escalates an unacked message along an unarmed chain and past an armed rung.
//!
//! Capabilities and heartbeats are written before the session initializes, because `initialize`
//! computes the ladder from them. The escalation test polls `delivery_status` (bounded) for the
//! state changes that the delivery tick makes.

mod common;

use agora_client::Ctx;
use agora_client::capabilities::{self, CapabilitiesFile, ClaudeCapabilities, HarnessCapabilities};
use agora_client::heartbeat;
use agora_client::ops::now_ms;
use agora_deliver::{Harness, Mechanism};
use agora_mcp::engine::EngineConfig;
use agora_store::hex;
use common::{StdioClient, World};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::time::Instant;

const CHANNEL: &str = "general";
/// Upper bound for one wait on a `delivery_status` state change.
const WAIT: Duration = Duration::from_secs(10);
/// Pause between two `delivery_status` checks.
const POLL: Duration = Duration::from_millis(200);
/// Window in which a chain armed at `stop_continue` must not move past that rung. The patience
/// of `stop_continue` is 60 s, so a chain that moved on would show within this window.
const HOLD_WINDOW: Duration = Duration::from_secs(5);
const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;

/// The ladder of a Claude Code session with every rung installed and channel push on.
fn full_ladder() -> Value {
    json!([
        "channel_push",
        "mod_submit",
        "async_rewake_hook",
        "stop_continue",
        "hook_inject",
        "wait_tool"
    ])
}

/// The default stdio config with channel push off, so the ladder starts with the pull rungs.
fn push_off() -> EngineConfig {
    EngineConfig {
        channel_push: false,
        ..EngineConfig::stdio()
    }
}

/// Writes `$AGORA_HOME/capabilities.json` in the world home with `claude` installed.
fn install_claude(world: &World, claude: ClaudeCapabilities) {
    let file = CapabilitiesFile {
        claude: Some(claude),
        ..Default::default()
    };
    capabilities::save(world.paths(), &file).expect("save capabilities.json in the world home");
}

/// The hex public key of `ctx`: the directory name of its heartbeats.
fn pk_of(ctx: &Ctx) -> String {
    hex(&ctx.identity.public())
}

/// A stdio session for the identity `seed`, initialized as `client_name`. The agent must be
/// registered first, and any capabilities and heartbeats must already be written.
async fn session(world: &World, seed: u8, cfg: EngineConfig, client_name: &str) -> StdioClient {
    let mut client = StdioClient::start(world, world.ctx(seed), cfg).await;
    client.initialize(client_name).await;
    client
}

/// The `structuredContent` of `whoami`.
async fn whoami_of(client: &mut StdioClient) -> Value {
    client.call_tool("whoami", json!({})).await["structuredContent"].clone()
}

/// The `structuredContent` of `delivery_status`.
async fn status_of(client: &mut StdioClient) -> Value {
    client.call_tool("delivery_status", json!({})).await["structuredContent"].clone()
}

/// Polls `delivery_status` every `POLL` until `done` holds, at most `WAIT`; returns the status
/// that did. The timeout bounds the whole wait, so a status that never changes fails the test.
async fn status_until(
    client: &mut StdioClient,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let polled = async {
        loop {
            let status = status_of(client).await;
            if done(&status) {
                return status;
            }
            tokio::time::sleep(POLL).await;
        }
    };
    tokio::time::timeout(WAIT, polled)
        .await
        .unwrap_or_else(|_| panic!("delivery_status did not show {what} within {WAIT:?}"))
}

/// Whether `status.pending` lists the message (`CHANNEL`, `seq`).
fn pending_has(status: &Value, seq: i64) -> bool {
    pending_of(status, seq).is_some()
}

/// The `pending` entry of the message (`CHANNEL`, `seq`).
fn pending_of(status: &Value, seq: i64) -> Option<&Value> {
    status["pending"]
        .as_array()?
        .iter()
        .find(|entry| entry["channel"] == CHANNEL && entry["seq"] == seq)
}

/// The escalations recorded for the message (`CHANNEL`, `seq`), oldest first.
fn escalations_of(status: &Value, seq: i64) -> Vec<Value> {
    status["escalations"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter(|e| e["channel"] == CHANNEL && e["seq"] == seq)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The (mechanism, armed) of each escalation, in order.
fn rungs(escalations: &[Value]) -> Vec<(&str, bool)> {
    escalations
        .iter()
        .map(|e| {
            (
                e["mechanism"].as_str().unwrap_or_default(),
                e["armed"] == true,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claude_session_reports_the_ladder_its_capabilities_install_and_no_pending_state() {
    let world = World::start().await;
    world.owner().await;
    world.agent(2).await;
    install_claude(
        &world,
        ClaudeCapabilities {
            mods: true,
            async_rewake: true,
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        },
    );
    let mut client = session(&world, 2, EngineConfig::stdio(), "claude-code").await;

    let whoami = whoami_of(&mut client).await;
    assert_eq!(whoami["ladder"], full_ladder());
    let status = status_of(&mut client).await;
    assert_eq!(status["ladder"], full_ladder());
    assert_eq!(status["armed"], json!([]));
    assert_eq!(status["heartbeats"], json!({}));
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hook_inject_heartbeat_seen_ten_minutes_ago_puts_hook_inject_before_wait_tool() {
    let world = World::start().await;
    world.owner().await;
    let reader = world.agent(2).await;
    heartbeat::beat_for(
        world.paths(),
        &pk_of(&reader),
        Harness::ClaudeCode,
        Mechanism::HookInject,
        now_ms() - 10 * MINUTE_MS,
    )
    .expect("write the hook_inject heartbeat");
    let mut client = session(&world, 2, EngineConfig::stdio(), "claude-code").await;

    let whoami = whoami_of(&mut client).await;
    assert_eq!(
        whoami["ladder"],
        json!(["channel_push", "hook_inject", "wait_tool"])
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hook_inject_heartbeat_eight_days_old_is_not_in_the_ladder() {
    let world = World::start().await;
    world.owner().await;
    let reader = world.agent(2).await;
    heartbeat::beat_for(
        world.paths(),
        &pk_of(&reader),
        Harness::ClaudeCode,
        Mechanism::HookInject,
        now_ms() - 8 * DAY_MS,
    )
    .expect("write the hook_inject heartbeat");
    let mut client = session(&world, 2, EngineConfig::stdio(), "claude-code").await;

    let whoami = whoami_of(&mut client).await;
    assert_eq!(whoami["ladder"], json!(["channel_push", "wait_tool"]));
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_chain_fires_every_rung_in_one_tick_and_a_fresh_stop_heartbeat_holds_the_next() {
    let world = World::start().await;
    let owner = world.owner().await;
    let reader = world.agent(2).await;
    let poster = world.agent(3).await;
    world.channel(&owner, CHANNEL, &[&reader, &poster]).await;
    install_claude(
        &world,
        ClaudeCapabilities {
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        },
    );
    let marker = world.post(&poster, CHANNEL, "backlog marker").await;
    let mut client = session(&world, 2, push_off(), "claude-code").await;
    status_until(&mut client, "the backlog marker pending", |s| {
        pending_has(s, marker)
    })
    .await;

    // No heartbeat is fresh, so both pull rungs are unarmed and passed over in the same tick,
    // and the chain ends armed at wait_tool.
    let first = world.post(&poster, CHANNEL, "first message").await;
    let status = status_until(
        &mut client,
        "the first message's wait_tool escalation",
        |s| rungs(&escalations_of(s, first)).contains(&("wait_tool", true)),
    )
    .await;
    let escalations = escalations_of(&status, first);
    assert_eq!(
        rungs(&escalations),
        vec![
            ("stop_continue", false),
            ("hook_inject", false),
            ("wait_tool", true)
        ]
    );
    let ticks: BTreeSet<i64> = escalations
        .iter()
        .filter_map(|e| e["at_ms"].as_i64())
        .collect();
    assert_eq!(
        ticks.len(),
        1,
        "the chain fired in one tick: {escalations:?}"
    );
    assert!(
        escalations.iter().all(|e| e["executed"] == false),
        "no pull rung executes on the agent's behalf: {escalations:?}"
    );

    // A fresh stop_continue heartbeat arms that rung for the next tick. The chain stops there,
    // and the wait_tool-armed first message is now spent.
    heartbeat::beat_for(
        world.paths(),
        &pk_of(&reader),
        Harness::ClaudeCode,
        Mechanism::StopContinue,
        now_ms(),
    )
    .expect("write the stop_continue heartbeat");
    let second = world.post(&poster, CHANNEL, "second message").await;
    let status = status_until(
        &mut client,
        "the second message's armed stop_continue",
        |s| rungs(&escalations_of(s, second)).contains(&("stop_continue", true)),
    )
    .await;
    let spent = pending_of(&status, first).expect("the first message is still pending");
    assert_eq!(
        spent["exhausted"], true,
        "the spent chain is exhausted: {spent}"
    );

    let deadline = Instant::now() + HOLD_WINDOW;
    while Instant::now() < deadline {
        let status = status_of(&mut client).await;
        let held = escalations_of(&status, second);
        assert_eq!(
            rungs(&held),
            vec![("stop_continue", true)],
            "the chain moved past its armed stop_continue within {HOLD_WINDOW:?}"
        );
        tokio::time::sleep(POLL).await;
    }
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_claude_harness_counts_its_own_heartbeats_and_not_the_claude_entry() {
    let world = World::start().await;
    world.owner().await;
    let codex = world.agent(4).await;
    install_claude(
        &world,
        ClaudeCapabilities {
            mods: true,
            async_rewake: true,
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        },
    );
    for mechanism in [
        Mechanism::ModSubmit,
        Mechanism::AsyncRewakeHook,
        Mechanism::StopContinue,
        Mechanism::HookInject,
    ] {
        heartbeat::beat_for(
            world.paths(),
            &pk_of(&codex),
            Harness::Codex,
            mechanism,
            now_ms(),
        )
        .expect("write a heartbeat");
    }
    let mut client = session(&world, 4, EngineConfig::stdio(), "codex-mcp-client").await;

    // The Claude entry and the Claude-only heartbeats (mod, async rewake) do not reach Codex; the
    // seen stop and context heartbeats do (SPEC M6 1.4).
    let whoami = whoami_of(&mut client).await;
    assert_eq!(
        whoami["ladder"],
        json!(["stop_continue", "hook_inject", "wait_tool"])
    );
    client.close().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claude_stop_beat_does_not_arm_the_codex_stop_continue_rung() {
    let world = World::start().await;
    let owner = world.owner().await;
    let codex = world.agent(4).await;
    let poster = world.agent(3).await;
    world.channel(&owner, CHANNEL, &[&codex, &poster]).await;
    // Codex has its stop and context hooks installed, so both rungs are in its ladder unarmed.
    let mut file = CapabilitiesFile::default();
    file.set_harness(
        Harness::Codex,
        Some(HarnessCapabilities {
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        }),
    );
    capabilities::save(world.paths(), &file).expect("save capabilities.json in the world home");
    // A fresh stop beat written by Claude Code for the same identity: it must not reach Codex.
    heartbeat::beat_for(
        world.paths(),
        &pk_of(&codex),
        Harness::ClaudeCode,
        Mechanism::StopContinue,
        now_ms(),
    )
    .expect("write the Claude stop_continue heartbeat");
    let mut client = session(&world, 4, EngineConfig::stdio(), "codex-mcp-client").await;

    let seq = world.post(&poster, CHANNEL, "codex message").await;
    let status = status_until(
        &mut client,
        "the codex message's wait_tool escalation",
        |s| rungs(&escalations_of(s, seq)).contains(&("wait_tool", true)),
    )
    .await;
    assert_eq!(
        rungs(&escalations_of(&status, seq)),
        vec![
            ("stop_continue", false),
            ("hook_inject", false),
            ("wait_tool", true)
        ],
        "the Claude beat armed a Codex rung: {status}"
    );
    assert_eq!(status["armed"], json!([]), "{status}");
    client.close().await;
    world.shutdown().await;
}
