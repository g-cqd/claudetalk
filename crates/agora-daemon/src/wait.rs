//! Event-driven `inbox_wait` (no polling). See SPEC "inbox_wait".

use crate::{Conn, Shared, dispatch};
use agora_proto::{ErrorBody, ErrorCode, Event, MessageView, Reply};
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The InboxWait request. Clamps timeout to MAX_WAIT_MS, subscribes to the bus
/// FIRST, then runs `wait_for` with a check that reads `list_channels(conn.pubkey)`
/// and `inbox_after(conn.pubkey, after, limit)` in one reader exec. The channel
/// ids from that read form the member set that decides which MessageAppended
/// events wake the waiter. Returns Reply::Inbox{messages, cursor, timed_out}; on
/// timeout messages is empty, cursor == after, timed_out true.
pub async fn inbox_wait(
    sh: &Shared,
    conn: &Conn,
    after: i64,
    timeout_ms: u64,
    limit: i64,
) -> Result<Reply, ErrorBody> {
    let timeout = Duration::from_millis(timeout_ms.min(agora_proto::MAX_WAIT_MS));
    // Subscribe before the first check: a commit that lands after the check
    // publishes its event only after it is durable, so the event is already queued.
    let rx = sh.bus.subscribe();
    let deadline = Instant::now() + timeout;
    let pk = conn.pubkey;
    let reader = sh.reader.clone();
    // Channels the waiter is a member of, as of the last check.
    let members = Arc::new(Mutex::new(HashSet::<String>::new()));
    let check_members = Arc::clone(&members);
    let check = move || {
        let reader = reader.clone();
        let members = Arc::clone(&check_members);
        async move {
            let (chans, rows) = reader
                .exec(move |s| {
                    let chans = s.list_channels(&pk)?;
                    let rows = s.inbox_after(&pk, after, limit)?;
                    Ok((chans, rows))
                })
                .await
                .map_err(dispatch::daemon_error)?;
            {
                let mut set = members.lock().unwrap_or_else(PoisonError::into_inner);
                *set = chans.into_iter().map(|c| c.id).collect();
            }
            Ok(if rows.is_empty() { None } else { Some(rows) })
        }
    };
    let relevant_members = Arc::clone(&members);
    let relevant = move |e: &Event| match e {
        Event::MessageAppended { channel, .. } => relevant_members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(channel),
        Event::ChannelChanged { .. } | Event::IdentityChanged { .. } => true,
        _ => false,
    };
    match wait_for(rx, deadline, &sh.shutdown, relevant, check).await? {
        Some(rows) => {
            let cursor = rows.iter().map(|(_, r)| r.seq).fold(after, i64::max);
            let messages = rows.iter().map(|(_, r)| MessageView::from_row(r)).collect();
            Ok(Reply::Inbox {
                messages,
                cursor,
                timed_out: false,
            })
        }
        None => Ok(Reply::Inbox {
            messages: Vec::new(),
            cursor: after,
            timed_out: true,
        }),
    }
}

/// Whether an event can make a reader's inbox non-empty: MessageAppended,
/// ChannelChanged and IdentityChanged -> true; everything else false.
pub fn wakes_inbox(e: &Event) -> bool {
    matches!(
        e,
        Event::MessageAppended { .. }
            | Event::ChannelChanged { .. }
            | Event::IdentityChanged { .. }
    )
}

/// Core loop, generic for unit tests. `rx` MUST have been subscribed before this is
/// called. Loop: `check().await?` -> Some(v) => return Ok(Some(v)); None => wait
/// for the next event, the deadline, or cancellation:
///   event for which `relevant` returns true, or Err(Lagged) -> check again;
///   irrelevant event (`relevant` false) -> keep waiting (no re-check);
///   deadline -> one final check, Some(v) => Ok(Some(v)), None => Ok(None);
///   cancellation or Err(Closed) -> Err(ShuttingDown).
/// `inbox_wait` passes a predicate that accepts MessageAppended only for the
/// waiter's own channels; tests may pass `wakes_inbox`.
pub async fn wait_for<T, R, C, Fut>(
    rx: broadcast::Receiver<Event>,
    deadline: tokio::time::Instant,
    shutdown: &CancellationToken,
    relevant: R,
    check: C,
) -> Result<Option<T>, ErrorBody>
where
    R: FnMut(&Event) -> bool,
    C: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>, ErrorBody>>,
{
    let mut rx = rx;
    let mut relevant = relevant;
    let mut check = check;
    loop {
        if let Some(v) = check().await? {
            return Ok(Some(v));
        }
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Err(shutting_down()),
                _ = tokio::time::sleep_until(deadline) => {
                    // A message that became visible at the deadline must not be lost.
                    return check().await;
                }
                r = rx.recv() => match r {
                    Ok(e) => {
                        if relevant(&e) {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => return Err(shutting_down()),
                },
            }
        }
    }
}

