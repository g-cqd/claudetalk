//! Event push for subscribed connections (see SPEC Part 1): which bus events a
//! subscriber sees, the slow-consumer policy, and presence.

use crate::presence::PresenceGuard;
use crate::{Conn, DaemonError, Shared, dispatch};
use agora_proto::{ErrorBody, ErrorCode, Event, Reply};
use agora_store::{Pub, hex};
use std::collections::{BTreeSet, HashSet};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;

/// Capacity of the per-connection event queue between the forwarder and the
/// connection writer. The forwarder never blocks on it (`try_send`).
pub const EVENT_QUEUE: usize = 256;

/// What one subscriber wants to see. `explicit` is the channel list the client
/// asked for (`None` = every channel it is a member of); `members` is the
/// identity's current membership as of the last refresh.
#[derive(Debug, Clone)]
pub struct Filter {
    pub me: Pub,
    pub explicit: Option<BTreeSet<String>>,
    pub members: HashSet<String>,
}

/// `explicit ∩ members` when a list was given, else all of `members`.
fn effective_for(
    explicit: Option<&BTreeSet<String>>,
    members: &HashSet<String>,
) -> BTreeSet<String> {
    match explicit {
        Some(list) => list
            .iter()
            .filter(|channel| members.contains(channel.as_str()))
            .cloned()
            .collect(),
        None => members.iter().cloned().collect(),
    }
}

impl Filter {
    pub fn new(me: Pub, explicit: Option<BTreeSet<String>>, members: HashSet<String>) -> Self {
        Self {
            me,
            explicit,
            members,
        }
    }

    /// The effective channel set: `explicit ∩ members` when a list was given,
    /// else all of `members`.
    pub fn effective(&self) -> BTreeSet<String> {
        effective_for(self.explicit.as_ref(), &self.members)
    }

    /// Whether `channel` is in the effective set, without building the set.
    fn in_effective(&self, channel: &str) -> bool {
        self.members.contains(channel)
            && self
                .explicit
                .as_ref()
                .is_none_or(|list| list.contains(channel))
    }

    /// True for `ChannelChanged{any}` and `IdentityChanged{pubkey == hex(me)}`: the
    /// events after which the membership must be re-read before `wants` is asked.
    pub fn needs_refresh(&self, e: &Event) -> bool {
        match e {
            Event::ChannelChanged { .. } => true,
            Event::IdentityChanged { pubkey } => *pubkey == hex(&self.me),
            _ => false,
        }
    }

    /// Forwarding rules (`self` = the state BEFORE the refresh the event triggers;
    /// `after_refresh` = the effective set after it, `None` when no refresh ran):
    /// * `MessageAppended{channel}` iff channel is in the effective set;
    /// * `ChannelChanged{channel}` iff channel was in the effective set before OR
    ///   after the refresh (a removed member learns it was removed, an added member
    ///   learns it was added); an explicit list only ever forwards its listed channels;
    /// * `Acked{reader}` iff reader == hex(me); `IdentityChanged{pubkey}` iff
    ///   pubkey == hex(me);
    /// * everything else (including `Lagged`, which is never on the bus): false.
    pub fn wants(&self, e: &Event, after_refresh: Option<&BTreeSet<String>>) -> bool {
        match e {
            Event::MessageAppended { channel, .. } => self.in_effective(channel),
            Event::ChannelChanged { channel } => {
                let listed = self
                    .explicit
                    .as_ref()
                    .is_none_or(|list| list.contains(channel));
                let before = self.in_effective(channel);
                let after = after_refresh.is_some_and(|set| set.contains(channel));
                listed && (before || after)
            }
            Event::Acked { reader, .. } => *reader == hex(&self.me),
            Event::IdentityChanged { pubkey } => *pubkey == hex(&self.me),
            _ => false,
        }
    }

    /// Replace the membership set after a refresh.
    pub fn set_members(&mut self, members: HashSet<String>) {
        self.members = members;
    }
}

