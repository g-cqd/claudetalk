//! The inbox pump: long-polls the agora inbox for the agent's identity and feeds `Delivery`s to
//! a harness driver (SPEC 5). Nothing here acks.
//!
//! Delivery discipline: a batch is sent to the driver first; the session cursor moves only after
//! the driver answers `true`. A refused or dropped batch is offered again, so a message is never
//! marked delivered before the agent accepted it. One exception: a batch refused `REFUSAL_CAP`
//! times in a row is skipped, logged at error level, so one batch cannot block the inbox forever.

use crate::{BridgeError, Delivery, prompt_text};
use agora_client::session_state::{SessionLock, load};
use agora_deliver::{Harness, Mechanism};
use agora_hooks::HookError;
use agora_hooks::daemon::{Inbox, ctx_for, fetch, heartbeat_for, seq_of};
use agora_hooks::render::render_messages;
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};
use tokio_util::sync::CancellationToken;

/// Default `PumpConfig::max_chars` (the render cap, below Claude's own 10,000).
pub const DEFAULT_MAX_CHARS: usize = 9_000;
/// Default `PumpConfig::wait_ms` (long-poll length per fetch).
pub const DEFAULT_WAIT_MS: u64 = 55_000;

/// Interval of the `OwnedSessionApi` heartbeat.
const BEAT_EVERY: Duration = Duration::from_secs(30);
/// The most messages one fetch returns (as the hooks fetch).
const FETCH_LIMIT: i64 = 50;
/// First delay after a failed fetch; doubled per failure up to `BACKOFF_MAX`.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
/// Longest delay between fetch retries.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// First pause before a refused batch is offered again; doubled per refusal up to
/// `REFUSAL_MAX`, and reset once the agent accepts a batch.
const REFUSAL_FIRST: Duration = Duration::from_secs(5);
/// Longest pause between offers of a refused batch.
const REFUSAL_MAX: Duration = Duration::from_secs(300);
/// Refusals in a row of one batch after which it is skipped: the cursor moves past it.
const REFUSAL_CAP: u32 = 5;
/// Shortest length of a round that fetched nothing new, measured from the fetch start: an inbox
/// that answers at once is still polled about once a second.
const MIN_ROUND: Duration = Duration::from_secs(1);
/// Retries of a failed cursor write after the first attempt, and the pause before each.
const COMMIT_RETRIES: u32 = 3;
const COMMIT_PAUSE: Duration = Duration::from_secs(1);

/// Settings of one pump: which inbox, which session cursor, how large each prompt may be.
#[derive(Debug, Clone)]
pub struct PumpConfig {
    /// The agora home (`$AGORA_HOME`).
    pub home: PathBuf,
    /// The identity folder (the agent's session root); identity via `agora_hooks::daemon::ctx_for`.
    pub folder: PathBuf,
    /// The `session_state` key: the harness's own session id (thread id, ACP session id, ...).
    pub session_key: String,
    /// Render cap in chars (`DEFAULT_MAX_CHARS`).
    pub max_chars: usize,
    /// Long-poll wait per fetch in ms (`DEFAULT_WAIT_MS`).
    pub wait_ms: u64,
    /// The harness of the bridged client: its `OwnedSessionApi` beat is filed under its slug.
    pub harness: Harness,
}

impl PumpConfig {
    /// The defaults above for the given home/folder/session key, beating as Claude Code (the
    /// harness `heartbeat` names). A bridge names its own harness with `with_harness`.
    pub fn new(home: PathBuf, folder: PathBuf, session_key: String) -> Self {
        Self {
            home,
            folder,
            session_key,
            max_chars: DEFAULT_MAX_CHARS,
            wait_ms: DEFAULT_WAIT_MS,
            harness: Harness::ClaudeCode,
        }
    }

    /// The same config, beating for `harness`.
    pub fn with_harness(mut self, harness: Harness) -> Self {
        self.harness = harness;
        self
    }
}

/// Where the pump reads its inbox and keeps the daemon and the heartbeat alive. `LiveDaemon` is
/// the real source; tests script one. `Sync` because the pump future holds it across awaits.
pub(crate) trait InboxSource: Sync {
    /// Makes sure a daemon serves the socket, spawning it when absent (the CLI's connect).
    fn ensure(&self) -> impl Future<Output = Result<(), BridgeError>> + Send;
    /// Long-polls the inbox for messages with `seq > after`, waiting at most `wait_ms`.
    fn fetch(
        &self,
        after: i64,
        wait_ms: u64,
    ) -> impl Future<Output = Result<Inbox, BridgeError>> + Send;
    /// Writes the `OwnedSessionApi` heartbeat for the identity.
    fn beat(&self);
}

