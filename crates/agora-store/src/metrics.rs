//! Flow metrics over cards and jobs.

use crate::{Store, StoreError};
use agora_core::CardState;
use agora_sched::model::MetricsReport;
use agora_sched::predict::{median, percentile};
use rusqlite::params;
use std::collections::BTreeMap;

/// The window start when `since_ms` is not given: 14 days before `now`.
const DEFAULT_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// The non-terminal card states, the keys of `wip`.
const WIP_STATES: [CardState; 6] = [
    CardState::Backlog,
    CardState::Ready,
    CardState::Claimed,
    CardState::Doing,
    CardState::Review,
    CardState::Blocked,
];

/// What one card's event history contributes to the report.
#[derive(Default)]
struct CardFlow {
    /// Timestamp of the first `card.created` event.
    created: Option<i64>,
    /// Timestamp of the first move into `doing`.
    first_doing: Option<i64>,
    /// Timestamp of the last move into `done`.
    last_done: Option<i64>,
    /// Blocked intervals that were left, as (entered, left).
    blocked: Vec<(i64, i64)>,
    /// When the card entered `blocked` and has not left yet.
    blocked_since: Option<i64>,
}

impl CardFlow {
    /// Folds one event into the flow. Events must arrive in sequence order.
    fn apply(&mut self, kind: &str, ts: i64, from: Option<&str>, to: Option<&str>) {
        match kind {
            "card.created" => {
                self.created.get_or_insert(ts);
            }
            "card.moved" => {
                // Leave before enter, so a move from blocked into blocked starts a new interval.
                if from == Some("blocked")
                    && let Some(entered) = self.blocked_since.take()
                {
                    self.blocked.push((entered, ts));
                }
                match to {
                    Some("doing") => {
                        self.first_doing.get_or_insert(ts);
                    }
                    Some("done") => self.last_done = Some(ts),
                    Some("blocked") => {
                        self.blocked_since.get_or_insert(ts);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Milliseconds to seconds, for every reported duration.
fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Milliseconds of `[start, end]` that lie inside `[since, now]`; zero when they do not overlap.
fn overlap(start: i64, end: i64, since: i64, now: i64) -> i64 {
    end.min(now).saturating_sub(start.max(since)).max(0)
}

impl Store {
    /// Flow metrics for cards in `team`/`lane` (both optional) and for all jobs, over the window
    /// `[since_ms, now]` (both ends inclusive; `since_ms` defaults to `now` minus 14 days).
    ///
    /// Cards come from `cards`; their events come from `events` (`card.created`, `card.moved` with
    /// data `{"from","to"}`), in sequence order. Durations are seconds.
    ///
    /// - `cards_done`: cards whose LAST move into `done` is in the window. Only these cards feed
    ///   the lead and cycle time samples.
    /// - Lead time: that done time minus the card's creation time, the first `card.created` event
    ///   or `cards.created` when there is none.
    /// - Cycle time: that done time minus the card's first move into `doing`. A card with no such
    ///   move at or before its done time has no cycle sample.
    /// - `lead_time_*`, `cycle_time_*`, `queue_wait_*`: p50 is `median`, p90 is the nearest-rank
    ///   `percentile`; None without samples.
    /// - `wip`: the CURRENT number of cards per non-terminal state (backlog, ready, claimed, doing,
    ///   review, blocked), zero-filled; not limited to the window.
    /// - Blocked time: for each card, the time it spent in `blocked` clipped to the window. An
    ///   interval starts at a move into `blocked` and ends at the next move out of it, or at `now`.
    ///   `blocked_time_total_s` sums all cards; `blocked_time_avg_s` divides by the cards with
    ///   positive blocked time in the window (None if there are none).
    /// - `jobs_done` / `jobs_failed`: tasks with `ended` in the window and state `done` / `failed`.
    /// - Queue wait: `started - requested` for tasks whose `started` is in the window.
    /// - The team and lane filters apply to cards only. Jobs (`work_tasks`) are not team- or
    ///   lane-scoped, so they are counted the same under any filter.
    pub fn metrics(
        &self,
        now: i64,
        team: Option<&str>,
        lane: Option<&str>,
        since_ms: Option<i64>,
    ) -> Result<MetricsReport, StoreError> {
        let since = since_ms.unwrap_or_else(|| now.saturating_sub(DEFAULT_WINDOW_MS));
        let in_window = since..=now;

        let flows = self.card_flows(team, lane)?;
        let mut lead = Vec::new();
        let mut cycle = Vec::new();
        let mut cards_done = 0u64;
        let mut blocked_ms = 0i64;
        let mut blocked_cards = 0u64;
        for (card_created, flow) in flows.values() {
            if let Some(done) = flow.last_done.filter(|d| in_window.contains(d)) {
                cards_done += 1;
                lead.push(secs(
                    done.saturating_sub(flow.created.unwrap_or(*card_created)),
                ));
                if let Some(doing) = flow.first_doing.filter(|d| *d <= done) {
                    cycle.push(secs(done.saturating_sub(doing)));
                }
            }
            let open = flow.blocked_since.map(|entered| (entered, now));
            let blocked_in_window = flow
                .blocked
                .iter()
                .copied()
                .chain(open)
                .map(|(start, end)| overlap(start, end, since, now))
                .fold(0, i64::saturating_add);
            if blocked_in_window > 0 {
                blocked_ms = blocked_ms.saturating_add(blocked_in_window);
                blocked_cards += 1;
            }
        }

        let wip = self.wip_counts(team, lane)?;
        let (jobs_done, jobs_failed) = self.job_outcomes(since, now)?;
        let waits = self.queue_waits(since, now)?;
        let blocked_total_s = secs(blocked_ms);

        Ok(MetricsReport {
            since_ms: since,
            until_ms: now,
            cards_done,
            lead_time_p50_s: median(&lead),
            lead_time_p90_s: percentile(&lead, 0.9),
            cycle_time_p50_s: median(&cycle),
            cycle_time_p90_s: percentile(&cycle, 0.9),
            wip,
            blocked_time_total_s: blocked_total_s,
            blocked_time_avg_s: (blocked_cards > 0).then(|| blocked_total_s / blocked_cards as f64),
            jobs_done,
            jobs_failed,
            queue_wait_p50_s: median(&waits),
            queue_wait_p90_s: percentile(&waits, 0.9),
        })
    }

    /// Each filtered card with its creation time from `cards`, and its event history folded into a
    /// [`CardFlow`]. Cards with no events are present with an empty flow.
    fn card_flows(
        &self,
        team: Option<&str>,
        lane: Option<&str>,
    ) -> Result<BTreeMap<String, (i64, CardFlow)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.id, c.created, e.kind, e.ts, json_extract(e.data, '$.from'), json_extract(e.data, '$.to')
             FROM cards c
             LEFT JOIN events e ON e.subject = c.id AND e.kind IN ('card.created', 'card.moved')
             WHERE (?1 IS NULL OR c.team = ?1) AND (?2 IS NULL OR c.lane = ?2)
             ORDER BY c.id, e.seq",
        )?;
        let rows = st.query_map(params![team, lane], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut flows: BTreeMap<String, (i64, CardFlow)> = BTreeMap::new();
        for row in rows {
            let (id, created, kind, ts, from, to) = row?;
            let entry = flows
                .entry(id)
                .or_insert_with(|| (created, CardFlow::default()));
            if let (Some(kind), Some(ts)) = (kind, ts) {
                entry.1.apply(&kind, ts, from.as_deref(), to.as_deref());
            }
        }
        Ok(flows)
    }

    /// Current card count per non-terminal state in the filter, zero-filled.
    fn wip_counts(
        &self,
        team: Option<&str>,
        lane: Option<&str>,
    ) -> Result<BTreeMap<String, u64>, StoreError> {
        let mut wip: BTreeMap<String, u64> = WIP_STATES
            .iter()
            .map(|s| (s.as_str().to_owned(), 0))
            .collect();
        let mut st = self.conn.prepare_cached(
            "SELECT c.state, COUNT(*) FROM cards c
             WHERE (?1 IS NULL OR c.team = ?1) AND (?2 IS NULL OR c.lane = ?2)
             GROUP BY c.state",
        )?;
        let rows = st.query_map(params![team, lane], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, n) = row?;
            // Terminal and unknown states have no key, so they are skipped.
            if let Some(count) = wip.get_mut(&state) {
                *count = u64::try_from(n).unwrap_or(0);
            }
        }
        Ok(wip)
    }

    /// (done, failed) task counts with `ended` in `[since, now]`.
    fn job_outcomes(&self, since: i64, now: i64) -> Result<(u64, u64), StoreError> {
        let (done, failed): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(state = 'done'), 0), COALESCE(SUM(state = 'failed'), 0)
             FROM work_tasks WHERE ended >= ?1 AND ended <= ?2",
            params![since, now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((
            u64::try_from(done).unwrap_or(0),
            u64::try_from(failed).unwrap_or(0),
        ))
    }

    /// Queue waits in seconds for tasks with `started` in `[since, now]`.
    fn queue_waits(&self, since: i64, now: i64) -> Result<Vec<f64>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT started - requested FROM work_tasks
             WHERE started >= ?1 AND started <= ?2 AND requested IS NOT NULL
             ORDER BY seq",
        )?;
        let rows = st.query_map(params![since, now], |r| r.get::<_, i64>(0))?;
        Ok(rows
            .map(|ms| ms.map(secs))
            .collect::<Result<Vec<f64>, rusqlite::Error>>()?)
    }
}

#[cfg(test)]
mod tests;
