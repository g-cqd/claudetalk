//! The daemon's background tasks: job adoption + admission, the timer loop and the reconciler.
//!
//! `Daemon::run` starts them with [`spawn_all`] before accepting connections and stops them with
//! [`Background::stop`] before it drops its `Shared`: every task holds a `Shared`, and the store actor threads
//! end only when the last one is gone.

use crate::{Shared, admission, jobs, reconciler, timers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// How long the tasks get to notice the shutdown token before they are aborted.
const STOP_GRACE: Duration = Duration::from_secs(5);

pub struct Background {
    pub handles: Vec<JoinHandle<()>>,
    /// Wakes the timer loop (`timers::run`).
    pub timers_kick: Arc<Notify>,
    /// Wakes the reconciler (`reconciler::run`).
    pub reconcile_kick: Arc<Notify>,
}

/// Start the background tasks. Empty (no tasks) when `config.work_enabled` is false.
pub fn spawn_all(sh: &Shared) -> Background {
    let timers_kick = Arc::new(Notify::new());
    let reconcile_kick = Arc::new(Notify::new());
    let mut handles = Vec::new();
    if sh.config.work_enabled {
        // Adoption first, so admission sees the true running set.
        let s = sh.clone();
        handles.push(tokio::spawn(async move {
            jobs::adopt_all(&s).await;
            admission::run(s).await;
        }));
        handles.push(tokio::spawn(timers::run(sh.clone(), timers_kick.clone())));
        handles.push(tokio::spawn(reconciler::run(
            sh.clone(),
            reconcile_kick.clone(),
        )));
    }
    Background {
        handles,
        timers_kick,
        reconcile_kick,
    }
}

impl Background {
    /// Wait for every task to end (the shutdown token must already be cancelled), aborting stragglers after a
    /// grace period; then abort and join the jobs layer's own tasks.
    pub async fn stop(self, sh: &Shared) {
        let Background { handles, .. } = self;
        for mut handle in handles {
            if tokio::time::timeout(STOP_GRACE, &mut handle).await.is_err() {
                tracing::warn!("a background task ignored the shutdown; aborting it");
                handle.abort();
                let _ = handle.await;
            }
        }
        sh.jobs.shutdown().await;
    }
}