/// The real inbox source: the daemon reached through the identity's `Ctx`, beating for `harness`.
struct LiveDaemon {
    ctx: agora_client::Ctx,
    harness: Harness,
}

impl InboxSource for LiveDaemon {
    async fn ensure(&self) -> Result<(), BridgeError> {
        // The spawning connect: starts the daemon when no socket answers. The client is dropped.
        agora_client::client::Client::connect(&self.ctx).await?;
        Ok(())
    }

    async fn fetch(&self, after: i64, wait_ms: u64) -> Result<Inbox, BridgeError> {
        fetch(&self.ctx, after, wait_ms, FETCH_LIMIT)
            .await
            .map_err(hook_error)
    }

    fn beat(&self) {
        heartbeat_for(&self.ctx, self.harness, Mechanism::OwnedSessionApi);
    }
}

/// Runs until `deliveries` is closed or `shutdown` fires: beat `OwnedSessionApi` at start and
/// every 30 s while the inbox answered recently; ensure the daemon at start and after each failed
/// fetch; loop { long-poll the inbox after the session cursor (agora_hooks::daemon fetch with
/// `wait_ms`, limit 50); keep `seq > cursor`, where the cursor is re-read after the poll so a
/// hook's advance is honored; none -> pause so the round lasts at least 1 s, then continue;
/// render with `agora_hooks::render::render_messages(max_chars)`; send `Delivery{text:
/// prompt_text(rendered), max_seq, count, reply}`; await reply; true -> `SessionLock::acquire(home,
/// session_key)`, cursor = max(cursor, max_seq), save, and the refusal backoff resets; false ->
/// sleep the refusal backoff (5 s doubling to 300 s) and offer the same batch again, or once it was
/// refused `REFUSAL_CAP` times in a row, skip it (error logged; cursor = its max_seq) }. Fetch
/// errors back off 1 s -> 30 s. Identity is an EXISTING one (`ctx_for`); a missing identity is an
/// error telling the user to start the agent once with agora installed or run `agora whoami` in
/// that folder. The beat continues while the pump waits for the daemon or the lock, and through a
/// driver wait whatever the inbox state, since one driver turn can outlast the beat window.
pub async fn run(
    cfg: PumpConfig,
    deliveries: mpsc::Sender<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    let ctx = ctx_for(&cfg.home, &cfg.folder).map_err(|e| identity_error(&cfg.folder, e))?;
    let source = LiveDaemon {
        ctx,
        harness: cfg.harness,
    };
    run_loop(&cfg, &source, deliveries, shutdown).await
}