/// Per-connection task state created by [`subscribe`]; run it once with [`Forwarder::run`].
pub struct Forwarder {
    sh: Shared,
    me: Pub,
    rx: broadcast::Receiver<Event>,
    filter: Filter,
    /// Keeps the identity online from the moment `Subscribed` is queued until the
    /// forwarder is dropped. Never read; its drop is the effect.
    _presence: PresenceGuard,
}

/// The channels `pk` is a current member of, read on the reader actor. Also the
/// active-identity check: an inactive identity gets the store's role error.
async fn read_members(sh: &Shared, pk: Pub) -> Result<HashSet<String>, DaemonError> {
    let channels = sh.reader.exec(move |s| s.list_channels(&pk)).await?;
    Ok(channels.into_iter().map(|c| c.id).collect())
}

/// What the forwarder does with one bus event, after any membership refresh it triggers.
struct Verdict {
    /// Offer the event to the connection.
    forward: bool,
    /// End the subscription after this event (the identity was revoked and can no
    /// longer be read, so there is nothing left to forward).
    end: bool,
}

impl Forwarder {
    /// Re-reads the CURRENT membership of this identity (`list_channels` on the reader
    /// actor) and installs it. It is the state now, not the state at the event: the
    /// forwarder only drives notifications, and the inbox cursor stays authoritative.
    ///
    /// With `event`, the verdict is `wants` asked against the state before the refresh
    /// and the refreshed effective set. Without one (a lost bus event), only the set
    /// changes. A failed read fails closed: the set is cleared, so no channel-scoped
    /// event is forwarded until a later refresh succeeds. An `IdentityChanged` for
    /// this identity then ends the subscription, so the presence guard drops with it.
    async fn refresh(&mut self, event: Option<&Event>) -> Verdict {
        match read_members(&self.sh, self.me).await {
            Ok(members) => {
                let after = effective_for(self.filter.explicit.as_ref(), &members);
                let forward = event.is_some_and(|e| self.filter.wants(e, Some(&after)));
                self.filter.set_members(members);
                Verdict {
                    forward,
                    end: false,
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "membership refresh failed; clearing the set");
                let forward = event.is_some_and(|e| self.filter.wants(e, None));
                self.filter.set_members(HashSet::new());
                let end = event.is_some_and(
                    |e| matches!(e, Event::IdentityChanged { pubkey } if *pubkey == hex(&self.me)),
                );
                Verdict { forward, end }
            }
        }
    }

    /// The verdict for one bus event; refreshes membership first when the event needs it.
    async fn decide(&mut self, event: &Event) -> Verdict {
        if self.filter.needs_refresh(event) {
            self.refresh(Some(event)).await
        } else {
            Verdict {
                forward: self.filter.wants(event, None),
                end: false,
            }
        }
    }

