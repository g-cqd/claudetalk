//! `work_schedule` create, list and delete round trip.

use super::*;

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
