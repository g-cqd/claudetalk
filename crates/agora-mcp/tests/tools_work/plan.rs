//! The given-tasks plan through `work_plan`: the differential fixture and the agent cap.

use super::*;

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
