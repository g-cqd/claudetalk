//! Event-driven `InboxWait` (see SPEC "inbox_wait"): wakeups, timeouts, cursors, acks,
//! lost-wakeup races, concurrent posts, and connection multiplexing.

mod common;

use agora_proto::{MessageView, Reply, Req, Request, ServerFrame, read_json, write_json};
use common::*;
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Bound on every await in this file, so a daemon bug fails here instead of hanging.
/// Larger than every `timeout_ms` used below.
const BOUND: Duration = Duration::from_secs(20);

/// Upper bound on pages read back in the concurrency test; the store caps a page at 500.
const MAX_PAGES: usize = 8;

/// The fields of `Reply::Inbox`, unpacked.
struct Inbox {
    messages: Vec<MessageView>,
    cursor: i64,
    timed_out: bool,
}

fn as_inbox(reply: Reply) -> Inbox {
    match reply {
        Reply::Inbox {
            messages,
            cursor,
            timed_out,
        } => Inbox {
            messages,
            cursor,
            timed_out,
        },
        other => panic!("expected Inbox, got {other:?}"),
    }
}

fn seqs(inbox: &Inbox) -> Vec<i64> {
    inbox.messages.iter().map(|m| m.seq).collect()
}

/// One InboxWait on `c`, returning the reply and how long the call took.
async fn wait_on(c: &mut TestClient, after: i64, timeout_ms: u64) -> (Inbox, Duration) {
    let started = Instant::now();
    let reply = c
        .call(Req::InboxWait {
            after,
            timeout_ms,
            limit: 50,
        })
        .await;
    let elapsed = started.elapsed();
    match reply {
        Ok(reply) => (as_inbox(reply), elapsed),
        Err(error) => panic!("InboxWait failed: {error}"),
    }
}

/// Run one InboxWait on a task that owns `c`; the task hands the client back with the result.
fn spawn_wait(
    mut c: TestClient,
    after: i64,
    timeout_ms: u64,
) -> JoinHandle<(TestClient, Inbox, Duration)> {
    tokio::spawn(async move {
        let (inbox, elapsed) = wait_on(&mut c, after, timeout_ms).await;
        (c, inbox, elapsed)
    })
}

/// Await a spawned task within `BOUND`, panicking if it hangs or panicked.
async fn join<T: Send + 'static>(task: JoinHandle<T>) -> T {
    timeout(BOUND, task)
        .await
        .expect("task finished within BOUND")
        .expect("task did not panic")
}

