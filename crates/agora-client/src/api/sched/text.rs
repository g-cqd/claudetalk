//! Pure text renderers of the scheduling verbs: one line per record. Every untrusted string (names, notes,
//! resources, cron expressions, notice text) goes through `printable`, so a peer cannot move the cursor or hide
//! text in the terminal.

use agora_sched::cron::civil_from_ms;
use agora_sched::model::{
    CardExt, LeaseKind, LeaseRecord, MetricsReport, NoticeRecord, PolicyRecord, PoolRecord,
    TimerRecord, WatchdogReport, WipView,
};
use agora_sched::pyfmt::printable;
use agora_sched::timer::describe;

/// `YYYY-MM-DDTHH:MM:SSZ` of a UTC instant given in ms since the epoch.
pub(super) fn iso_utc(ms: i64) -> String {
    let (year, month, day, hour, minute, _weekday) = civil_from_ms(ms);
    let second = ms.rem_euclid(60_000) / 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `iso_utc`, or "-" when the record has no such instant.
fn opt_iso(ms: Option<i64>) -> String {
    ms.map_or_else(|| "-".to_string(), iso_utc)
}

/// A number as Rust prints it (`2`, `0.5`).
fn num(x: f64) -> String {
    format!("{x}")
}

/// `num`, or "-" when absent.
fn opt_num(x: Option<f64>) -> String {
    x.map_or_else(|| "-".to_string(), num)
}

/// Seconds with one decimal, or "-" when absent.
fn secs(x: Option<f64>) -> String {
    x.map_or_else(|| "-".to_string(), |s| format!("{s:.1}s"))
}

/// A WIP limit, or "-" when the lane has none.
fn limit_text(limit: Option<u32>) -> String {
    limit.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// Space-joins the non-empty parts.
fn words(parts: &[&str]) -> String {
    parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Two spaces in front of a nested line.
fn indented(line: String) -> String {
    format!("  {line}")
}

/// Lines joined with newlines.
pub(super) fn join<I: IntoIterator<Item = String>>(lines: I) -> String {
    lines.into_iter().collect::<Vec<_>>().join("\n")
}

pub(super) fn timer_line(t: &TimerRecord) -> String {
    let state = if t.enabled { "enabled" } else { "disabled" };
    let owner = format!("owner {}", t.owner_name);
    let next = format!("next {}", opt_iso(t.next_fire_ms));
    let fired = format!("fired {}", t.fired);
    printable(&words(&[
        &t.id,
        state,
        &describe(&t.spec),
        &owner,
        &next,
        &fired,
        &t.template.title,
    ]))
}

pub(super) fn pool_line(p: &PoolRecord) -> String {
    let used = format!("{}/{}", num(p.used), num(p.capacity));
    let by = format!("by {}", p.created_by);
    printable(&words(&[&p.name, &used, &p.unit, &by, &p.note]))
}

pub(super) fn lease_line(l: &LeaseRecord) -> String {
    let amount = match l.kind {
        LeaseKind::Pool => format!("x{}", num(l.amount)),
        LeaseKind::Path | LeaseKind::Quiet => String::new(),
    };
    let expires = format!("expires {}", opt_iso(l.expires_ms));
    let released = if l.released_ms.is_some() {
        "released"
    } else {
        ""
    };
    printable(&words(&[
        &l.id,
        l.kind.as_str(),
        &amount,
        &l.resource,
        "held by",
        &l.holder_name,
        &expires,
        released,
    ]))
}

pub(super) fn policy_line(p: &PolicyRecord) -> String {
    let policy = &p.policy;
    let chain = policy.escalation_chain.as_ref().map_or(0, Vec::len);
    printable(&words(&[
        &format!("{}/{}", p.team, p.lane),
        &format!("wip {}", limit_text(policy.wip_limit)),
        &format!("stale {}", opt_num(policy.stale_after_s)),
        &format!("escalate {}", opt_num(policy.escalate_after_s)),
        &format!("reclaim {}", opt_num(policy.reclaim_after_s)),
        &format!("chain {chain}"),
        &format!("by {}", p.by),
    ]))
}

pub(super) fn card_ext_line(c: &CardExt) -> String {
    let team = if c.team.is_empty() {
        String::new()
    } else {
        format!("team {}", c.team)
    };
    let after = if c.after.is_empty() {
        String::new()
    } else {
        format!("after {}", c.after.join(","))
    };
    let owner = if c.needs_owner {
        format!("needs owner: {}", c.owner_action)
    } else {
        String::new()
    };
    printable(&words(&[
        &c.id,
        &format!("p{}", c.priority),
        &format!("value {}", num(c.value)),
        &format!("size {}", num(c.size)),
        &team,
        &after,
        &owner,
    ]))
}

pub(super) fn notice_line(n: &NoticeRecord) -> String {
    let head = words(&[
        &format!("#{}", n.seq),
        &iso_utc(n.ts_ms),
        &n.kind,
        &n.target,
        &n.subject,
    ]);
    if n.text.is_empty() {
        printable(&head)
    } else {
        printable(&format!("{head}: {}", n.text))
    }
}

/// The watchdog report: a header per section with its count, then one indented line per item.
pub(super) fn watchdog_text(r: &WatchdogReport) -> String {
    let mut lines = vec![format!("watchdog at {}", iso_utc(r.now_ms))];
    if let Some(paused) = &r.paused {
        lines.push(printable(&format!("admission paused: {paused}")));
    }
    lines.push(format!("stale cards: {}", r.stale.len()));
    lines.extend(r.stale.iter().map(|s| {
        let idle = format!("idle {}s", num(s.idle_s));
        indented(printable(&words(&[
            &s.id, &s.state, &idle, &s.stage, &s.title,
        ])))
    }));
    lines.push(format!("expiring leases: {}", r.expiring_leases.len()));
    lines.extend(r.expiring_leases.iter().map(|l| indented(lease_line(l))));
    lines.push(format!("notices: {}", r.notices.len()));
    lines.extend(r.notices.iter().map(|n| indented(notice_line(n))));
    lines.push(format!("needs owner: {}", r.needs_owner.len()));
    lines.extend(
        r.needs_owner
            .iter()
            .map(|n| indented(printable(&words(&[&n.card, &n.title, &n.owner_action])))),
    );
    lines.push(format!("wip: {}", r.wip.len()));
    lines.extend(r.wip.iter().map(|w| indented(wip_line(w))));
    lines.push(format!("orphaned jobs: {}", r.orphaned_jobs.len()));
    lines.extend(r.orphaned_jobs.iter().map(|job| indented(printable(job))));
    join(lines)
}

fn wip_line(w: &WipView) -> String {
    let count = format!("{}/{}", w.count, limit_text(w.limit));
    printable(&words(&[&format!("{}/{}", w.team, w.lane), &count]))
}

/// The metrics report: one line per measure, then one line per lane's WIP count.
pub(super) fn metrics_text(r: &MetricsReport) -> String {
    let mut lines = vec![
        format!("metrics {} .. {}", iso_utc(r.since_ms), iso_utc(r.until_ms)),
        format!("cards done: {}", r.cards_done),
        format!(
            "lead time p50 {} p90 {}",
            secs(r.lead_time_p50_s),
            secs(r.lead_time_p90_s)
        ),
        format!(
            "cycle time p50 {} p90 {}",
            secs(r.cycle_time_p50_s),
            secs(r.cycle_time_p90_s)
        ),
        format!(
            "blocked: total {} avg {}",
            secs(Some(r.blocked_time_total_s)),
            secs(r.blocked_time_avg_s)
        ),
        format!("jobs: done {} failed {}", r.jobs_done, r.jobs_failed),
        format!(
            "queue wait p50 {} p90 {}",
            secs(r.queue_wait_p50_s),
            secs(r.queue_wait_p90_s)
        ),
    ];
    lines.extend(
        r.wip
            .iter()
            .map(|(lane, n)| printable(&format!("  wip {lane}: {n}"))),
    );
    join(lines)
}

pub(super) fn escalated_text(card: &str, notified: &[String]) -> String {
    if notified.is_empty() {
        printable(&format!("escalated {card}: nobody to notify"))
    } else {
        printable(&format!(
            "escalated {card}: notified {}",
            notified.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::json;

    /// Builds a record from JSON, so the tests state only the fields they care about.
    fn record<T: DeserializeOwned>(v: serde_json::Value) -> T {
        serde_json::from_value(v).expect("valid record")
    }

    /// 2023-11-14T22:13:20Z
    const T0: i64 = 1_700_000_000_000;

    #[test]
    fn iso_utc_formats_the_epoch_and_a_known_instant() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(T0), "2023-11-14T22:13:20Z");
        // The sub-second part is dropped.
        assert_eq!(iso_utc(T0 + 999), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn iso_utc_of_a_negative_instant_falls_in_the_previous_second() {
        assert_eq!(iso_utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn timer_line_shows_state_schedule_owner_and_next_fire() {
        let t: TimerRecord = record(json!({
            "id": "t1", "owner": "pk", "owner_name": "alice",
            "spec": {"kind": "every", "every_s": 60},
            "template": {"title": "nightly"},
            "enabled": true, "next_fire_ms": T0, "fired": 2, "created_ms": 0
        }));
        let expected = format!(
            "t1 enabled {} owner alice next 2023-11-14T22:13:20Z fired 2 nightly",
            describe(&t.spec)
        );
        assert_eq!(timer_line(&t), expected);
    }

    #[test]
    fn timer_line_marks_a_disabled_timer_without_a_next_fire() {
        let t: TimerRecord = record(json!({
            "id": "t2", "owner": "pk", "owner_name": "bob",
            "spec": {"kind": "at", "at_ms": T0},
            "template": {}, "enabled": false, "created_ms": 0
        }));
        let line = timer_line(&t);
        assert!(line.starts_with("t2 disabled "), "{line}");
        assert!(line.contains(" next - fired 0"), "{line}");
    }

    #[test]
    fn timer_line_escapes_a_control_character_in_the_owner_name() {
        let t: TimerRecord = record(json!({
            "id": "t3", "owner": "pk", "owner_name": "al\nice",
            "spec": {"kind": "every", "every_s": 60}, "template": {}, "created_ms": 0
        }));
        let line = timer_line(&t);
        assert!(line.contains("owner al\\nice"), "{line}");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn pool_line_shows_used_over_capacity_and_drops_an_empty_unit() {
        let p: PoolRecord = record(json!({
            "name": "build", "capacity": 4.0, "used": 1.5,
            "created_ms": 0, "created_by": "ops"
        }));
        assert_eq!(pool_line(&p), "build 1.5/4 by ops");
    }

    #[test]
    fn lease_line_shows_kind_resource_holder_and_utc_expiry() {
        let l: LeaseRecord = record(json!({
            "id": "l1", "kind": "path", "resource": "/tmp/x",
            "amount": 0.0, "holder": "pk", "holder_name": "alice",
            "created_ms": 0, "expires_ms": T0, "heartbeat_ms": 0
        }));
        assert_eq!(
            lease_line(&l),
            "l1 path /tmp/x held by alice expires 2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn lease_line_shows_the_pool_amount_and_a_released_marker() {
        let l: LeaseRecord = record(json!({
            "id": "l2", "kind": "pool", "resource": "build", "amount": 2.0,
            "holder": "pk", "holder_name": "bob", "created_ms": 0,
            "heartbeat_ms": 0, "released_ms": T0
        }));
        assert_eq!(
            lease_line(&l),
            "l2 pool x2 build held by bob expires - released"
        );
    }

    #[test]
    fn lease_line_escapes_untrusted_characters_in_the_resource() {
        let l: LeaseRecord = record(json!({
            "id": "l3", "kind": "path", "resource": "a\u{1b}[2Jb",
            "amount": 0.0, "holder": "pk", "holder_name": "eve",
            "created_ms": 0, "heartbeat_ms": 0
        }));
        let line = lease_line(&l);
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("a\\x1b[2Jb"), "{line}");
    }

    #[test]
    fn policy_line_shows_dashes_for_unset_limits() {
        let p: PolicyRecord = record(json!({
            "team": "core", "lane": "api", "policy": {},
            "updated_ms": 0, "by": "carol"
        }));
        assert_eq!(
            policy_line(&p),
            "core/api wip - stale - escalate - reclaim - chain 0 by carol"
        );
    }

    #[test]
    fn card_ext_line_lists_dependencies_and_an_open_owner_action() {
        let c: CardExt = record(json!({
            "id": "c1", "priority": 3, "value": 5.0, "size": 2.5,
            "team": "core", "needs_owner": true, "owner_action": "pick it up",
            "after": ["c0", "c9"]
        }));
        assert_eq!(
            card_ext_line(&c),
            "c1 p3 value 5 size 2.5 team core after c0,c9 needs owner: pick it up"
        );
    }

    #[test]
    fn notice_line_escapes_newlines_in_the_text() {
        let n: NoticeRecord = record(json!({
            "seq": 7, "ts_ms": T0, "target": "pk", "kind": "reconcile.notice",
            "subject": "c1", "text": "line one\nline two"
        }));
        let line = notice_line(&n);
        assert_eq!(
            line,
            "#7 2023-11-14T22:13:20Z reconcile.notice pk c1: line one\\nline two"
        );
    }

    #[test]
    fn watchdog_text_lists_each_section_with_its_count_and_items() {
        let r: WatchdogReport = record(json!({
            "now_ms": T0,
            "stale": [{"id": "c1", "title": "fix\tit", "state": "doing",
                       "idle_s": 90.0, "stage": "warn"}],
            "wip": [{"team": "core", "lane": "api", "count": 3, "limit": 2}],
            "orphaned_jobs": ["t9"]
        }));
        let text = watchdog_text(&r);
        assert!(
            text.starts_with("watchdog at 2023-11-14T22:13:20Z\n"),
            "{text}"
        );
        assert!(
            text.contains("stale cards: 1\n  c1 doing idle 90s warn fix\\tit"),
            "{text}"
        );
        assert!(text.contains("expiring leases: 0\n"), "{text}");
        assert!(text.contains("  core/api 3/2"), "{text}");
        assert!(text.ends_with("orphaned jobs: 1\n  t9"), "{text}");
    }

    #[test]
    fn metrics_text_shows_missing_percentiles_as_a_dash() {
        let r: MetricsReport = record(json!({
            "since_ms": 0, "until_ms": 0, "cards_done": 2,
            "lead_time_p50_s": 12.5, "wip": {"core/api": 4}
        }));
        let text = metrics_text(&r);
        assert!(text.contains("cards done: 2\n"), "{text}");
        assert!(text.contains("lead time p50 12.5s p90 -\n"), "{text}");
        assert!(text.contains("cycle time p50 - p90 -\n"), "{text}");
        assert!(text.ends_with("  wip core/api: 4"), "{text}");
    }

    #[test]
    fn escalated_text_names_nobody_when_the_chain_is_empty() {
        assert_eq!(escalated_text("c1", &[]), "escalated c1: nobody to notify");
        assert_eq!(
            escalated_text("c1", &["ann".to_string(), "bo".to_string()]),
            "escalated c1: notified ann, bo"
        );
    }
}
