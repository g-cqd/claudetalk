//! Benchmarks for the daemon's idle CPU cost and message latency. Every test is ignored by
//! default; run with `cargo test -p agora-daemon --test bench -- --ignored --nocapture`.
//! Results are printed as `BENCH <name> <value> <unit>` lines.

mod common;

use agora_daemon::now_ms;
use agora_proto::{ErrorBody, MessageWire, Reply, Req};
use agora_store::SignedMessage;
use common::*;
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Upper bound on every await in this file, so a daemon bug fails here instead of hanging.
const BOUND: Duration = Duration::from_secs(30);

/// `getrusage(RUSAGE_SELF)` counts the whole process, so the benchmarks must not overlap
/// even when the test harness runs them in parallel; each one holds this for its duration.
static EXCLUSIVE: Mutex<()> = Mutex::const_new(());

async fn exclusive() -> MutexGuard<'static, ()> {
    timeout(BOUND, EXCLUSIVE.lock())
        .await
        .expect("benchmark lock acquired within BOUND")
}

/// Await a spawned task within `BOUND`, panicking if it hangs or panicked.
async fn join<T: Send + 'static>(task: JoinHandle<T>) -> T {
    timeout(BOUND, task)
        .await
        .expect("task finished within BOUND")
        .expect("task did not panic")
}

fn timeval_ms(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0
}

/// User + system CPU time consumed by this process so far, in milliseconds.
fn cpu_ms() -> f64 {
    let mut ru = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `ru` is a writable, zero-initialised `rusage`, and `RUSAGE_SELF` is a valid
    // target. On success getrusage writes every field, and every field is an integer, so the
    // zeroed bytes were already a valid `rusage` before the call.
    let ru = unsafe {
        assert_eq!(
            libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()),
            0,
            "getrusage failed"
        );
        ru.assume_init()
    };
    timeval_ms(ru.ru_utime) + timeval_ms(ru.ru_stime)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile (1-based rank = ceil(n * pct / 100)); sorts `samples` in place.
fn percentile(samples: &mut [Duration], pct: usize) -> Duration {
    assert!(!samples.is_empty(), "percentile of no samples");
    samples.sort_unstable();
    let rank = (samples.len() * pct).div_ceil(100);
    samples[rank.max(1) - 1]
}

