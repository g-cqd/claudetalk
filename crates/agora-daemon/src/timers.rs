//! The timer loop: fires due timers as `timer.fire` system ops.
//!
//! The loop keeps no timer state of its own. Each pass asks the store which timers are due at the clock's `now`,
//! fires them in `next_fire` order, and then sleeps until the store's next due instant. It wakes again only for
//! shutdown, a kick, a `TimerChanged` on the bus, or a lagged bus. A timer whose fire fails is remembered at the
//! instant that failed and is left alone until a `TimerChanged` for it arrives, so a persistent error cannot spin
//! the loop.

use crate::{DaemonError, Shared, system};
use agora_proto::Event;
use agora_sched::model::TimerRecord;
use agora_sched::model_ops::TimerFireArgs;
use agora_store::StoreError;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Notify, broadcast};

/// How long a failed store read waits before the loop retries it, so a transient error does not leave due timers
/// unfired until some unrelated change happens.
const RETRY_MS: i64 = 1_000;

/// Loop: `next = store.next_timer_due()`; `sleep_until(next)` | `kick` | `Event::TimerChanged` | shutdown;
/// fire every due timer via the `timer.fire` system op; publish `WorkChanged` for created tasks and
/// `TimerChanged`; kick admission (`sh.kick`).
pub async fn run(sh: Shared, kick: Arc<Notify>) {
    if !sh.config.work_enabled {
        sh.shutdown.cancelled().await;
        return;
    }
    // Subscribed before the first pass, so a change committed after this point is never missed.
    let mut bus = sh.bus.subscribe();
    let mut failed = Failed::default();
    loop {
        let deadline = match pass(&sh, &mut failed).await {
            Ok(deadline) => deadline,
            Err(DaemonError::ActorGone) => return,
            Err(e) => {
                tracing::warn!(error = %e, "timer scan failed; retrying shortly");
                Some(sh.clock.now_ms().saturating_add(RETRY_MS))
            }
        };
        if matches!(
            wait(&sh, &kick, &mut bus, deadline, &mut failed).await,
            Wake::Stop
        ) {
            return;
        }
    }
}

/// One pass: fire every timer due at the clock's `now`, then return the instant to sleep until.
async fn pass(sh: &Shared, failed: &mut Failed) -> Result<Option<i64>, DaemonError> {
    let now = sh.clock.now_ms();
    let due = sh.reader.exec(move |store| store.due_timers(now)).await?;
    failed.retain_due(&due);
    let mut created_task = false;
    for timer in &due {
        let Some(next_fire) = timer.next_fire_ms else {
            continue;
        };
        if failed.contains(&timer.id, next_fire) {
            continue;
        }
        match fire(sh, timer, now).await {
            Ok(created) => created_task |= created,
            Err(DaemonError::ActorGone) => return Err(DaemonError::ActorGone),
            Err(e) => {
                tracing::warn!(
                    timer = %timer.id,
                    error = %e,
                    "timer fire failed; it is not retried until it changes"
                );
                failed.insert(&timer.id, next_fire);
            }
        }
    }
    if created_task {
        sh.kick.notify_one();
    }
    next_deadline(sh, failed).await
}

/// Fire one due timer as a `timer.fire` system op. Returns whether a task was created. The writer publishes the
/// events (`WorkChanged` for the new task, then `TimerChanged`) after the commit and before the reply.
async fn fire(sh: &Shared, timer: &TimerRecord, now: i64) -> Result<bool, DaemonError> {
    let arg = fire_arg(timer, now)
        .map_err(|_| DaemonError::Store(StoreError::Invalid("timer.fire arg did not serialize")))?;
    let op = system::sign(&sh.system, now, "timer.fire", &timer.id, &arg);
    let id = timer.id.clone();
    let task = sh
        .writer
        .exec(move |store| {
            let task = store.timer_fire(now, &op)?;
            let mut events = Vec::with_capacity(2);
            if let Some(task) = &task {
                events.push(Event::WorkChanged {
                    task: task.id.clone(),
                });
            }
            events.push(Event::TimerChanged { timer: id });
            Ok((task, events))
        })
        .await?;
    Ok(task.is_some())
}