    /// Forward wanted bus events to `out` until `ending` is cancelled, the bus
    /// closes or `out` is closed. Holds `sh.presence.enter(me)` for as long as it
    /// runs, so the identity is online exactly while the subscription lives.
    ///
    /// For each bus event, `decide` re-reads membership when the event needs it and
    /// asks `wants`; forwarded events keep bus (commit) order.
    ///
    /// Slow consumer policy: never block (`try_send`). When `out` is full, count the
    /// event as dropped; as soon as `out` accepts again, send `Event::Lagged{dropped}`
    /// FIRST and reset the count. A bus `RecvError::Lagged(n)` counts as `n` dropped
    /// and re-reads membership, since a ChannelChanged may have been lost.
    ///
    /// The run ends when `ending` or the daemon shutdown fires, the bus or `out` closes,
    /// or this identity's revocation arrives and cannot be re-read (that notice is
    /// offered once, then the run returns). Returning drops the presence guard.
    pub async fn run(mut self, out: mpsc::Sender<Event>, ending: CancellationToken) {
        let mut dropped: u64 = 0;
        loop {
            tokio::select! {
                biased;
                _ = ending.cancelled() => break,
                _ = self.sh.shutdown.cancelled() => break,
                // Wakes on a free queue slot while events are owed, so the marker goes
                // out even when no later event arrives to carry it.
                permit = out.reserve(), if dropped > 0 => match permit {
                    Ok(permit) => {
                        permit.send(Event::Lagged { dropped });
                        dropped = 0;
                    }
                    Err(_) => return,
                },
                received = self.rx.recv() => match received {
                    Ok(event) => {
                        let verdict = self.decide(&event).await;
                        if verdict.forward && !offer(&out, &mut dropped, event) {
                            return;
                        }
                        if verdict.end {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        dropped = dropped.saturating_add(n);
                        self.refresh(None).await;
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    }
}

/// Queues `event` on `out` without blocking, after an owed `Lagged` marker. An event
/// (or marker) that does not fit is counted in `dropped`. False when `out` is closed.
fn offer(out: &mpsc::Sender<Event>, dropped: &mut u64, event: Event) -> bool {
    if *dropped > 0 {
        match out.try_send(Event::Lagged { dropped: *dropped }) {
            Ok(()) => *dropped = 0,
            Err(TrySendError::Full(_)) => {
                *dropped = dropped.saturating_add(1);
                return true;
            }
            Err(TrySendError::Closed(_)) => return false,
        }
    }
    match out.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            *dropped = dropped.saturating_add(1);
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

/// Start a subscription for `conn`. Order matters (no lost events): call
/// `sh.bus.subscribe()` FIRST, then check the identity is active (`role_of`) and
/// resolve membership via the reader, then build `Reply::Subscribed{effective, sorted}`.
/// Empty `channels` = all current memberships (re-resolved later); non-empty must
/// each be a current membership, else `NotMember(<first offending id as supplied>)`
/// (same error for a missing channel: no existence leak) and no subscription.
/// An inactive identity gets the `role_of` error.
pub async fn subscribe(
    sh: &Shared,
    conn: &Conn,
    channels: Vec<String>,
) -> Result<(Reply, Forwarder), ErrorBody> {
    let rx = sh.bus.subscribe();
    let me = conn.pubkey;
    let members = read_members(sh, me).await.map_err(dispatch::daemon_error)?;
    let explicit = if channels.is_empty() {
        None
    } else {
        if let Some(missing) = channels.iter().find(|c| !members.contains(c.as_str())) {
            return Err(ErrorBody::new(ErrorCode::NotMember, missing.clone()));
        }
        Some(channels.into_iter().collect::<BTreeSet<String>>())
    };
    let filter = Filter::new(me, explicit, members);
    let effective: Vec<String> = filter.effective().into_iter().collect();
    // Online from here on: the Subscribed reply below is queued by the caller.
    let presence = sh.presence.enter(me);
    let forwarder = Forwarder {
        sh: sh.clone(),
        me,
        rx,
        filter,
        _presence: presence,
    };
    Ok((
        Reply::Subscribed {
            channels: effective,
        },
        forwarder,
    ))
}

/// `Req::Online`: hex keys that are online (`sh.presence`) AND share at least one
/// channel with the caller, plus the caller itself if online, sorted. Never anyone
/// else (no leak); the caller must be active, else the `role_of` error.
pub async fn online(sh: &Shared, conn: &Conn) -> Result<Reply, ErrorBody> {
    let me = conn.pubkey;
    let peers = sh
        .reader
        .exec(move |s| {
            let mut peers: BTreeSet<Pub> = BTreeSet::new();
            for channel in s.list_channels(&me)? {
                for (pk, _pseudonym) in s.channel_members(&channel.id)? {
                    peers.insert(pk);
                }
            }
            Ok(peers)
        })
        .await
        .map_err(dispatch::daemon_error)?;
    // `presence.online()` is sorted by key bytes, which is the order of their
    // lowercase hex, so the result stays sorted.
    let pubkeys = sh
        .presence
        .online()
        .into_iter()
        .filter(|pk| *pk == me || peers.contains(pk))
        .map(|pk| hex(&pk))
        .collect();
    Ok(Reply::Online { pubkeys })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Daemon, DaemonConfig};
    use std::path::PathBuf;
    use std::time::Duration;

    /// Upper bound on the one wait in these tests, so a bug fails instead of hanging.
    const WAIT: Duration = Duration::from_secs(5);

    fn key(b: u8) -> Pub {
        [b; 32]
    }

    fn members(channels: &[&str]) -> HashSet<String> {
        channels.iter().map(|c| c.to_string()).collect()
    }

    fn listed(channels: &[&str]) -> BTreeSet<String> {
        channels.iter().map(|c| c.to_string()).collect()
    }

    fn message(channel: &str) -> Event {
        Event::MessageAppended {
            channel: channel.into(),
            seq: 1,
        }
    }

    fn channel_changed(channel: &str) -> Event {
        Event::ChannelChanged {
            channel: channel.into(),
        }
    }

    #[test]
    fn message_is_forwarded_only_for_member_channels() {
        let f = Filter::new(key(1), None, members(&["general"]));
        assert!(f.wants(&message("general"), None));
        assert!(!f.wants(&message("secret"), None));
    }

    #[test]
    fn explicit_list_forwards_only_listed_member_channels() {
        let f = Filter::new(key(1), Some(listed(&["a"])), members(&["a", "b"]));
        assert!(f.wants(&message("a"), None));
        assert!(!f.wants(&message("b"), None));
        assert!(!f.wants(&message("z"), None));
    }

    #[test]
    fn effective_is_explicit_intersect_members_or_all_members() {
        let explicit = Filter::new(key(1), Some(listed(&["a", "x"])), members(&["a", "b"]));
        assert_eq!(explicit.effective(), listed(&["a"]));
        let all = Filter::new(key(1), None, members(&["b", "a"]));
        assert_eq!(all.effective(), listed(&["a", "b"]));
    }

    #[test]
    fn channel_added_is_forwarded_through_the_refresh_result() {
        let f = Filter::new(key(1), None, members(&[]));
        assert!(!f.wants(&channel_changed("new"), None));
        assert!(f.wants(&channel_changed("new"), Some(&listed(&["new"]))));
    }

    #[test]
    fn channel_removed_is_forwarded_through_the_previous_membership() {
        let f = Filter::new(key(1), None, members(&["gone"]));
        assert!(f.wants(&channel_changed("gone"), Some(&listed(&[]))));
        assert!(f.wants(&channel_changed("gone"), None));
    }

    #[test]
    fn unrelated_channel_change_is_not_forwarded() {
        let f = Filter::new(key(1), None, members(&["a"]));
        assert!(!f.wants(&channel_changed("z"), Some(&listed(&["a"]))));
    }

    #[test]
    fn explicit_list_never_forwards_an_unlisted_channel_change() {
        let f = Filter::new(key(1), Some(listed(&["a"])), members(&["a", "b"]));
        assert!(!f.wants(&channel_changed("b"), Some(&listed(&["a", "b"]))));
        assert!(f.wants(&channel_changed("a"), Some(&listed(&["a"]))));
    }

    #[test]
    fn acked_is_forwarded_only_for_own_reader() {
        let f = Filter::new(key(1), None, members(&["a"]));
        let own = Event::Acked {
            reader: hex(&key(1)),
            channel: "a".into(),
            seq: 3,
        };
        let other = Event::Acked {
            reader: hex(&key(2)),
            channel: "a".into(),
            seq: 3,
        };
        assert!(f.wants(&own, None));
        assert!(!f.wants(&other, None));
    }

    #[test]
    fn identity_changed_is_forwarded_only_for_self() {
        let f = Filter::new(key(1), None, members(&[]));
        assert!(f.wants(
            &Event::IdentityChanged {
                pubkey: hex(&key(1))
            },
            None
        ));
        assert!(!f.wants(
            &Event::IdentityChanged {
                pubkey: hex(&key(2))
            },
            None
        ));
    }

    #[test]
    fn lagged_and_other_events_are_never_forwarded() {
        let f = Filter::new(key(1), None, members(&["a"]));
        assert!(!f.wants(&Event::Lagged { dropped: 1 }, None));
        assert!(!f.wants(&Event::BlobStored { hash: "h".into() }, None));
        assert!(!f.wants(&Event::NotePublished { id: "n".into() }, None));
    }

    #[test]
    fn needs_refresh_for_channel_changes_and_own_identity_only() {
        let f = Filter::new(key(1), None, members(&[]));
        assert!(f.needs_refresh(&channel_changed("any")));
        assert!(f.needs_refresh(&Event::IdentityChanged {
            pubkey: hex(&key(1))
        }));
        assert!(!f.needs_refresh(&Event::IdentityChanged {
            pubkey: hex(&key(2))
        }));
        assert!(!f.needs_refresh(&message("a")));
        assert!(!f.needs_refresh(&Event::Acked {
            reader: hex(&key(1)),
            channel: "a".into(),
            seq: 1,
        }));
    }

    #[test]
    fn set_members_changes_what_is_forwarded() {
        let mut f = Filter::new(key(1), None, members(&["a"]));
        assert!(!f.wants(&message("b"), None));
        f.set_members(members(&["b"]));
        assert!(f.wants(&message("b"), None));
        assert!(!f.wants(&message("a"), None));
    }

    /// A short unique home under /tmp, removed when dropped. `tag` keeps tests that run
    /// in parallel in one process apart.
    struct TestHome(PathBuf);

    impl TestHome {
        fn new(tag: &str) -> Self {
            let dir = PathBuf::from(format!("/tmp/agpush-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn config(&self) -> DaemonConfig {
            DaemonConfig {
                home: self.0.clone(),
                handle_signals: false,
                ..DaemonConfig::default()
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Next event from `rx`, within `WAIT`.
    async fn next_event(rx: &mut mpsc::Receiver<Event>) -> Event {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("an event within WAIT")
            .expect("the queue stays open")
    }

    /// Current-thread runtime on purpose: the forwarder only runs when the test yields,
    /// so the five posts all sit in the bus before it looks at them, and the outcome
    /// (two events, then one marker for three drops) does not depend on scheduling.
    #[tokio::test]
    async fn overrun_queue_gets_two_events_then_a_lagged_marker_for_the_rest() {
        let home = TestHome::new("lagged");
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let sh = daemon.shared().clone();
        let owner = agora_crypto::Identity::from_seed([7; 32]);
        let me = owner.public();
        let now = crate::now_ms();
        sh.writer
            .exec(move |s| {
                s.bootstrap_owner(now, &me)?;
                Ok(((), Vec::new()))
            })
            .await
            .expect("bootstrap the owner");
        let arg = serde_json::json!({
            "kind": "group",
            "members": [{"pk": hex(&me), "role": "member"}],
        })
        .to_string();
        let op =
            agora_store::SignedOp::new("channel.create", "g", &arg, now, "create-g").sign(&owner);
        sh.writer
            .exec(move |s| {
                s.create_channel(now, &op, &[])?;
                Ok(((), Vec::new()))
            })
            .await
            .expect("create the channel");

        let conn = Conn {
            pubkey: me,
            uid: 0,
            label: "test".into(),
        };
        let (_reply, forwarder) = subscribe(&sh, &conn, Vec::new())
            .await
            .expect("subscribe before the posts");

        let mut seqs = Vec::new();
        for i in 0..5 {
            let msg = agora_store::SignedMessage {
                id: format!("m{i}"),
                channel: "g".into(),
                thread_of: None,
                sender: [0; 32],
                ts: now,
                encrypted: false,
                epoch: 0,
                body: b"burst".to_vec(),
                attachments: vec![],
                sig: vec![],
            }
            .sign(&owner);
            let seq = sh
                .writer
                .exec(move |s| {
                    let seq = s.post_message(now, &msg)?;
                    Ok((
                        seq,
                        vec![Event::MessageAppended {
                            channel: "g".into(),
                            seq,
                        }],
                    ))
                })
                .await
                .expect("post");
            seqs.push(seq);
        }

        // Queue of 2, nobody reading yet: the first two events fit, the other three
        // are counted as dropped.
        let (out, mut queue) = mpsc::channel::<Event>(2);
        let ending = CancellationToken::new();
        let run = tokio::spawn(forwarder.run(out, ending.clone()));
        assert_eq!(
            next_event(&mut queue).await,
            Event::MessageAppended {
                channel: "g".into(),
                seq: seqs[0],
            }
        );
        assert_eq!(
            next_event(&mut queue).await,
            Event::MessageAppended {
                channel: "g".into(),
                seq: seqs[1],
            }
        );
        assert_eq!(next_event(&mut queue).await, Event::Lagged { dropped: 3 });

        ending.cancel();
        tokio::time::timeout(WAIT, run)
            .await
            .expect("the forwarder ends on cancel")
            .expect("the forwarder does not panic");
    }

    /// A revoked subscriber's forwarder ends after the notice, and ending it clears
    /// presence. The revoke runs in the store and publishes what the revoke handler
    /// publishes, so the refresh that follows really fails.
    #[tokio::test]
    async fn revoked_subscriber_leaves_presence_once_its_forwarder_ends() {
        let home = TestHome::new("presence");
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let sh = daemon.shared().clone();
        let owner = agora_crypto::Identity::from_seed([1; 32]);
        let bob_id = agora_crypto::Identity::from_seed([4; 32]);
        let owner_pk = owner.public();
        let bob = bob_id.public();
        let bob_hex = hex(&bob);
        let now = crate::now_ms();
        sh.writer
            .exec(move |s| {
                s.bootstrap_owner(now, &owner_pk)?;
                s.register_identity(now, &bob, "bob")?;
                Ok(((), Vec::new()))
            })
            .await
            .expect("bootstrap the owner and register bob");
        let arg = serde_json::json!({
            "kind": "group",
            "members": [{"pk": bob_hex, "role": "member"}],
        })
        .to_string();
        let op =
            agora_store::SignedOp::new("channel.create", "g", &arg, now, "create-g").sign(&owner);
        sh.writer
            .exec(move |s| {
                s.create_channel(now, &op, &[])?;
                Ok(((), Vec::new()))
            })
            .await
            .expect("create the channel with bob in it");

        let conn = Conn {
            pubkey: bob,
            uid: 0,
            label: "test".into(),
        };
        let (_reply, forwarder) = subscribe(&sh, &conn, Vec::new())
            .await
            .expect("bob subscribes");
        assert!(sh.presence.is_online(&bob), "subscribing marks bob online");

        let (out, mut queue) = mpsc::channel::<Event>(8);
        let run = tokio::spawn(forwarder.run(out, CancellationToken::new()));

        let revoke = agora_store::SignedOp::new("identity.revoke", &bob_hex, "", now, "revoke-bob")
            .sign(&owner);
        let notice = bob_hex.clone();
        sh.writer
            .exec(move |s| {
                s.revoke_identity(now, &revoke)?;
                Ok(((), vec![Event::IdentityChanged { pubkey: notice }]))
            })
            .await
            .expect("revoke bob");

        assert_eq!(
            next_event(&mut queue).await,
            Event::IdentityChanged { pubkey: bob_hex }
        );
        // The run returns on its own, within the 2 s bound: its presence guard drops with it.
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("the forwarder ends within 2 s of the revocation")
            .expect("the forwarder does not panic");
        assert!(
            queue.recv().await.is_none(),
            "nothing follows the notice: the queue is closed once the forwarder is gone"
        );
        assert!(
            !sh.presence.is_online(&bob),
            "bob is offline once his forwarder has ended"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribe_names_the_first_channel_the_identity_is_not_in() {
        let home = TestHome::new("subscribe");
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let sh = daemon.shared().clone();
        let me = key(7);
        sh.writer
            .exec(move |s| {
                s.bootstrap_owner(crate::now_ms(), &me)?;
                Ok(((), Vec::new()))
            })
            .await
            .expect("bootstrap the owner");
        let conn = Conn {
            pubkey: me,
            uid: 0,
            label: "test".into(),
        };
        let result = tokio::time::timeout(
            WAIT,
            subscribe(&sh, &conn, vec!["nope".into(), "also-nope".into()]),
        )
        .await
        .expect("subscribe answers in time");
        let Err(err) = result else {
            panic!("a channel the identity is not in must be refused");
        };
        assert_eq!(err.code, ErrorCode::NotMember);
        assert_eq!(err.message, "nope");
    }
}
