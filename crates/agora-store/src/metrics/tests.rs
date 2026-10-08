use super::*;
use serde_json::json;

const SEC: i64 = 1_000;
const H: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * H;
/// The scripted history is read over the window [SINCE, NOW].
const SINCE: i64 = 10 * H;
const NOW: i64 = 100 * H;

fn store() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

fn card(s: &Store, id: &str, team: &str, lane: &str, state: &str, created: i64) {
    s.conn
        .execute(
            "INSERT INTO cards(id, title, lane, state, created, updated, team) \
             VALUES (?1, ?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, lane, state, created, team],
        )
        .expect("insert card");
}

fn event(s: &Store, ts: i64, kind: &str, subject: &str, data: &serde_json::Value) {
    s.conn
        .execute(
            "INSERT INTO events(ts, actor, kind, subject, data) VALUES (?1, 'test', ?2, ?3, ?4)",
            params![ts, kind, subject, data.to_string()],
        )
        .expect("insert event");
}

fn created(s: &Store, ts: i64, card: &str) {
    event(
        s,
        ts,
        "card.created",
        card,
        &json!({"title": card, "lane": "x"}),
    );
}

fn moved(s: &Store, ts: i64, card: &str, from: &str, to: &str) {
    event(s, ts, "card.moved", card, &json!({"from": from, "to": to}));
}

fn task(s: &Store, id: &str, state: &str, requested: i64, started: i64, ended: i64) {
    s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state, requested, started, ended) \
             VALUES (?1, 'test', ?2, ?3, ?2, ?4, ?5)",
            params![id, requested, state, started, ended],
        )
        .expect("insert task");
}

/// Every non-terminal state at zero, with the given counts overridden.
fn wip_with(counts: &[(&str, u64)]) -> BTreeMap<String, u64> {
    let mut wip: BTreeMap<String, u64> = WIP_STATES
        .iter()
        .map(|s| (s.as_str().to_owned(), 0))
        .collect();
    for (state, n) in counts {
        wip.insert((*state).to_owned(), *n);
    }
    wip
}

/// Cards A (core/alpha), B (core/beta), C (ops/alpha) and four jobs; read over [SINCE, NOW].
fn scripted() -> Store {
    let s = store();
    // A: created, doing, review, blocked for 1 h, done.
    card(&s, "a", "core", "alpha", "done", 20 * H);
    created(&s, 20 * H, "a");
    moved(&s, 22 * H, "a", "claimed", "doing");
    moved(&s, 24 * H, "a", "doing", "review");
    moved(&s, 25 * H, "a", "review", "blocked");
    moved(&s, 26 * H, "a", "blocked", "review");
    moved(&s, 30 * H, "a", "review", "done");
    // B: doing, blocked for 2 h, doing again, done.
    card(&s, "b", "core", "beta", "done", 15 * H);
    created(&s, 15 * H, "b");
    moved(&s, 16 * H, "b", "ready", "doing");
    moved(&s, 36 * H, "b", "doing", "blocked");
    moved(&s, 38 * H, "b", "blocked", "doing");
    moved(&s, 40 * H, "b", "doing", "done");
    // C: doing, then blocked and still blocked at NOW.
    card(&s, "c", "ops", "alpha", "blocked", 60 * H);
    created(&s, 60 * H, "c");
    moved(&s, 61 * H, "c", "ready", "doing");
    moved(&s, 90 * H, "c", "doing", "blocked");
    // Jobs: two done, one failed, one cancelled (its queue wait still counts).
    task(&s, "j1", "done", 20 * H, 20 * H + 30 * SEC, 21 * H);
    task(&s, "j2", "failed", 25 * H, 25 * H + 90 * SEC, 26 * H);
    task(&s, "j3", "done", 50 * H, 50 * H + 600 * SEC, 51 * H);
    task(&s, "j4", "cancelled", 60 * H, 60 * H + 10 * SEC, 61 * H);
    s
}

fn scripted_expected() -> MetricsReport {
    MetricsReport {
        since_ms: SINCE,
        until_ms: NOW,
        cards_done: 2,
        lead_time_p50_s: Some(63_000.0),
        lead_time_p90_s: Some(90_000.0),
        cycle_time_p50_s: Some(57_600.0),
        cycle_time_p90_s: Some(86_400.0),
        wip: wip_with(&[("blocked", 1)]),
        blocked_time_total_s: 46_800.0,
        blocked_time_avg_s: Some(15_600.0),
        jobs_done: 2,
        jobs_failed: 1,
        queue_wait_p50_s: Some(60.0),
        queue_wait_p90_s: Some(600.0),
    }
}