/// Read one server frame within `BOUND`.
async fn read_frame(stream: &mut UnixStream) -> ServerFrame {
    timeout(BOUND, read_json::<ServerFrame, _>(stream))
        .await
        .expect("frame arrived within BOUND")
        .expect("frame decodes")
        .expect("connection stayed open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_waiter_wakes_when_a_message_lands() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;

    let waiter = spawn_wait(w.bob, 0, 10_000);
    // Let bob's wait park before alice posts; the assertions hold either way.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seq = w.alice.post("g", "wake up").await;
    let (_bob, inbox, elapsed) = join(waiter).await;

    assert!(
        !inbox.timed_out,
        "the wait timed out although a message landed"
    );
    assert_eq!(seqs(&inbox), vec![seq]);
    assert_eq!(inbox.cursor, seq);
    assert!(
        elapsed < Duration::from_secs(3),
        "the wake took {elapsed:?}, expected well under the 10 s timeout"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_returns_immediately_when_a_message_is_already_unacked() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "already here").await;

    let (inbox, elapsed) = wait_on(&mut w.bob, 0, 10_000).await;

    assert!(!inbox.timed_out);
    assert_eq!(seqs(&inbox), vec![seq]);
    assert_eq!(inbox.cursor, seq);
    assert!(
        elapsed < Duration::from_secs(3),
        "a present message should not wait, took {elapsed:?}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_times_out_with_an_empty_inbox_and_cursor_equal_to_after() {
    let d = start().await;
    let mut w = world(&d).await;

    let (inbox, elapsed) = wait_on(&mut w.bob, 42, 300).await;

    assert!(inbox.timed_out);
    assert!(inbox.messages.is_empty());
    assert_eq!(inbox.cursor, 42, "a timed-out reply echoes `after`");
    assert!(
        elapsed >= Duration::from_millis(300),
        "returned before its timeout: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "a 300 ms wait took {elapsed:?}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_past_the_returned_cursor_does_not_return_the_same_message_again() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "seen once").await;
    let (first, _) = wait_on(&mut w.bob, 0, 10_000).await;
    assert_eq!(seqs(&first), vec![seq]);

    let (again, elapsed) = wait_on(&mut w.bob, first.cursor, 200).await;

    assert!(
        again.timed_out,
        "the message at the cursor was returned again"
    );
    assert!(again.messages.is_empty());
    assert_eq!(again.cursor, first.cursor);
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_messages_do_not_wake_the_sender_waiter() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    // A second alice connection posts while the first one waits.
    let mut alice_poster = connect(&d, ident(3)).await;

    let waiter = spawn_wait(w.alice, 0, 500);
    tokio::time::sleep(Duration::from_millis(100)).await;
    alice_poster.post("g", "talking to myself").await;
    let (_alice, inbox, _) = join(waiter).await;

    assert!(inbox.timed_out, "the sender's own message woke its waiter");
    assert!(inbox.messages.is_empty());
    assert_eq!(inbox.cursor, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_messages_are_not_returned_by_a_later_wait() {
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "read and acked").await;
    let (first, _) = wait_on(&mut w.bob, 0, 10_000).await;
    assert_eq!(seqs(&first), vec![seq]);
    let acked = w
        .bob
        .ok(Req::Ack {
            channel: "g".to_string(),
            seq,
        })
        .await;
    assert_eq!(acked, Reply::Unit);

    let (after_ack, elapsed) = wait_on(&mut w.bob, 0, 200).await;

    assert!(after_ack.timed_out, "an acked message was returned again");
    assert!(after_ack.messages.is_empty());
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_wakeup_is_lost_when_a_post_races_the_wait() {
    const ITERATIONS: u64 = 50;
    let d = start().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut bob = w.bob;
    let mut cursor = 0;

    for i in 0..ITERATIONS {
        let waiter = spawn_wait(bob, cursor, 5_000);
        // Deterministic 0..=3 ms jitter moves the post across bob's check and subscribe.
        tokio::time::sleep(Duration::from_millis(i % 4)).await;
        let seq = w.alice.post("g", &format!("race {i}")).await;
        let (returned, inbox, _) = join(waiter).await;
        bob = returned;

        assert!(!inbox.timed_out, "iteration {i}: the wakeup was lost");
        assert_eq!(seqs(&inbox), vec![seq], "iteration {i}");
        cursor = inbox.cursor;
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_posts_from_twenty_clients_are_all_stored_once_in_seq_order() {
    const CLIENTS: u8 = 20;
    const POSTS_EACH: usize = 50;
    let d = start().await;
    let mut owner = connect(&d, ident(1)).await;
    owner.ok(Req::BootstrapOwner).await;
    let mut clients = Vec::new();
    for seed in 20..20 + CLIENTS {
        let mut c = connect(&d, ident(seed)).await;
        assert_eq!(
            c.ok(Req::Register {
                label: format!("client{seed}"),
            })
            .await,
            Reply::Unit
        );
        clients.push(c);
    }
    let senders: Vec<String> = clients.iter().map(TestClient::pk_hex).collect();
    let members: Vec<&TestClient> = clients.iter().collect();
    owner.create_group("g", &members).await;

    let posting: Vec<JoinHandle<(String, Vec<i64>)>> = clients
        .into_iter()
        .map(|mut c| {
            tokio::spawn(async move {
                let mut seqs = Vec::with_capacity(POSTS_EACH);
                for n in 0..POSTS_EACH {
                    seqs.push(c.post("g", &format!("msg {n}")).await);
                }
                (c.pk_hex(), seqs)
            })
        })
        .collect();

    // seq -> the sender that posted it, from the post replies.
    let mut posted: BTreeMap<i64, String> = BTreeMap::new();
    for task in posting {
        let (sender, seqs) = join(task).await;
        assert_eq!(seqs.len(), POSTS_EACH);
        for seq in seqs {
            assert!(
                posted.insert(seq, sender.clone()).is_none(),
                "seq {seq} issued twice"
            );
        }
    }
    assert_eq!(posted.len(), CLIENTS as usize * POSTS_EACH);

    let mut read_back: Vec<MessageView> = Vec::new();
    let mut reached_end = false;
    let mut after = 0;
    for _ in 0..MAX_PAGES {
        let page = match owner
            .ok(Req::Read {
                channel: "g".to_string(),
                after,
                limit: 500,
            })
            .await
        {
            Reply::Messages { messages } => messages,
            other => panic!("expected Messages, got {other:?}"),
        };
        match page.last() {
            None => {
                reached_end = true;
                break;
            }
            Some(last) => after = last.seq,
        }
        read_back.extend(page);
    }

    assert!(reached_end, "paged read did not reach an empty page");
    assert_eq!(read_back.len(), posted.len(), "stored message count");
    let ids: HashSet<&str> = read_back.iter().map(|m| m.msg.id.as_str()).collect();
    assert_eq!(ids.len(), read_back.len(), "message ids are not unique");
    assert!(
        read_back.windows(2).all(|w| w[0].seq < w[1].seq),
        "seqs are not strictly increasing"
    );
    for m in &read_back {
        assert_eq!(
            posted.get(&m.seq),
            Some(&m.msg.sender),
            "seq {} was not posted by {}",
            m.seq,
            m.msg.sender
        );
    }
    for sender in &senders {
        let count = read_back.iter().filter(|m| &m.msg.sender == sender).count();
        assert_eq!(count, POSTS_EACH, "messages stored for {sender}");
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_wait_does_not_block_a_ping_on_a_second_connection() {
    let d = start().await;
    let w = world(&d).await;
    let mut bob_second = connect(&d, ident(4)).await;

    let waiter = spawn_wait(w.bob, 0, 2_000);
    let pong = bob_second.ok(Req::Ping).await;

    assert_eq!(pong, Reply::Pong);
    assert!(
        !waiter.is_finished(),
        "the ping was answered only after the 2 s wait had already returned"
    );
    let (_bob, inbox, _) = join(waiter).await;
    assert!(inbox.timed_out);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_wait_does_not_block_a_later_frame_on_the_same_connection() {
    let d = start().await;
    let mut w = world(&d).await;
    let bob = &mut w.bob;

    write_json(
        &mut bob.stream,
        &Request {
            id: 1,
            req: Req::InboxWait {
                after: 0,
                timeout_ms: 2_000,
                limit: 50,
            },
        },
    )
    .await
    .expect("send InboxWait");
    write_json(
        &mut bob.stream,
        &Request {
            id: 2,
            req: Req::Ping,
        },
    )
    .await
    .expect("send Ping");

    let first = read_frame(&mut bob.stream).await;
    assert_eq!(
        first,
        ServerFrame::Ok {
            id: 2,
            reply: Reply::Pong,
        },
        "the Ping behind a parked wait must be answered first"
    );
    match read_frame(&mut bob.stream).await {
        ServerFrame::Ok {
            id: 1,
            reply:
                Reply::Inbox {
                    messages,
                    cursor,
                    timed_out,
                },
        } => {
            assert!(timed_out);
            assert!(messages.is_empty());
            assert_eq!(cursor, 0);
        }
        other => panic!("expected the Inbox reply to id 1, got {other:?}"),
    }
    d.stop().await;
}
