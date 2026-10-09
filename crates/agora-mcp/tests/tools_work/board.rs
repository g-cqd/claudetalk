//! The board: watchdog, metrics, quiet windows, escalation, card priority and the board summary extras.

use super::*;

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