#[test]
fn scripted_history_gives_exact_values_for_every_field() {
    let report = scripted().metrics(NOW, None, None, Some(SINCE)).unwrap();
    assert_eq!(report, scripted_expected());
}

#[test]
fn team_filter_restricts_cards_but_not_jobs() {
    let s = scripted();
    // Team core: A and B only. Their blocked time is 1 h + 2 h; C is out.
    let core = s.metrics(NOW, Some("core"), None, Some(SINCE)).unwrap();
    assert_eq!(
        core,
        MetricsReport {
            wip: wip_with(&[]),
            blocked_time_total_s: 10_800.0,
            blocked_time_avg_s: Some(5_400.0),
            ..scripted_expected()
        }
    );
    // A team with no cards: card metrics are empty, jobs are unchanged.
    let none = s.metrics(NOW, Some("nobody"), None, Some(SINCE)).unwrap();
    assert_eq!(
        none,
        MetricsReport {
            cards_done: 0,
            lead_time_p50_s: None,
            lead_time_p90_s: None,
            cycle_time_p50_s: None,
            cycle_time_p90_s: None,
            wip: wip_with(&[]),
            blocked_time_total_s: 0.0,
            blocked_time_avg_s: None,
            ..scripted_expected()
        }
    );
}

#[test]
fn lane_and_team_and_lane_filters_select_cards() {
    let s = scripted();
    // Lane alpha: A (done, 1 h blocked) and C (blocked to NOW, 10 h): 11 h over two cards.
    let alpha = s.metrics(NOW, None, Some("alpha"), Some(SINCE)).unwrap();
    assert_eq!(
        alpha,
        MetricsReport {
            cards_done: 1,
            lead_time_p50_s: Some(36_000.0),
            lead_time_p90_s: Some(36_000.0),
            cycle_time_p50_s: Some(28_800.0),
            cycle_time_p90_s: Some(28_800.0),
            blocked_time_total_s: 39_600.0,
            blocked_time_avg_s: Some(19_800.0),
            ..scripted_expected()
        }
    );
    // Team core and lane alpha: only A.
    let both = s
        .metrics(NOW, Some("core"), Some("alpha"), Some(SINCE))
        .unwrap();
    assert_eq!(
        both,
        MetricsReport {
            cards_done: 1,
            lead_time_p50_s: Some(36_000.0),
            lead_time_p90_s: Some(36_000.0),
            cycle_time_p50_s: Some(28_800.0),
            cycle_time_p90_s: Some(28_800.0),
            wip: wip_with(&[]),
            blocked_time_total_s: 3_600.0,
            blocked_time_avg_s: Some(3_600.0),
            ..scripted_expected()
        }
    );
}

#[test]
fn empty_window_reports_zeros_and_none_but_current_wip() {
    // [1 h, 5 h] is before any history. C's open block starts later, so it adds nothing.
    let report = scripted().metrics(5 * H, None, None, Some(H)).unwrap();
    assert_eq!(
        report,
        MetricsReport {
            since_ms: H,
            until_ms: 5 * H,
            cards_done: 0,
            lead_time_p50_s: None,
            lead_time_p90_s: None,
            cycle_time_p50_s: None,
            cycle_time_p90_s: None,
            wip: wip_with(&[("blocked", 1)]),
            blocked_time_total_s: 0.0,
            blocked_time_avg_s: None,
            jobs_done: 0,
            jobs_failed: 0,
            queue_wait_p50_s: None,
            queue_wait_p90_s: None,
        }
    );
}

#[test]
fn empty_store_reports_zeros_and_none() {
    let report = store().metrics(NOW, None, None, None).unwrap();
    assert_eq!(
        report,
        MetricsReport {
            since_ms: NOW - 14 * DAY,
            until_ms: NOW,
            cards_done: 0,
            lead_time_p50_s: None,
            lead_time_p90_s: None,
            cycle_time_p50_s: None,
            cycle_time_p90_s: None,
            wip: wip_with(&[]),
            blocked_time_total_s: 0.0,
            blocked_time_avg_s: None,
            jobs_done: 0,
            jobs_failed: 0,
            queue_wait_p50_s: None,
            queue_wait_p90_s: None,
        }
    );
}

