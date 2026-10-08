//! Handlers for timers, pools, leases, quiet windows, policies, card extensions, watchdog report,
//! metrics, board extras and notices. Timer changes publish `Event::TimerChanged` (the timer loop listens);
//! quiet/lease changes publish their events and quiet open/close and lease release kick admission.

use crate::dispatch::{daemon_error, op_from_wire, unrouted};
use crate::{Conn, Shared};
use agora_proto::{ErrorBody, Event, Reply, Req};
use agora_store::{Pub, Store, StoreError, hex};

/// Routes `TimerCreate/Delete/List`, `PoolDefine/List`, `LeaseAcquire/Renew/Release/List`, `QuietOpen/Close`,
/// `PolicySet/List`, `CardUpdate/Dep/Link/Escalate/Ext`, `WatchdogReport`, `Metrics`, `BoardExtras` and
/// `Notices`; anything else is `dispatch::unrouted()`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    let kick = kicks_admission(&req);
    let reply = match req {
        Req::TimerCreate { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let timer = s.timer_create(now, &op)?;
                let events = timer_events(timer.id.clone());
                Ok((Reply::Timer { timer }, events))
            })
            .await?
        }
        Req::TimerDelete { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            commit(sh, move |s| {
                s.timer_delete(now, &op)?;
                Ok((Reply::Unit, timer_events(subject)))
            })
            .await?
        }
        Req::TimerList { owner } => {
            read(sh, conn.pubkey, move |s| {
                let timers = s.timers(owner.as_deref())?;
                Ok(Reply::Timers { timers })
            })
            .await?
        }
        Req::PoolDefine { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let pool = s.pool_define(now, &op)?;
                let events = lease_events(pool_subject(&pool.name));
                Ok((Reply::Pool { pool }, events))
            })
            .await?
        }
        Req::PoolList => {
            let now = sh.clock.now_ms();
            read(sh, conn.pubkey, move |s| {
                let pools = s.pools(now)?;
                Ok(Reply::Pools { pools })
            })
            .await?
        }
        Req::LeaseAcquire { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let lease = s.lease_acquire(now, &op)?;
                let events = lease_events(lease.id.clone());
                Ok((Reply::Lease { lease }, events))
            })
            .await?
        }
        Req::LeaseRenew { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let lease = s.lease_renew(now, &op)?;
                let events = lease_events(lease.id.clone());
                Ok((Reply::Lease { lease }, events))
            })
            .await?
        }
        Req::LeaseRelease { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            commit(sh, move |s| {
                s.lease_release(now, &op)?;
                Ok((Reply::Unit, lease_events(subject)))
            })
            .await?
        }
        Req::LeaseList {
            kind,
            holder,
            active_only,
        } => {
            let now = sh.clock.now_ms();
            read(sh, conn.pubkey, move |s| {
                let leases = s.leases(now, kind, holder.as_deref(), active_only)?;
                Ok(Reply::Leases { leases })
            })
            .await?
        }
        Req::QuietOpen { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let lease = s.quiet_open(now, &op)?;
                let events = lease_events(lease.id.clone());
                Ok((Reply::Lease { lease }, events))
            })
            .await?
        }
        Req::QuietClose { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            commit(sh, move |s| {
                s.quiet_close(now, &op)?;
                Ok((Reply::Unit, lease_events(subject)))
            })
            .await?
        }
        Req::PolicySet { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let policy = s.policy_set(now, &op)?;
                Ok((Reply::Policy { policy }, policy_events()))
            })
            .await?
        }
        Req::PolicyList => {
            read(sh, conn.pubkey, move |s| {
                let policies = s.policies()?;
                Ok(Reply::Policies { policies })
            })
            .await?
        }
        Req::CardUpdate { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = sh.clock.now_ms();
            commit(sh, move |s| {
                let ext = s.card_update(now, &op)?;
                let events = card_events(ext.id.clone());
                Ok((Reply::CardExts { cards: vec![ext] }, events))
            })
            .await?
        }
        Req::CardDep { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            commit(sh, move |s| {
                s.card_dep(now, &op)?;
                Ok((Reply::Unit, card_events(subject)))
            })
            .await?
        }
        Req::CardLink { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            commit(sh, move |s| {
                s.card_link(now, &op)?;
                Ok((Reply::Unit, card_events(subject)))
            })
            .await?
        }
        Req::CardEscalate { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (sh.clock.now_ms(), op.subject.clone());
            let review = sh.config.require_review;
            commit(sh, move |s| {
                let notified = s.card_escalate(now, &op, review)?;
                let events = escalation_events(subject, &notified);
                Ok((Reply::Escalated { notified }, events))
            })
            .await?
        }
        Req::CardExt { ids } => {
            read(sh, conn.pubkey, move |s| {
                let cards = s.card_ext(&ids)?;
                Ok(Reply::CardExts { cards })
            })
            .await?
        }
        Req::WatchdogReport => {
            let (now, me) = (sh.clock.now_ms(), hex(&conn.pubkey));
            let review = sh.config.require_review;
            read(sh, conn.pubkey, move |s| {
                let report = s.watchdog_report(now, &me, review)?;
                Ok(Reply::Watchdog { report })
            })
            .await?
        }
        Req::Metrics {
            team,
            lane,
            since_ms,
        } => {
            let now = sh.clock.now_ms();
            read(sh, conn.pubkey, move |s| {
                let report = s.metrics(now, team.as_deref(), lane.as_deref(), since_ms)?;
                Ok(Reply::Metrics { report })
            })
            .await?
        }
        Req::BoardExtras => {
            let now = sh.clock.now_ms();
            read(sh, conn.pubkey, move |s| {
                let extras = s.board_extras(now)?;
                Ok(Reply::BoardExtras { extras })
            })
            .await?
        }
        Req::Notices { after, limit } => {
            let me = hex(&conn.pubkey);
            read(sh, conn.pubkey, move |s| {
                let notices = s.notices(&me, after, limit)?;
                Ok(Reply::Notices { notices })
            })
            .await?
        }
        _ => return Err(unrouted()),
    };
    if kick {
        sh.kick.notify_one();
    }
    Ok(reply)
}