fn shutting_down() -> ErrorBody {
    ErrorBody::new(ErrorCode::ShuttingDown, "daemon is shutting down")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn appended(seq: i64) -> Event {
        Event::MessageAppended {
            channel: "general".into(),
            seq,
        }
    }

    #[test]
    fn wakes_inbox_only_for_inbox_events() {
        assert!(wakes_inbox(&appended(1)));
        assert!(wakes_inbox(&Event::ChannelChanged {
            channel: "c".into()
        }));
        assert!(wakes_inbox(&Event::IdentityChanged { pubkey: "k".into() }));
        assert!(!wakes_inbox(&Event::CardChanged { card: "c".into() }));
        assert!(!wakes_inbox(&Event::SprintChanged { sprint: "s".into() }));
        assert!(!wakes_inbox(&Event::NotePublished { id: "n".into() }));
        assert!(!wakes_inbox(&Event::ReactionChanged {
            kind: "message".into(),
            target: "t".into(),
        }));
        assert!(!wakes_inbox(&Event::Acked {
            reader: "r".into(),
            channel: "c".into(),
            seq: 1,
        }));
        assert!(!wakes_inbox(&Event::BlobStored { hash: "h".into() }));
    }

    #[tokio::test]
    async fn commit_between_check_and_wait_is_not_lost() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let shutdown = CancellationToken::new();
        let check = || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // A commit that lands after the first check and before the wait.
                let _ = tx.send(appended(1));
            }
            async move { Ok(if n == 0 { None } else { Some(1i64) }) }
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(Some(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(Instant::now() < deadline, "woke by event, not by deadline");
    }

    #[tokio::test]
    async fn irrelevant_events_do_not_trigger_a_recheck() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        for e in [
            Event::CardChanged { card: "c".into() },
            Event::SprintChanged { sprint: "s".into() },
            Event::NotePublished { id: "n".into() },
            Event::ReactionChanged {
                kind: "message".into(),
                target: "t".into(),
            },
            Event::Acked {
                reader: "r".into(),
                channel: "c".into(),
                seq: 1,
            },
            Event::BlobStored { hash: "h".into() },
        ] {
            let _ = tx.send(e);
        }
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_millis(50);
        let shutdown = CancellationToken::new();
        let check = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(None::<i64>) }
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(None));
        // The initial check and the final check at the deadline; the irrelevant
        // events in between cause no check.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn event_rejected_by_predicate_does_not_recheck() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        // A MessageAppended is an inbox event, but the predicate rejects it.
        let _ = tx.send(appended(1));
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_millis(50);
        let shutdown = CancellationToken::new();
        let check = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(None::<i64>) }
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, |_: &Event| false, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(None));
        // The initial check and the final check at the deadline; the rejected event
        // causes no check in between.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn relevant_event_rechecks_and_returns_the_found_value() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        // Another channel's message is not relevant; the one in "general" is.
        let _ = tx.send(Event::MessageAppended {
            channel: "other".into(),
            seq: 1,
        });
        let _ = tx.send(appended(2));
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let shutdown = CancellationToken::new();
        let relevant =
            |e: &Event| matches!(e, Event::MessageAppended { channel, .. } if channel == "general");
        let check = || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(if n == 0 { None } else { Some(5i64) }) }
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, relevant, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(Some(5)));
        // The initial check, then one re-check woken by the relevant event only.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(Instant::now() < deadline, "woke by event, not by deadline");
    }

    #[tokio::test]
    async fn deadline_without_events_returns_none_after_deadline() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        let deadline = Instant::now() + Duration::from_millis(30);
        let shutdown = CancellationToken::new();
        let check = || async { Ok(None::<i64>) };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(None));
        assert!(Instant::now() >= deadline);
    }

    #[tokio::test]
    async fn message_appearing_at_deadline_is_returned() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        let deadline = Instant::now() + Duration::from_millis(30);
        let shutdown = CancellationToken::new();
        // No event is ever published: the row is visible only from the deadline on,
        // so only the final check after the deadline can find it.
        let check = move || async move {
            Ok(if Instant::now() >= deadline {
                Some(9i64)
            } else {
                None
            })
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(Some(9)));
        assert!(Instant::now() >= deadline);
    }

    #[tokio::test]
    async fn cancellation_returns_shutting_down() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        let deadline = Instant::now() + Duration::from_secs(5);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let check = || async { Ok(None::<i64>) };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::ShuttingDown));
    }

    #[tokio::test]
    async fn lagged_receiver_rechecks() {
        let (tx, _keep) = broadcast::channel::<Event>(1);
        let rx = tx.subscribe();
        // Capacity 1: the receiver lags behind the three sends below.
        for _ in 0..3 {
            let _ = tx.send(Event::CardChanged { card: "c".into() });
        }
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let shutdown = CancellationToken::new();
        let check = || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(if n == 0 { None } else { Some(7i64) }) }
        };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got, Ok(Some(7)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dropped_sender_returns_shutting_down() {
        let (tx, _keep) = broadcast::channel::<Event>(16);
        let rx = tx.subscribe();
        drop(tx);
        drop(_keep);
        let deadline = Instant::now() + Duration::from_secs(5);
        let shutdown = CancellationToken::new();
        let check = || async { Ok(None::<i64>) };
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(rx, deadline, &shutdown, wakes_inbox, check),
        )
        .await
        .expect("wait_for must not hang");
        assert_eq!(got.map_err(|e| e.code), Err(ErrorCode::ShuttingDown));
    }
}