#[test]
fn default_window_is_fourteen_days_with_inclusive_start() {
    let s = store();
    // D is done at 39 h, one ms before the window start; E is done at 40 h, exactly on it.
    card(&s, "d", "t", "x", "done", 38 * H);
    moved(&s, 39 * H, "d", "doing", "done");
    card(&s, "e", "t", "x", "done", 38 * H);
    moved(&s, 39 * H, "e", "ready", "doing");
    moved(&s, 40 * H, "e", "doing", "done");
    // Jobs: one ended before the start (excluded), one failed at the start (included).
    task(&s, "j_out", "done", 38 * H, 38 * H, 39 * H);
    task(&s, "j_in", "failed", 40 * H, 40 * H, 40 * H);
    let now = 40 * H + 14 * DAY;
    let report = s.metrics(now, None, None, None).unwrap();
    assert_eq!(report.since_ms, 40 * H);
    assert_eq!(report.until_ms, now);
    assert_eq!(report.cards_done, 1);
    assert_eq!(report.lead_time_p50_s, Some(7_200.0));
    assert_eq!(report.cycle_time_p50_s, Some(3_600.0));
    assert_eq!(report.jobs_done, 0);
    assert_eq!(report.jobs_failed, 1);
    assert_eq!(report.queue_wait_p50_s, Some(0.0));
}

#[test]
fn last_move_into_done_decides_the_window() {
    let s = store();
    // R was done at 7 h (before the window), reopened, and done again at 50 h: it counts at 50 h.
    card(&s, "r", "t", "x", "done", 5 * H);
    created(&s, 5 * H, "r");
    moved(&s, 6 * H, "r", "ready", "doing");
    moved(&s, 7 * H, "r", "doing", "done");
    moved(&s, 12 * H, "r", "done", "review");
    moved(&s, 50 * H, "r", "review", "done");
    // Q is done only at 7 h: outside the window, so it is not counted.
    card(&s, "q", "t", "x", "done", 5 * H);
    created(&s, 5 * H, "q");
    moved(&s, 7 * H, "q", "doing", "done");
    let report = s.metrics(NOW, None, None, Some(SINCE)).unwrap();
    assert_eq!(report.cards_done, 1);
    // Lead: 50 h - 5 h = 45 h. Cycle: 50 h - first doing at 6 h = 44 h.
    assert_eq!(report.lead_time_p50_s, Some(162_000.0));
    assert_eq!(report.cycle_time_p50_s, Some(158_400.0));
}

#[test]
fn lead_time_falls_back_to_cards_created_and_cycle_needs_a_doing_move() {
    let s = store();
    // F has no card.created event: lead starts at cards.created (20 h).
    card(&s, "f", "t", "x", "done", 20 * H);
    moved(&s, 30 * H, "f", "ready", "doing");
    moved(&s, 35 * H, "f", "doing", "done");
    // H never entered doing: it has a lead sample but no cycle sample.
    card(&s, "h", "t", "x", "done", 20 * H);
    moved(&s, 25 * H, "h", "review", "done");
    let report = s.metrics(NOW, None, None, Some(SINCE)).unwrap();
    assert_eq!(report.cards_done, 2);
    // Lead samples: F 15 h = 54 000 s, H 5 h = 18 000 s.
    assert_eq!(report.lead_time_p50_s, Some(36_000.0));
    assert_eq!(report.lead_time_p90_s, Some(54_000.0));
    // Cycle samples: F only, 35 h - 30 h = 18 000 s.
    assert_eq!(report.cycle_time_p50_s, Some(18_000.0));
    assert_eq!(report.cycle_time_p90_s, Some(18_000.0));
}

#[test]
fn blocked_time_is_clipped_to_the_window_and_open_blocks_close_at_now() {
    let s = store();
    // K: blocked 5 h to 12 h, so 10 h to 12 h is inside the window (2 h).
    card(&s, "k", "t", "x", "doing", H);
    moved(&s, 5 * H, "k", "doing", "blocked");
    moved(&s, 12 * H, "k", "blocked", "doing");
    // L: blocked from 95 h and still blocked at NOW, so 95 h to 100 h (5 h).
    card(&s, "l", "t", "x", "blocked", H);
    moved(&s, 95 * H, "l", "doing", "blocked");
    // N: blocked 1 h to 2 h, entirely before the window: not counted.
    card(&s, "n", "t", "x", "doing", H);
    moved(&s, H, "n", "doing", "blocked");
    moved(&s, 2 * H, "n", "blocked", "doing");
    let report = s.metrics(NOW, None, None, Some(SINCE)).unwrap();
    assert_eq!(report.blocked_time_total_s, 25_200.0);
    // Two cards were blocked in the window (K, L), so the average is over two, not three.
    assert_eq!(report.blocked_time_avg_s, Some(12_600.0));
    assert_eq!(report.wip, wip_with(&[("doing", 2), ("blocked", 1)]));
}