/// Runs one signed write on the writer thread. The writer publishes the events after the commit and
/// before the reply is delivered; a failed write publishes nothing.
async fn commit<F>(sh: &Shared, write: F) -> Result<Reply, ErrorBody>
where
    F: FnOnce(&mut Store) -> Result<(Reply, Vec<Event>), StoreError> + Send + 'static,
{
    sh.writer.exec(write).await.map_err(daemon_error)
}

/// Runs one read on the reader thread after requiring that the connection's identity is active.
async fn read<F>(sh: &Shared, pk: Pub, query: F) -> Result<Reply, ErrorBody>
where
    F: FnOnce(&Store) -> Result<Reply, StoreError> + Send + 'static,
{
    sh.reader
        .exec(move |s| {
            s.role_of(&pk)?;
            query(s)
        })
        .await
        .map_err(daemon_error)
}

/// Quiet windows and lease releases change what admission may start, so they kick it. Other lease,
/// pool, timer, policy and card requests do not.
fn kicks_admission(req: &Req) -> bool {
    matches!(
        req,
        Req::QuietOpen { .. } | Req::QuietClose { .. } | Req::LeaseRelease { .. }
    )
}

/// Pool definitions share the lease event channel; their subject is `pool:<name>`.
fn pool_subject(name: &str) -> String {
    format!("pool:{name}")
}

fn timer_events(timer: String) -> Vec<Event> {
    vec![Event::TimerChanged { timer }]
}

fn lease_events(lease: String) -> Vec<Event> {
    vec![Event::LeaseChanged { lease }]
}

fn card_events(card: String) -> Vec<Event> {
    vec![Event::CardChanged { card }]
}

fn policy_events() -> Vec<Event> {
    vec![Event::PolicyChanged]
}

/// The card change, then one `NoticePosted` per notified target. The notice rows were written in the
/// same transaction, but their sequence numbers are not returned by the store, so `seq` is 0; a
/// subscriber that needs the real number reads the notices with `Notices{after}`.
fn escalation_events(card: String, notified: &[String]) -> Vec<Event> {
    let mut events = card_events(card);
    events.extend(notified.iter().map(|target| Event::NoticePosted {
        target: target.clone(),
        seq: 0,
    }));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_proto::OpWire;

    fn wire() -> OpWire {
        OpWire {
            kind: "quiet.open".into(),
            subject: String::new(),
            arg: "{}".into(),
            ts: 0,
            nonce: String::new(),
            actor: String::new(),
            sig: String::new(),
        }
    }

    #[test]
    fn pool_definitions_publish_under_the_pool_subject() {
        assert_eq!(pool_subject("gpu"), "pool:gpu");
        assert_eq!(
            lease_events(pool_subject("gpu")),
            vec![Event::LeaseChanged {
                lease: "pool:gpu".into()
            }]
        );
    }

    #[test]
    fn each_entity_publishes_its_own_event_variant() {
        assert_eq!(
            timer_events("t1".into()),
            vec![Event::TimerChanged { timer: "t1".into() }]
        );
        assert_eq!(
            lease_events("l1".into()),
            vec![Event::LeaseChanged { lease: "l1".into() }]
        );
        assert_eq!(
            card_events("c1".into()),
            vec![Event::CardChanged { card: "c1".into() }]
        );
        assert_eq!(policy_events(), vec![Event::PolicyChanged]);
    }

    #[test]
    fn escalation_publishes_the_card_then_one_notice_per_target_in_order() {
        let targets = vec!["aa".to_owned(), "bb".to_owned()];
        assert_eq!(
            escalation_events("c1".into(), &targets),
            vec![
                Event::CardChanged { card: "c1".into() },
                Event::NoticePosted {
                    target: "aa".into(),
                    seq: 0
                },
                Event::NoticePosted {
                    target: "bb".into(),
                    seq: 0
                },
            ]
        );
    }

    #[test]
    fn escalation_with_no_targets_publishes_only_the_card() {
        assert_eq!(
            escalation_events("c1".into(), &[]),
            vec![Event::CardChanged { card: "c1".into() }]
        );
    }

    #[test]
    fn only_quiet_windows_and_lease_releases_kick_admission() {
        assert!(kicks_admission(&Req::QuietOpen { op: wire() }));
        assert!(kicks_admission(&Req::QuietClose { op: wire() }));
        assert!(kicks_admission(&Req::LeaseRelease { op: wire() }));
        assert!(!kicks_admission(&Req::LeaseAcquire { op: wire() }));
        assert!(!kicks_admission(&Req::LeaseRenew { op: wire() }));
        assert!(!kicks_admission(&Req::PoolDefine { op: wire() }));
        assert!(!kicks_admission(&Req::PolicySet { op: wire() }));
        assert!(!kicks_admission(&Req::TimerCreate { op: wire() }));
        assert!(!kicks_admission(&Req::PoolList));
    }
}