/// The instant to wake for: the store's own minimum, unless a failed timer holds it. Then the minimum is taken
/// over the other timers, so one failing timer cannot hide a later one.
async fn next_deadline(sh: &Shared, failed: &Failed) -> Result<Option<i64>, DaemonError> {
    if failed.is_empty() {
        return sh.reader.exec(|store| store.next_timer_due()).await;
    }
    let timers = sh.reader.exec(|store| store.timers(None)).await?;
    Ok(earliest_unfailed(&timers, failed))
}

/// Why the wait ended.
#[derive(Debug)]
enum Wake {
    /// Run another pass.
    Rescan,
    /// Shutdown, or the bus closed: return from the loop.
    Stop,
}

/// Sleep until `deadline` (forever when `None`), or wake early for shutdown, a kick, a `TimerChanged` or a lagged
/// bus. Other bus events do not wake the loop.
async fn wait(
    sh: &Shared,
    kick: &Notify,
    bus: &mut broadcast::Receiver<Event>,
    deadline: Option<i64>,
    failed: &mut Failed,
) -> Wake {
    loop {
        let sleep: Pin<Box<dyn Future<Output = ()> + Send>> = match deadline {
            Some(at) => sh.clock.sleep_until(at),
            None => Box::pin(std::future::pending::<()>()),
        };
        tokio::select! {
            () = sh.shutdown.cancelled() => return Wake::Stop,
            () = kick.notified() => return Wake::Rescan,
            () = sleep => return Wake::Rescan,
            received = bus.recv() => match received {
                Ok(Event::TimerChanged { timer }) => {
                    failed.forget(&timer);
                    return Wake::Rescan;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => return Wake::Rescan,
                Err(broadcast::error::RecvError::Closed) => return Wake::Stop,
            },
        }
    }
}

/// Timers whose `timer.fire` failed, each at the instant that failed. Such a timer is not retried until a
/// `TimerChanged` for it arrives.
#[derive(Debug, Default)]
struct Failed {
    at: HashMap<String, i64>,
}

impl Failed {
    fn contains(&self, id: &str, next_fire: i64) -> bool {
        self.at.get(id) == Some(&next_fire)
    }

    fn insert(&mut self, id: &str, next_fire: i64) {
        self.at.insert(id.to_string(), next_fire);
    }

    /// A change to the timer lets the next pass try it again.
    fn forget(&mut self, id: &str) {
        self.at.remove(id);
    }

    /// Drop the entries whose timer is no longer due at the failed instant (fired, deleted or disabled).
    fn retain_due(&mut self, due: &[TimerRecord]) {
        self.at.retain(|id, at| {
            due.iter()
                .any(|t| t.id == *id && t.next_fire_ms == Some(*at))
        });
    }

    fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

/// The `fire_ms` a `timer.fire` carries: the instant the timer was scheduled for, never the wall time of the fire.
/// The store computes the next fire from `now`, so a late fire still coalesces the missed instants.
fn fire_ms(timer: &TimerRecord, now: i64) -> i64 {
    timer.next_fire_ms.unwrap_or(now)
}

/// The JSON argument of a `timer.fire` op.
fn fire_arg(timer: &TimerRecord, now: i64) -> Result<String, serde_json::Error> {
    serde_json::to_string(&TimerFireArgs {
        fire_ms: fire_ms(timer, now),
    })
}

/// The earliest next fire among the enabled timers, leaving out the (timer, instant) pairs that failed.
fn earliest_unfailed(timers: &[TimerRecord], failed: &Failed) -> Option<i64> {
    timers
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| t.next_fire_ms.map(|at| (t, at)))
        .filter(|(t, at)| !failed.contains(&t.id, *at))
        .map(|(_, at)| at)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Daemon, DaemonConfig, Injected};
    use agora_sched::model_ops::AddArgs;
    use agora_sched::timer::TimerSpec;
    use std::time::Duration;

    fn timer(id: &str, next_fire: Option<i64>, enabled: bool) -> TimerRecord {
        TimerRecord {
            id: id.to_string(),
            owner: String::new(),
            owner_name: String::new(),
            spec: TimerSpec::At { at_ms: 0 },
            template: AddArgs::default(),
            enabled,
            next_fire_ms: next_fire,
            last_fire_ms: None,
            fired: 0,
            created_ms: 0,
        }
    }

    #[test]
    fn fire_ms_is_the_scheduled_instant_not_the_wall_time() {
        assert_eq!(fire_ms(&timer("t", Some(1_000), true), 1_003), 1_000);
    }

    #[test]
    fn fire_ms_falls_back_to_now_without_a_schedule() {
        assert_eq!(fire_ms(&timer("t", None, true), 1_003), 1_003);
    }

    #[test]
    fn fire_arg_is_the_exact_fire_time_object() {
        let arg = fire_arg(&timer("t", Some(1_000), true), 1_003).expect("serializes");
        assert_eq!(arg, r#"{"fire_ms":1000}"#);
    }

    #[test]
    fn a_failed_instant_is_skipped_until_forgotten() {
        let mut failed = Failed::default();
        assert!(failed.is_empty());
        failed.insert("t", 1_000);
        assert!(failed.contains("t", 1_000));
        assert!(
            !failed.contains("t", 2_000),
            "a later instant is a new fire"
        );
        assert!(!failed.contains("u", 1_000), "another timer is not failed");
        failed.forget("t");
        assert!(!failed.contains("t", 1_000));
        assert!(failed.is_empty());
    }

    #[test]
    fn retain_due_drops_failures_that_are_no_longer_due() {
        let mut failed = Failed::default();
        failed.insert("kept", 1_000);
        failed.insert("fired", 1_000);
        failed.insert("moved", 1_000);
        let due = [
            timer("kept", Some(1_000), true),
            timer("moved", Some(5_000), true),
        ];
        failed.retain_due(&due);
        assert!(failed.contains("kept", 1_000));
        assert!(!failed.contains("fired", 1_000));
        assert!(!failed.contains("moved", 1_000));
    }

    #[test]
    fn earliest_unfailed_skips_disabled_and_failed_timers() {
        let mut failed = Failed::default();
        failed.insert("failing", 500);
        let timers = [
            timer("failing", Some(500), true),
            timer("later", Some(800), true),
            timer("disabled", Some(100), false),
        ];
        assert_eq!(earliest_unfailed(&timers, &failed), Some(800));
    }

    #[test]
    fn earliest_unfailed_takes_the_minimum_and_none_when_idle() {
        let failed = Failed::default();
        let timers = [timer("a", Some(900), true), timer("b", Some(300), true)];
        assert_eq!(earliest_unfailed(&timers, &failed), Some(300));
        assert_eq!(earliest_unfailed(&[], &failed), None);
        assert_eq!(
            earliest_unfailed(&[timer("c", None, true)], &failed),
            None,
            "a timer with no next fire is not scheduled"
        );
    }

    #[test]
    fn a_failure_at_an_old_instant_does_not_hide_a_new_schedule() {
        let mut failed = Failed::default();
        failed.insert("t", 500);
        let timers = [timer("t", Some(900), true)];
        assert_eq!(earliest_unfailed(&timers, &failed), Some(900));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_work_waits_for_shutdown_only() {
        let home = tempfile::Builder::new()
            .prefix("ag")
            .tempdir_in("/tmp")
            .expect("temp home under /tmp");
        let mut cfg = DaemonConfig::load(home.path()).expect("default config");
        cfg.handle_signals = false;
        cfg.work_enabled = false;
        let daemon = Daemon::bind_with(
            cfg,
            Injected {
                clock: None,
                machine: None,
            },
        )
        .await
        .expect("bind daemon");
        let shared = daemon.shared().clone();
        let token = daemon.shutdown_token();
        let mut task = tokio::spawn(run(shared, Arc::new(Notify::new())));
        // Nothing wakes the loop while work is disabled: it is still running after a short bounded wait.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut task)
                .await
                .is_err(),
            "the loop must not return before shutdown"
        );
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("loop returns after shutdown")
            .expect("loop task did not panic");
    }
}
