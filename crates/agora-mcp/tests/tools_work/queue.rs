//! The command queue and its acceptance: `work_add`, `work_next`, `work_show`, `work_accept`, and the author refusal.

use super::*;

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
