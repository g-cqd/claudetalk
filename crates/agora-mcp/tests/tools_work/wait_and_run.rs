//! `work_wait` on an unknown task, and the `work_run` tests that need the supervisor (ignored here).

use super::*;

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
