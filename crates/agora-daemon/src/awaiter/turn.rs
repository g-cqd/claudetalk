//! The parts every condition shares: the clock window, the wake-ups between checks, and the result of a wait.

use super::cursor::{Cursor, encode_cursor, seconds};
use crate::{DaemonError, Shared, watch::Watcher};
use agora_proto::Event;
use agora_sched::exit::EXIT_TIMEOUT;
use agora_sched::model::{TaskRecord, WaitResult};
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};

/// Longest one filesystem wake-up waits before the loop re-checks. A healthy watcher wakes on its own events, so this
/// only bounds the cost of a missed event; the watcher's polling mode clamps itself to 1 s, and the deadline still
/// bounds the whole wait.
pub(super) const FS_RECHECK: Duration = Duration::from_secs(60);
/// Re-check for waits whose condition can become true without a further event (an append after a create).
/// FSEvents does not report every append to a file that was just created (a job's first line can land after the
/// create event and produce no event of its own, seen under host load), so such a wait cannot rely on events for
/// new bytes. A scan reads only new bytes, so a 1 s cadence is cheap.
pub(super) const GROWTH_RECHECK: Duration = Duration::from_secs(1);

/// What wakes a waiting loop between two checks.
pub(super) enum Wake {
    /// Bus events the predicate accepts. The subscription is taken before the first check.
    Bus {
        rx: broadcast::Receiver<Event>,
        relevant: Box<dyn Fn(&Event) -> bool + Send>,
    },
    /// Changes seen by a watcher that was created before the first check and is kept for the whole wait, and the
    /// longest the loop waits for one before it re-checks anyway.
    Fs { watcher: Watcher, recheck: Duration },
    /// Only the caller's pause (pid waits probe liveness on a timer).
    Timer,
}

/// Per-request state: the clock window and the condition name the result carries.
pub(super) struct Ctx<'a> {
    pub(super) sh: &'a Shared,
    pub(super) condition: &'static str,
    pub(super) start_ms: i64,
    pub(super) deadline_ms: i64,
    /// Milliseconds already accumulated by earlier calls of the same wait (from the cursor).
    pub(super) elapsed_before_ms: i64,
}

/// What a wait found when it ended. On a timeout the status is always 124, whatever `status` says.
#[derive(Default)]
pub(super) struct Found {
    pub(super) status: i32,
    pub(super) targets: Vec<String>,
    pub(super) pending: Vec<String>,
    pub(super) tasks: Vec<TaskRecord>,
    pub(super) tail: Vec<String>,
    pub(super) cursor: Cursor,
}

impl Ctx<'_> {
    /// Sleeps until the next wake-up: a relevant event, a filesystem change, the pause, or the deadline. Returns
    /// `Ok(false)` once the deadline has passed (the caller makes its last check first, so none is lost);
    /// `Ok(true)` after any wake-up, which the caller answers with a re-check.
    pub(super) async fn wait_turn(
        &self,
        wake: &mut Wake,
        pause_ms: Option<i64>,
    ) -> Result<bool, DaemonError> {
        let now = self.sh.clock.now_ms();
        if now >= self.deadline_ms {
            return Ok(false);
        }
        let wake_at = pause_ms.map_or(self.deadline_ms, |p| {
            now.saturating_add(p).min(self.deadline_ms)
        });
        let mut timer = self.sh.clock.sleep_until(wake_at);
        match wake {
            Wake::Bus { rx, relevant } => {
                loop {
                    tokio::select! {
                        biased;
                        () = self.sh.shutdown.cancelled() => return Err(shutting_down()),
                        () = &mut timer => break,
                        r = rx.recv() => match r {
                            Ok(event) => {
                                if relevant(&event) {
                                    break;
                                }
                            }
                            // A lagged receiver may have missed the relevant event: re-check.
                            Err(RecvError::Lagged(_)) => break,
                            Err(RecvError::Closed) => return Err(shutting_down()),
                        },
                    }
                }
                drain(rx);
            }
            Wake::Fs { watcher, recheck } => {
                tokio::select! {
                    biased;
                    () = self.sh.shutdown.cancelled() => return Err(shutting_down()),
                    () = &mut timer => {}
                    () = watcher.changed(*recheck) => {}
                }
            }
            Wake::Timer => {
                tokio::select! {
                    biased;
                    () = self.sh.shutdown.cancelled() => return Err(shutting_down()),
                    () = &mut timer => {}
                }
            }
        }
        Ok(true)
    }

    /// The result of a wait that ended now; the elapsed time accumulates into the cursor.
    pub(super) fn end(&self, timed_out: bool, found: Found) -> WaitResult {
        let elapsed = self.sh.clock.now_ms().saturating_sub(self.start_ms).max(0);
        let total = self.elapsed_before_ms.saturating_add(elapsed);
        let cursor = Cursor {
            elapsed_ms: total,
            ..found.cursor
        };
        WaitResult {
            status: if timed_out {
                EXIT_TIMEOUT
            } else {
                found.status
            },
            timed_out,
            condition: self.condition.to_string(),
            targets: found.targets,
            pending: found.pending,
            seconds: seconds(total),
            tail: found.tail,
            tasks: found.tasks,
            cursor: timed_out.then(|| encode_cursor(&cursor)),
        }
    }
}

/// Drops the events already queued on a woken bus subscription. The caller re-checks the state after this wake-up,
/// and that check reflects every one of them, so a burst costs one check rather than one per event.
fn drain(rx: &mut broadcast::Receiver<Event>) {
    loop {
        match rx.try_recv() {
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
        }
    }
}

/// Daemon shutdown ends the wait with the ShuttingDown code, which `dispatch::daemon_error` gives `ActorGone`.
pub(super) fn shutting_down() -> DaemonError {
    DaemonError::ActorGone
}