/// A plaintext message on `channel` from `client`, with a caller-chosen id.
fn bench_message(client: &TestClient, channel: &str, id: &str) -> MessageWire {
    let msg = SignedMessage {
        id: id.to_string(),
        channel: channel.to_string(),
        thread_of: None,
        sender: [0; 32],
        ts: now_ms(),
        encrypted: false,
        epoch: 0,
        body: format!("bench {id}").into_bytes(),
        attachments: vec![],
        sig: vec![],
    }
    .sign(&client.id);
    MessageWire::from_message(&msg)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run with --ignored --nocapture"]
async fn idle_cpu_with_20_idle_connections() {
    const PARKED: usize = 10;
    const IDLE_WINDOW: Duration = Duration::from_secs(3);
    const MAX_CPU_MS: f64 = 150.0;

    let _exclusive = exclusive().await;
    let d = start().await;
    let mut clients = Vec::new();
    for seed in 20..40u8 {
        let mut c = connect(&d, ident(seed)).await;
        assert_eq!(
            c.ok(Req::Register {
                label: format!("idle{seed}"),
            })
            .await,
            Reply::Unit
        );
        clients.push(c);
    }

    // The first PARKED clients wait on their own tasks; the rest stay connected and silent.
    let parked: Vec<JoinHandle<Result<Reply, ErrorBody>>> = clients
        .drain(..PARKED)
        .map(|mut c| {
            tokio::spawn(async move {
                c.call(Req::InboxWait {
                    after: 0,
                    timeout_ms: 5_000,
                    limit: 50,
                })
                .await
            })
        })
        .collect();

    let before = cpu_ms();
    tokio::time::sleep(IDLE_WINDOW).await;
    let after = cpu_ms();
    let used = after - before;

    assert!(
        parked.iter().all(|h| !h.is_finished()),
        "a parked InboxWait returned inside the idle window, so the window measured no waiting"
    );
    println!("BENCH idle_cpu_ms {used:.1} ms");
    assert!(
        used < MAX_CPU_MS,
        "idle daemon used {used:.1} ms of CPU over {IDLE_WINDOW:?}, bound is {MAX_CPU_MS} ms"
    );

    for handle in parked {
        match join(handle).await.expect("InboxWait succeeds") {
            Reply::Inbox {
                messages,
                cursor,
                timed_out,
            } => {
                assert!(timed_out, "parked wait returned without its timeout");
                assert!(messages.is_empty());
                assert_eq!(cursor, 0);
            }
            other => panic!("expected Inbox, got {other:?}"),
        }
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run with --ignored --nocapture"]
async fn message_latency_p50_p99() {
    const MESSAGES: usize = 500;
    const CHANNEL: &str = "bench";
    const MAX_POST_P99: Duration = Duration::from_millis(50);
    const MAX_WAKE_P99: Duration = Duration::from_millis(100);

    let _exclusive = exclusive().await;
    let d = start().await;
    let w = world(&d).await;
    let World {
        mut owner,
        mut alice,
        bob,
        ..
    } = w;
    owner.create_group(CHANNEL, &[&alice, &bob]).await;

    // Bob parks in InboxWait on his own task and records when each message reaches him.
    let waiter: JoinHandle<HashMap<String, Instant>> = tokio::spawn(async move {
        let mut bob = bob;
        let mut woke = HashMap::with_capacity(MESSAGES);
        let mut cursor = 0;
        while woke.len() < MESSAGES {
            let reply = bob
                .call(Req::InboxWait {
                    after: cursor,
                    timeout_ms: 5_000,
                    limit: 100,
                })
                .await
                .expect("InboxWait succeeds");
            let received = Instant::now();
            match reply {
                Reply::Inbox {
                    messages,
                    cursor: next,
                    timed_out,
                } => {
                    assert!(
                        !timed_out,
                        "bob's wait timed out with {} of {MESSAGES} received",
                        woke.len()
                    );
                    for m in messages {
                        assert!(
                            woke.insert(m.msg.id, received).is_none(),
                            "a message was delivered to bob twice"
                        );
                    }
                    cursor = next;
                }
                other => panic!("expected Inbox, got {other:?}"),
            }
        }
        woke
    });

    // Alice posts sequentially, timing each round trip and noting when she sent it.
    let mut post_rtts = Vec::with_capacity(MESSAGES);
    let mut sent: Vec<(String, Instant)> = Vec::with_capacity(MESSAGES);
    for i in 0..MESSAGES {
        let id = format!("bench-{i}");
        let req = Req::Post {
            msg: bench_message(&alice, CHANNEL, &id),
        };
        let sent_at = Instant::now();
        match alice.ok(req).await {
            Reply::Posted { .. } => {}
            other => panic!("post: expected Posted, got {other:?}"),
        }
        post_rtts.push(sent_at.elapsed());
        sent.push((id, sent_at));
    }

    let woke = join(waiter).await;
    let mut wake_latencies: Vec<Duration> = sent
        .iter()
        .map(|(id, sent_at)| {
            let received = woke.get(id).expect("bob received every message");
            received.duration_since(*sent_at)
        })
        .collect();

    let post_p50 = percentile(&mut post_rtts, 50);
    let post_p99 = percentile(&mut post_rtts, 99);
    let wake_p50 = percentile(&mut wake_latencies, 50);
    let wake_p99 = percentile(&mut wake_latencies, 99);
    println!("BENCH messages {MESSAGES} count");
    println!("BENCH post_rtt_p50 {:.3} ms", ms(post_p50));
    println!("BENCH post_rtt_p99 {:.3} ms", ms(post_p99));
    println!("BENCH wake_p50 {:.3} ms", ms(wake_p50));
    println!("BENCH wake_p99 {:.3} ms", ms(wake_p99));

    assert!(
        post_p99 < MAX_POST_P99,
        "p99 post round trip {:.3} ms exceeds {} ms",
        ms(post_p99),
        ms(MAX_POST_P99)
    );
    assert!(
        wake_p99 < MAX_WAKE_P99,
        "p99 post-send to bob-wake {:.3} ms exceeds {} ms",
        ms(wake_p99),
        ms(MAX_WAKE_P99)
    );
    d.stop().await;
}