/// The pump loop of `run`, over any inbox source.
async fn run_loop<S: InboxSource>(
    cfg: &PumpConfig,
    source: &S,
    deliveries: mpsc::Sender<Delivery>,
    shutdown: CancellationToken,
) -> Result<(), BridgeError> {
    source.beat();
    let mut pulse = Pulse::new(cfg.wait_ms);
    if ensure_daemon(source, &mut pulse, &shutdown).await.is_none() {
        return Ok(());
    }
    let mut backoff = BACKOFF_FIRST;
    let mut refusal = REFUSAL_FIRST;
    let mut refusals = Refusals::default();
    loop {
        let cursor = load(&cfg.home, &cfg.session_key).cursor;
        let started = Instant::now();
        let fetched = beating(
            source,
            &mut pulse,
            &shutdown,
            false,
            source.fetch(cursor, cfg.wait_ms),
        )
        .await;
        let Some(fetched) = fetched else {
            return Ok(());
        };
        let inbox = match fetched {
            Ok(inbox) => {
                pulse.alive_since = Instant::now();
                backoff = BACKOFF_FIRST;
                inbox
            }
            Err(err) => {
                tracing::warn!(error = %err, "agora inbox fetch failed; retrying");
                if ensure_daemon(source, &mut pulse, &shutdown).await.is_none() {
                    return Ok(());
                }
                let pause = tokio::time::sleep(backoff);
                if beating(source, &mut pulse, &shutdown, false, pause)
                    .await
                    .is_none()
                {
                    return Ok(());
                }
                backoff = next_backoff(backoff);
                continue;
            }
        };

        // A hook may have delivered messages while this poll was open: only what is still above
        // the cursor on disk is offered.
        let fresh_after = cursor.max(load(&cfg.home, &cfg.session_key).cursor);
        let msgs = select_fresh(inbox.messages, fresh_after);
        if msgs.is_empty() {
            let pause = round_pause(started.elapsed());
            if !pause.is_zero() {
                let pause = tokio::time::sleep(pause);
                if beating(source, &mut pulse, &shutdown, false, pause)
                    .await
                    .is_none()
                {
                    return Ok(());
                }
            }
            continue;
        }
        let Some((text, max_seq, count)) = batch_of(&msgs, cfg.max_chars) else {
            return Err(BridgeError::Client(agora_client::ClientError::Usage(
                format!(
                    "render cap of {} chars cannot show one agora message",
                    cfg.max_chars
                ),
            )));
        };

        let (reply, answer) = oneshot::channel();
        let delivery = Delivery {
            text,
            max_seq,
            count,
            reply,
        };
        match beating(
            source,
            &mut pulse,
            &shutdown,
            true,
            deliveries.send(delivery),
        )
        .await
        {
            // Shutdown, or the driver is gone: nothing more to deliver.
            None | Some(Err(_)) => return Ok(()),
            Some(Ok(())) => {}
        }
        let accepted = match beating(source, &mut pulse, &shutdown, true, answer).await {
            None => return Ok(()),
            Some(Ok(accepted)) => accepted,
            // A dropped reply is a refusal.
            Some(Err(_)) => false,
        };
        if !accepted {
            if refusals.refuse(max_seq) >= REFUSAL_CAP {
                tracing::error!(
                    seq = max_seq,
                    refusals = REFUSAL_CAP,
                    "agent refused the same agora batch repeatedly; skipped so the inbox is not blocked"
                );
                commit(&cfg.home, &cfg.session_key, max_seq).await?;
                refusals = Refusals::default();
                refusal = REFUSAL_FIRST;
                continue;
            }
            tracing::warn!(
                retry_in_s = refusal.as_secs(),
                "agent did not accept the agora batch; retrying"
            );
            let pause = tokio::time::sleep(refusal);
            if beating(source, &mut pulse, &shutdown, false, pause)
                .await
                .is_none()
            {
                return Ok(());
            }
            refusal = next_refusal(refusal);
            continue;
        }
        refusal = REFUSAL_FIRST;
        refusals = Refusals::default();
        commit(&cfg.home, &cfg.session_key, max_seq).await?;
    }
}

/// The heartbeat clock of one pump. `every` ticks every `BEAT_EVERY`; a tick beats only while the
/// inbox answered within `window`, where `alive_since` is the last successful fetch (or the start).
struct Pulse {
    every: Interval,
    alive_since: Instant,
    window: Duration,
}

impl Pulse {
    /// A clock whose first tick is one `BEAT_EVERY` from now, with the window of `beat_window`.
    fn new(wait_ms: u64) -> Self {
        let mut every = interval_at(Instant::now() + BEAT_EVERY, BEAT_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            every,
            alive_since: Instant::now(),
            window: beat_window(wait_ms),
        }
    }
}

/// How long after the last successful fetch the pump still beats: twice the long poll, and never
/// less than one beat period, so a pump that polls without a wait keeps beating.
fn beat_window(wait_ms: u64) -> Duration {
    Duration::from_millis(wait_ms.saturating_mul(2)).max(BEAT_EVERY)
}

/// Awaits `fut`, beating on every tick of `pulse` while the inbox was reached within its window,
/// until `fut` completes (`Some`) or `shutdown` fires (`None`). Shutdown is checked first. With
/// `keepalive` (a wait for the driver) every tick beats, since a driver turn can outlast the window.
async fn beating<S: InboxSource, F: Future>(
    source: &S,
    pulse: &mut Pulse,
    shutdown: &CancellationToken,
    keepalive: bool,
    fut: F,
) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return None,
            out = &mut fut => return Some(out),
            _ = pulse.every.tick() => {
                if keepalive || pulse.alive_since.elapsed() < pulse.window {
                    source.beat();
                }
            }
        }
    }
}

