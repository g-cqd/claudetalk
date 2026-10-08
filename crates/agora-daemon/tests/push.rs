//! Event push to subscribed connections (see SPEC Part 1, "Daemon push" and "Per-channel
//! waiter wake"): what a subscriber receives and in which order, membership refreshes,
//! the slow-consumer policy, presence, and the InboxWait relevance filter.

mod common;

use agora_proto::{
    ErrorBody, ErrorCode, Event, Reply, Req, Request, ServerFrame, read_json, write_json,
};
use common::*;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Bound on every wait in this file, so a daemon bug fails here instead of hanging.
const BOUND: Duration = Duration::from_secs(20);

/// Posts made while the slow subscriber does not read. The per-connection event queue
/// holds `EVENT_QUEUE` (256) and the socket buffers hold a few hundred more small frames,
/// so this many posts overrun it.
const OVERRUN_POSTS: usize = 1000;

/// Upper bound on the pages read back by the inbox walk: the store caps a page at 500.
const INBOX_PAGE: i64 = 500;

/// Next frame from `c`, within `BOUND`. Panics on EOF, a decode error or silence.
async fn frame(c: &mut TestClient) -> ServerFrame {
    match timeout(BOUND, read_json::<ServerFrame, _>(&mut c.stream)).await {
        Err(_) => panic!("no frame within {BOUND:?}"),
        Ok(Err(e)) => panic!("reading a frame failed: {e}"),
        Ok(Ok(None)) => panic!("the daemon closed the connection"),
        Ok(Ok(Some(f))) => f,
    }
}

/// Next frame on `c`, which must be an event.
async fn event(c: &mut TestClient) -> Event {
    match frame(c).await {
        ServerFrame::Event { event } => event,
        other => panic!("expected an event, got {other:?}"),
    }
}

/// Write `req` on `c` without reading anything back; returns its request id.
async fn send(c: &mut TestClient, req: Req) -> u64 {
    let id = c.next;
    c.next += 1;
    write_json(&mut c.stream, &Request { id, req })
        .await
        .expect("send request");
    id
}

/// Subscribe `c` to `channels` (empty = every membership) and read the reply, which must be
/// the next frame: the reply is queued before any event, so no event may come first.
async fn subscribe(c: &mut TestClient, channels: &[&str]) -> Result<Reply, ErrorBody> {
    let req = Req::Subscribe {
        channels: channels.iter().map(|ch| ch.to_string()).collect(),
    };
    let id = send(c, req).await;
    match frame(c).await {
        ServerFrame::Ok { id: got, reply } if got == id => Ok(reply),
        ServerFrame::Err { id: got, error } if got == id => Err(error),
        ServerFrame::Event { event } => {
            panic!("event {event:?} arrived before the Subscribe reply")
        }
        other => panic!("expected the reply to Subscribe {id} first, got {other:?}"),
    }
}

/// A Ping on `c` must be answered next: no event frame may come in front of the Pong.
async fn ping_is_next(c: &mut TestClient) {
    let id = send(c, Req::Ping).await;
    match frame(c).await {
        ServerFrame::Ok {
            id: got,
            reply: Reply::Pong,
        } if got == id => {}
        other => panic!("expected Pong for request {id} next, got {other:?}"),
    }
}

/// Asserts that `c` receives no frame for `window`. A closed connection also passes:
/// the point is that no event follows. Used after a bound wait, where a frame that
/// arrives late would still be a bug, so the window must exceed the daemon's delivery
/// latency (microseconds in practice) by a wide margin.
async fn assert_no_frame_within(c: &mut TestClient, window: Duration) {
    match timeout(window, read_json::<ServerFrame, _>(&mut c.stream)).await {
        Err(_) | Ok(Ok(None)) => {}
        Ok(Ok(Some(frame))) => panic!("expected no further frame, got {frame:?}"),
        Ok(Err(e)) => panic!("reading a frame failed: {e}"),
    }
}

