//! Smoke tests of the shared harness (`tests/common`) over the real daemon and client verbs:
//! the world starts, a message posts, a member reads it, and the daemon stops.

mod common;

use agora_client::api::chat::{self, InboxParams};
use common::World;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posting_to_a_channel_returns_a_positive_seq() {
    let world = World::start().await;
    let owner = world.owner().await;
    let agent = world.agent(2).await;
    world.channel(&owner, "general", &[&agent]).await;

    let seq = world.post(&agent, "general", "hello").await;

    assert!(seq > 0, "a posted message gets a positive seq, got {seq}");
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_inbox_returns_the_posted_message() {
    let world = World::start().await;
    let owner = world.owner().await;
    let agent = world.agent(2).await;
    world.channel(&owner, "general", &[&agent]).await;
    let seq = world.post(&owner, "general", "hello from the owner").await;

    let out = chat::inbox(
        &agent,
        InboxParams {
            wait: None,
            after: 0,
            limit: 50,
        },
    )
    .await
    .expect("read the member's inbox");
    let seen: Vec<(String, i64, String)> = out.json["messages"]
        .as_array()
        .expect("inbox has a messages array")
        .iter()
        .map(|m| {
            (
                m["channel"].as_str().expect("channel").to_owned(),
                m["seq"].as_i64().expect("seq"),
                m["text"].as_str().expect("text").to_owned(),
            )
        })
        .collect();

    assert_eq!(
        seen,
        vec![("general".to_owned(), seq, "hello from the owner".to_owned())]
    );
    world.shutdown().await;
}
