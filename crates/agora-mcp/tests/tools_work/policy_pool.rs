//! Policy and pool tools: the owner-only `policy_set`, and pool capacity with leases and releases.

use super::*;

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
