//! The tool catalogue: the 60 tools of `tools/list` and the spec annotations of the work and scheduling tools.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_list_serves_all_60_tools() {
    let world = World::start().await;
    let mut client = session(&world, ALICE).await;

    let response = client.request("tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list has no tools array: {response}"));

    assert_eq!(tools.len(), 60);
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
