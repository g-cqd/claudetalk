//! Refusals of the local-only calls over HTTP (D16), including `work_accept` of a command.

use super::*;

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
