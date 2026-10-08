//! Injectable time. All M5 daemon code reads time through [`Clock::now_ms`] and sleeps through
//! [`Clock::sleep_until`], so tests drive it with [`FakeClock`] and never depend on wall time.
//! M1-M4 code keeps using [`crate::now_ms`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Longest single `tokio::time::sleep` the system clock issues; it re-checks the wall clock after each.
const MAX_SLEEP_MS: u64 = 86_400_000;

pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
    /// Resolves once `now_ms() >= deadline_ms` (immediately if already past).
    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::now_ms()
    }

    /// Sleeps in chunks of at most a day and re-checks the wall clock, so a clock step is honoured.
    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            loop {
                let remaining = deadline_ms.saturating_sub(crate::now_ms());
                if remaining <= 0 {
                    return;
                }
                let ms = u64::try_from(remaining).unwrap_or(MAX_SLEEP_MS);
                tokio::time::sleep(Duration::from_millis(ms.min(MAX_SLEEP_MS))).await;
            }
        })
    }
}

/// A clock that only moves when a test says so. Sleepers wake on `advance`/`set`, with no polling.
#[derive(Debug)]
pub struct FakeClock {
    tx: watch::Sender<i64>,
}

impl FakeClock {
    pub fn new(start_ms: i64) -> Arc<FakeClock> {
        Arc::new(FakeClock {
            tx: watch::channel(start_ms).0,
        })
    }

    /// Move the clock forward by `ms` (backwards if negative) and wake the sleepers whose deadline passed.
    pub fn advance(&self, ms: i64) {
        self.tx.send_modify(|now| *now = now.saturating_add(ms));
    }

    /// Set the clock to `ms`.
    pub fn set(&self, ms: i64) {
        self.tx.send_replace(ms);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        *self.tx.borrow()
    }

    fn sleep_until(&self, deadline_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let mut rx = self.tx.subscribe();
        Box::pin(async move {
            let reached = rx.wait_for(|now| *now >= deadline_ms).await.is_ok();
            if !reached {
                // The clock was dropped: time can never advance again.
                std::future::pending::<()>().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_clock_wakes_sleepers_only_when_the_deadline_passes() {
        let clock = FakeClock::new(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        // A deadline already in the past resolves at once.
        tokio::time::timeout(Duration::from_secs(5), clock.sleep_until(900))
            .await
            .expect("past deadline");
        let sleeper = tokio::spawn(clock.sleep_until(1_500));
        clock.advance(400);
        tokio::task::yield_now().await;
        assert!(!sleeper.is_finished());
        clock.advance(100);
        tokio::time::timeout(Duration::from_secs(5), sleeper)
            .await
            .expect("woken")
            .expect("task");
        assert_eq!(clock.now_ms(), 1_500);
        clock.set(7);
        assert_eq!(clock.now_ms(), 7);
    }

    #[tokio::test]
    async fn system_clock_sleeps_until_a_near_deadline() {
        let clock = SystemClock;
        let deadline = clock.now_ms() + 30;
        tokio::time::timeout(Duration::from_secs(5), clock.sleep_until(deadline))
            .await
            .expect("slept");
        assert!(clock.now_ms() >= deadline);
    }
}