/// Ensures the daemon, logging a failure; `None` on shutdown.
async fn ensure_daemon<S: InboxSource>(
    source: &S,
    pulse: &mut Pulse,
    shutdown: &CancellationToken,
) -> Option<()> {
    match beating(source, pulse, shutdown, false, source.ensure()).await? {
        Ok(()) => {}
        Err(err) => tracing::warn!(error = %err, "agora daemon not reachable; retrying"),
    }
    Some(())
}

/// Maps a failed identity lookup to the error the user acts on.
fn identity_error(folder: &Path, err: HookError) -> BridgeError {
    BridgeError::Client(agora_client::ClientError::Usage(format!(
        "no agora identity for {}: {err}. Start the agent once with agora installed, or run \
         `agora whoami` in that folder.",
        folder.display()
    )))
}

/// Maps a hook-side error of the shared inbox read to the bridge's error.
fn hook_error(err: HookError) -> BridgeError {
    match err {
        HookError::Client(e) => BridgeError::Client(e),
        HookError::Io(e) => BridgeError::Io(e),
        other => BridgeError::Protocol(other.to_string()),
    }
}

/// The messages with an integer `seq` above `cursor`, in ascending seq order. Messages without
/// an integer `seq` are dropped: the cursor cannot track them.
fn select_fresh(mut msgs: Vec<Value>, cursor: i64) -> Vec<Value> {
    msgs.retain(|m| seq_of(m).is_some_and(|seq| seq > cursor));
    msgs.sort_by_key(seq_of);
    msgs
}

/// The pause after a round that fetched nothing new: what is left of `MIN_ROUND` after `elapsed`.
fn round_pause(elapsed: Duration) -> Duration {
    MIN_ROUND.saturating_sub(elapsed)
}

/// The prompt for `msgs` rendered within `max_chars`, as `(prompt text, max seq of the shown
/// messages, number of shown messages)`. `None` when nothing is shown. Only the messages that
/// `render_messages` frames are counted: the rest stay above the cursor for the next round.
fn batch_of(msgs: &[Value], max_chars: usize) -> Option<(String, i64, usize)> {
    if msgs.is_empty() {
        return None;
    }
    let rendered = render_messages(msgs, max_chars);
    if rendered.shown == 0 {
        return None;
    }
    // Shown messages are an ascending seq prefix, so the last one carries the highest seq.
    let max_seq = rendered.last_seq?;
    Some((prompt_text(&rendered.text), max_seq, rendered.shown))
}

/// The refusals in a row of the batch ending at `max_seq`. A refusal of another batch starts the
/// count again.
#[derive(Debug, Default)]
struct Refusals {
    max_seq: i64,
    count: u32,
}

impl Refusals {
    /// Counts a refusal of the batch ending at `max_seq`; returns how many in a row it has now.
    fn refuse(&mut self, max_seq: i64) -> u32 {
        if self.count == 0 || self.max_seq != max_seq {
            self.max_seq = max_seq;
            self.count = 0;
        }
        self.count += 1;
        self.count
    }
}

/// The retry delay after `d` for a fetch failure: doubled, at most `BACKOFF_MAX`.
fn next_backoff(d: Duration) -> Duration {
    d.saturating_mul(2).min(BACKOFF_MAX)
}

/// The pause before the next offer after a refusal `d`: doubled, at most `REFUSAL_MAX`.
fn next_refusal(d: Duration) -> Duration {
    d.saturating_mul(2).min(REFUSAL_MAX)
}

/// Advances the session cursor to `max_seq`, retrying a failed write `COMMIT_RETRIES` times with
/// `COMMIT_PAUSE` between attempts. Called only after the driver accepted the batch.
async fn commit(home: &Path, session_key: &str, max_seq: i64) -> Result<(), BridgeError> {
    for _ in 0..COMMIT_RETRIES {
        match commit_once(home, session_key, max_seq).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(error = %err, "session cursor write failed; retrying");
                tokio::time::sleep(COMMIT_PAUSE).await;
            }
        }
    }
    commit_once(home, session_key, max_seq).await
}

/// One cursor write under the session lock; the cursor never moves back.
async fn commit_once(home: &Path, session_key: &str, max_seq: i64) -> Result<(), BridgeError> {
    let mut lock = SessionLock::acquire(home, session_key).await?;
    let state = lock.state();
    state.cursor = state.cursor.max(max_seq);
    lock.save()?;
    Ok(())
}

#[cfg(test)]
mod tests;
