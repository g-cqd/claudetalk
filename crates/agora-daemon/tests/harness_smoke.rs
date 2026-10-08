//! Temporary smoke test for the daemon harness: start, world, group create, post, read, stop.

mod common;

use agora_proto::{Reply, Req, unb64};
use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_runs_a_group_post_and_read_end_to_end() {
    let d = start().await;
    let mut w = world(&d).await;

    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "hello bob").await;

    let reply = w
        .bob
        .ok(Req::Read {
            channel: "g".to_string(),
            after: 0,
            limit: 10,
        })
        .await;
    match reply {
        Reply::Messages { messages } => {
            assert_eq!(messages.len(), 1, "bob should see alice's one message");
            assert_eq!(messages[0].seq, seq);
            assert_eq!(messages[0].msg.sender, w.alice.pk_hex());
            let body = unb64(&messages[0].msg.body, "body").expect("body is base64");
            assert_eq!(body, b"hello bob");
        }
        other => panic!("expected Messages, got {other:?}"),
    }

    d.stop().await;
}