/// `Req::Online` on `c`, unpacked.
async fn online(c: &mut TestClient) -> Vec<String> {
    match c.ok(Req::Online).await {
        Reply::Online { pubkeys } => pubkeys,
        other => panic!("expected Online, got {other:?}"),
    }
}

/// Owner adds `member` (hex pk) to plaintext `channel` as a member.
async fn add_member(owner: &mut TestClient, channel: &str, member: &str) {
    let arg = serde_json::json!({"pk": member, "role": "member"}).to_string();
    let op = owner.op("channel.add", channel, &arg);
    match owner.ok(Req::ChannelAdd { op, wrapped: None }).await {
        Reply::Unit => {}
        other => panic!("add {member} to {channel}: expected Unit, got {other:?}"),
    }
}

/// Owner removes `member` (hex pk) from plaintext `channel`.
async fn remove_member(owner: &mut TestClient, channel: &str, member: &str) {
    let arg = serde_json::json!({"pk": member}).to_string();
    let op = owner.op("channel.remove", channel, &arg);
    match owner
        .ok(Req::ChannelRemove {
            op,
            keys: Vec::new(),
        })
        .await
    {
        Reply::Unit => {}
        other => panic!("remove {member} from {channel}: expected Unit, got {other:?}"),
    }
}

/// Run an InboxWait on a task that owns `c`; the task hands the client back with the reply.
fn spawn_wait(
    mut c: TestClient,
    timeout_ms: u64,
) -> JoinHandle<(TestClient, Result<Reply, ErrorBody>)> {
    tokio::spawn(async move {
        let reply = c
            .call(Req::InboxWait {
                after: 0,
                timeout_ms,
                limit: 50,
            })
            .await;
        (c, reply)
    })
}

/// Await a spawned task within `BOUND`, panicking if it hangs or panicked.
async fn join<T: Send + 'static>(task: JoinHandle<T>) -> T {
    timeout(BOUND, task)
        .await
        .expect("task finished within BOUND")
        .expect("task did not panic")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribed_member_receives_its_message_event_after_the_subscribed_reply() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;

    let reply = subscribe(&mut bob, &[]).await.expect("bob subscribes");
    assert_eq!(
        reply,
        Reply::Subscribed {
            channels: vec!["g".to_string()]
        }
    );
    let seq = w.alice.post("g", "hello bob").await;

    assert_eq!(
        event(&mut bob).await,
        Event::MessageAppended {
            channel: "g".to_string(),
            seq
        }
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_event_arrives_only_once_its_seq_is_readable_on_another_connection() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");

    // The post runs on its own task so that the read below happens when the event
    // arrives, not after the Posted reply.
    let mut alice = w.alice;
    let poster = tokio::spawn(async move {
        let seq = alice.post("g", "committed first").await;
        (alice, seq)
    });
    let Event::MessageAppended { channel, seq } = event(&mut bob).await else {
        panic!("the first event for bob must be a MessageAppended");
    };
    assert_eq!(channel, "g");

    let mut reader = connect(&d, ident(3)).await;
    let readable = match reader
        .ok(Req::Read {
            channel: "g".to_string(),
            after: 0,
            limit: 10,
        })
        .await
    {
        Reply::Messages { messages } => messages.iter().any(|m| m.seq == seq),
        other => panic!("expected Messages, got {other:?}"),
    };
    assert!(
        readable,
        "event for seq {seq} arrived before the message was readable"
    );
    let (_alice, posted) = join(poster).await;
    assert_eq!(posted, seq, "the event carries the seq of the Posted reply");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_subscription_never_forwards_events_for_channels_the_subscriber_is_not_in() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("private", &[&w.alice, &w.bob]).await;
    w.owner.create_group("shared", &[&w.alice, &w.carol]).await;
    let mut carol = w.carol;
    let reply = subscribe(&mut carol, &[]).await.expect("carol subscribes");
    assert_eq!(
        reply,
        Reply::Subscribed {
            channels: vec!["shared".to_string()]
        }
    );

    w.alice.post("private", "not for carol").await;
    let seq = w.alice.post("shared", "for carol").await;

    // Commit order: the private post came first, so any leak would be the first event.
    assert_eq!(
        event(&mut carol).await,
        Event::MessageAppended {
            channel: "shared".to_string(),
            seq
        }
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_subscription_forwards_only_its_listed_channels() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("a", &[&w.alice, &w.bob]).await;
    w.owner.create_group("b", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    let reply = subscribe(&mut bob, &["a"])
        .await
        .expect("bob subscribes to a");
    assert_eq!(
        reply,
        Reply::Subscribed {
            channels: vec!["a".to_string()]
        }
    );

    w.alice.post("b", "not listed").await;
    let seq = w.alice.post("a", "listed").await;

    assert_eq!(
        event(&mut bob).await,
        Event::MessageAppended {
            channel: "a".to_string(),
            seq
        }
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribing_to_a_channel_not_joined_or_missing_is_refused_as_not_member_naming_it() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("private", &[&w.alice, &w.bob]).await;
    let mut carol = w.carol;

    let not_joined = subscribe(&mut carol, &["private"]).await;
    let missing = subscribe(&mut carol, &["nope"]).await;

    // Refused attempts do not use up the connection's one subscription: the second
    // attempt is judged on its own channel, not refused as "already subscribed".
    for (result, channel) in [(not_joined, "private"), (missing, "nope")] {
        match result {
            Err(error) => {
                assert_eq!(error.code, ErrorCode::NotMember);
                assert_eq!(error.message, channel);
            }
            Ok(reply) => panic!("subscribe to {channel} must be refused, got {reply:?}"),
        }
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_subscribe_on_the_same_connection_is_invalid() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("first subscribe");

    match subscribe(&mut bob, &[]).await {
        Err(error) => assert_eq!(error.code, ErrorCode::Invalid),
        Ok(reply) => panic!("a second subscribe must be refused, got {reply:?}"),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_that_never_subscribed_receives_no_events() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut subscribed = w.bob;
    subscribe(&mut subscribed, &[])
        .await
        .expect("bob subscribes");
    // A second connection of the same identity that never subscribes.
    let mut silent = connect(&d, ident(4)).await;

    let seq = w.alice.post("g", "hello").await;
    assert_eq!(
        event(&mut subscribed).await,
        Event::MessageAppended {
            channel: "g".to_string(),
            seq
        }
    );
    // Structural guarantee: events reach a connection only through its forwarder, and a
    // forwarder exists only after that connection subscribed, so the silent connection
    // has no path to receive one. The Ping is sent after the post, so its Pong fixes the
    // point in time without a sleep, and the check fails if an event was written ahead.
    ping_is_next(&mut silent).await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_gained_and_lost_is_announced_and_follows_the_subscription() {
    let d = start().await;
    let mut w = world(&d).await;
    let carol_pk = w.carol.pk_hex();
    w.owner.create_group("c", &[&w.alice]).await;
    w.owner.create_group("d", &[&w.alice, &w.carol]).await;
    let mut carol = w.carol;
    let reply = subscribe(&mut carol, &[]).await.expect("carol subscribes");
    assert_eq!(
        reply,
        Reply::Subscribed {
            channels: vec!["d".to_string()]
        }
    );

    add_member(&mut w.owner, "c", &carol_pk).await;
    assert_eq!(
        event(&mut carol).await,
        Event::ChannelChanged {
            channel: "c".to_string()
        }
    );
    let joined = w.alice.post("c", "after joining").await;
    assert_eq!(
        event(&mut carol).await,
        Event::MessageAppended {
            channel: "c".to_string(),
            seq: joined
        }
    );

    remove_member(&mut w.owner, "c", &carol_pk).await;
    assert_eq!(
        event(&mut carol).await,
        Event::ChannelChanged {
            channel: "c".to_string()
        }
    );
    w.alice.post("c", "after leaving").await;
    // A message in a channel carol stays in is the barrier: the next event must be it,
    // so no event for "c" was forwarded after the removal.
    let barrier = w.alice.post("d", "barrier").await;
    assert_eq!(
        event(&mut carol).await,
        Event::MessageAppended {
            channel: "d".to_string(),
            seq: barrier
        }
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acks_are_forwarded_for_the_subscribers_own_identity_only() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");
    // Bob's second connection acks; alice acks on her own connection first.
    let mut bob_again = connect(&d, ident(4)).await;
    let bob_pk = bob_again.pk_hex();

    let seq = w.alice.post("g", "please ack").await;
    assert_eq!(
        event(&mut bob).await,
        Event::MessageAppended {
            channel: "g".to_string(),
            seq
        }
    );
    w.alice
        .ok(Req::Ack {
            channel: "g".to_string(),
            seq,
        })
        .await;
    bob_again
        .ok(Req::Ack {
            channel: "g".to_string(),
            seq,
        })
        .await;

    // Alice's ack was committed first; if it had been forwarded it would come first here.
    assert_eq!(
        event(&mut bob).await,
        Event::Acked {
            reader: bob_pk,
            channel: "g".to_string(),
            seq,
        }
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overrun_subscriber_gets_lagged_and_its_inbox_keeps_every_message() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");

    // Bob does not read while alice posts.
    let mut posted = Vec::with_capacity(OVERRUN_POSTS);
    for i in 0..OVERRUN_POSTS {
        posted.push(w.alice.post("g", &format!("burst {i}")).await);
    }

    // Every post is either delivered or counted once by a Lagged marker.
    let mut delivered: Vec<i64> = Vec::new();
    let mut dropped: u64 = 0;
    let mut saw_lagged = false;
    while (delivered.len() as u64) + dropped < OVERRUN_POSTS as u64 {
        match frame(&mut bob).await {
            ServerFrame::Event {
                event: Event::MessageAppended { channel, seq },
            } => {
                assert_eq!(channel, "g");
                delivered.push(seq);
            }
            ServerFrame::Event {
                event: Event::Lagged { dropped: n },
            } => {
                assert!(n > 0, "a Lagged marker reports at least one drop");
                saw_lagged = true;
                dropped += n;
            }
            other => panic!("unexpected frame for the overrun subscriber: {other:?}"),
        }
    }
    assert_eq!(
        delivered.len() as u64 + dropped,
        OVERRUN_POSTS as u64,
        "delivered plus dropped must account for every post, with no duplicates"
    );
    assert!(
        delivered.windows(2).all(|pair| pair[0] < pair[1]),
        "delivered events keep commit order"
    );
    // Weaker outcome allowed when the socket buffers absorbed the whole burst: then
    // nothing was dropped and every event arrived, in order.
    assert!(
        saw_lagged || delivered == posted,
        "without a Lagged marker every event must have arrived"
    );

    // The connection still serves requests.
    assert_eq!(bob.ok(Req::Ping).await, Reply::Pong);

    // The inbox (cursor-based, not notification-based) still holds every message: walk
    // it page by page on another connection of bob's identity, acking each page.
    let mut reader = connect(&d, ident(4)).await;
    let mut seen: Vec<i64> = Vec::new();
    for _ in 0..(OVERRUN_POSTS / INBOX_PAGE as usize + 2) {
        let page = match reader.ok(Req::Inbox { limit: INBOX_PAGE }).await {
            Reply::Inbox { messages, .. } => messages,
            other => panic!("expected Inbox, got {other:?}"),
        };
        let Some(last) = page.last().map(|m| m.seq) else {
            break;
        };
        seen.extend(page.iter().map(|m| m.seq));
        reader
            .ok(Req::Ack {
                channel: "g".to_string(),
                seq: last,
            })
            .await;
    }
    assert_eq!(seen, posted, "the inbox holds every message in order");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_lists_a_subscribed_identity_to_a_peer_sharing_a_channel() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let bob_pk = w.bob.pk_hex();

    assert_eq!(
        online(&mut w.alice).await,
        Vec::<String>::new(),
        "nobody is online before a subscription"
    );
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");

    assert_eq!(online(&mut w.alice).await, vec![bob_pk]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_never_lists_an_identity_to_a_peer_sharing_no_channel() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");

    // Carol is online too, so her list is not empty for the wrong reason.
    let mut carol = w.carol;
    subscribe(&mut carol, &[]).await.expect("carol subscribes");
    let carol_pk = carol.pk_hex();
    assert_eq!(
        online(&mut carol).await,
        vec![carol_pk],
        "carol sees herself and not bob, who shares no channel with her"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_drops_an_identity_when_its_subscribed_connection_closes() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let bob_pk = w.bob.pk_hex();
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");
    assert_eq!(online(&mut w.alice).await, vec![bob_pk]);

    drop(bob);
    // Presence has no event of its own. The daemon clears it when the connection's
    // forwarder ends, which runs after the close is noticed and is not ordered with
    // this client's next request. So the test asks again until the answer settles,
    // within a 2 s bound (the daemon takes milliseconds; the bound only fails a bug).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let seen = online(&mut w.alice).await;
        if seen.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bob is still online 2 s after his subscribed connection closed: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_subscriber_gets_its_notice_then_no_more_events() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let bob_pk = w.bob.pk_hex();
    let mut bob = w.bob;
    subscribe(&mut bob, &[]).await.expect("bob subscribes");
    assert_eq!(online(&mut w.alice).await, vec![bob_pk.clone()]);

    // Revocation deletes bob's memberships, so his membership re-read fails. The
    // forwarder must fail closed: forward the notice once, then end.
    let op = w.owner.op("identity.revoke", &bob_pk, "");
    assert_eq!(w.owner.ok(Req::Revoke { op }).await, Reply::Unit);
    assert_eq!(
        event(&mut bob).await,
        Event::IdentityChanged {
            pubkey: bob_pk.clone()
        }
    );

    // Alice posts in the channel bob was in. A subscription that kept its old membership
    // would forward this; the fixed one has ended, so nothing may arrive.
    w.alice.post("g", "after the revocation").await;
    assert_no_frame_within(&mut bob, Duration::from_millis(500)).await;

    // Online needs a shared channel, and bob has none left, so he is absent for that
    // reason too. The bounded poll checks that presence is gone as well: it is cleared
    // when the forwarder ends, which is not ordered with alice's next request.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !online(&mut w.alice).await.contains(&bob_pk) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bob is still listed as online 2 s after his revocation"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_stays_parked_for_messages_in_channels_it_is_not_in() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    w.owner.create_group("other", &[&w.alice, &w.carol]).await;
    let waiting = spawn_wait(w.bob, 1_000);

    w.alice.post("other", "not bob's channel").await;

    let (_bob, reply) = join(waiting).await;
    match reply {
        Ok(Reply::Inbox {
            messages,
            cursor,
            timed_out,
        }) => {
            assert!(timed_out, "the wait must run to its timeout");
            assert!(messages.is_empty());
            assert_eq!(cursor, 0);
        }
        other => panic!("expected an Inbox reply, got {other:?}"),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_wait_returns_the_message_in_its_own_channel_after_other_traffic() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    w.owner.create_group("other", &[&w.alice, &w.carol]).await;
    let waiting = spawn_wait(w.bob, 10_000);

    w.alice.post("other", "not bob's channel").await;
    let seq = w.alice.post("g", "bob's channel").await;

    let (_bob, reply) = join(waiting).await;
    match reply {
        Ok(Reply::Inbox {
            messages,
            cursor,
            timed_out,
        }) => {
            assert!(
                !timed_out,
                "the message in bob's channel must wake the wait"
            );
            assert_eq!(
                messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
                vec![seq]
            );
            assert_eq!(cursor, seq);
        }
        other => panic!("expected an Inbox reply, got {other:?}"),
    }
    d.stop().await;
}
