//! Public-API tests of the reconciler's effects (apply_reconcile, reconcile_input, watchdog_report), the job lifecycle
//! and job history, the flow metrics over a scripted history, and retention (work_prune). Rules: SPEC section 6 (apply
//! semantics), section 3.2, and the decisions R3-10, R3-14/O-16, O-33 and O-36.

mod common;
#[path = "reconcile_metrics/support.rs"]
mod support;

use agora_core::{CardState, Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::model::{LeaseRecord, MetricsReport, TaskState};
use agora_sched::predict::Run;
use agora_sched::reconcile::{CardSnap, Mark, ReconcileAction, ReconcileInput, SprintSnap};
use agora_store::{Store, StoreError, hex};
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use support::*;

// ---- helpers ----------------------------------------------------------------------------------------------------

/// A `wip` map with every non-terminal state present, the given counts set.
fn wip_counts(counts: [(&str, u64); 6]) -> BTreeMap<String, u64> {
    counts
        .into_iter()
        .map(|(state, n)| (state.to_string(), n))
        .collect()
}

fn input(w: &World, now: i64) -> ReconcileInput {
    w.s.reconcile_input(now, None, false)
        .expect("reconcile input")
}

fn controller(w: &mut World) -> Identity {
    let ctl = Identity::from_seed([9; 32]);
    w.s.register_identity(NOW, &ctl.public(), "ctl")
        .expect("register controller");
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&ctl), "controller"),
    )
    .expect("grant controller");
    ctl
}

/// A path lease held by alice, expiring `ttl_s` seconds after NOW.
fn path_lease(w: &mut World, resource: &str, ttl_s: f64) -> LeaseRecord {
    let arg = json!({"kind": "path", "resource": resource, "ttl_s": ttl_s}).to_string();
    w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &arg))
        .expect("acquire path lease")
}

/// A quiet window opened by the controller, ending at `end_ms`.
fn quiet_window(w: &mut World, ctl: &Identity, end_ms: i64) -> LeaseRecord {
    let arg = json!({"expected_end_ms": end_ms, "note": "maintenance"}).to_string();
    w.s.quiet_open(NOW, &op(ctl, "quiet.open", "", &arg))
        .expect("open quiet window")
}

/// The release time of `lease` as the store reports it at `now`; None while it is open.
fn released(s: &Store, lease: &str, now: i64) -> Option<i64> {
    s.leases(now, None, None, false)
        .expect("leases")
        .into_iter()
        .find(|l| l.id == lease)
        .expect("the lease exists")
        .released_ms
}

fn notices_of(w: &World, who: &Identity) -> Vec<(String, String, String, i64)> {
    w.s.notices(&pk_hex(who), 0, 100)
        .expect("notices")
        .into_iter()
        .map(|n| (n.kind, n.subject, n.text, n.ts_ms))
        .collect()
}

fn open_blocker(
    w: &mut World,
    sys: &Identity,
    key: &str,
    title: &str,
    owner_action: &str,
    ts: i64,
) -> bool {
    let action = ReconcileAction::Blocker {
        key: key.into(),
        open: true,
        title: title.into(),
        owner_action: owner_action.into(),
    };
    w.s.apply_reconcile(ts, &act(sys, &action, ts))
        .expect("apply blocker")
}

fn close_blocker(w: &mut World, sys: &Identity, key: &str, ts: i64) -> bool {
    let action = ReconcileAction::Blocker {
        key: key.into(),
        open: false,
        title: String::new(),
        owner_action: String::new(),
    };
    w.s.apply_reconcile(ts, &act(sys, &action, ts))
        .expect("apply blocker close")
}

const BLOCKER_TITLE: &str = "Machine floor breached: disk free 3.1 GiB below the floor of 5.0 GiB";
const BLOCKER_ACTION: &str =
    "free disk (3.1 GiB < floor 5.0 GiB) -- admission is paused until then";

/// A card claimed by alice at `at(2)`, for the reclaim tests.
fn claimed_card(w: &mut World, id: &str) {
    card_at(&mut w.s, &w.sup, id, "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, id, "ready", at(0));
    move_card(&mut w.s, &w.alice, id, "claimed", at(2));
}

/// (ts, actor, data) of every card.moved event for `card`, in sequence order, read from the audit table.
fn card_moves(scratch: &Scratch, card: &str) -> Vec<(i64, String, Value)> {
    let conn = scratch.conn();
    let mut st = conn
        .prepare("SELECT ts, actor, data FROM events WHERE kind = 'card.moved' AND subject = ?1 ORDER BY seq")
        .expect("prepare");
    let rows = st
        .query_map([card], |r| {
            let data: String = r.get(2)?;
            let data = serde_json::from_str(&data).expect("event data is JSON");
            Ok((r.get(0)?, r.get(1)?, data))
        })
        .expect("query");
    rows.collect::<Result<_, _>>().expect("rows")
}

/// Evidence refs recorded on `card`, from the audit table.
fn evidence_refs(scratch: &Scratch, card: &str) -> Vec<String> {
    let conn = scratch.conn();
    let mut st = conn
        .prepare("SELECT ref FROM evidence WHERE card = ?1 ORDER BY rowid")
        .expect("prepare");
    let rows = st.query_map([card], |r| r.get(0)).expect("query");
    rows.collect::<Result<_, _>>().expect("rows")
}

/// Events of the given kind pattern (SQL LIKE) older than `cut`, from the audit table.
fn events_before(scratch: &Scratch, pattern: &str, cut: i64) -> i64 {
    scratch
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind LIKE ?1 AND ts < ?2",
            (pattern, cut),
            |r| r.get(0),
        )
        .expect("count events")
}

/// Work and job events at or after `cut`, from the audit table.
fn work_events_from(scratch: &Scratch, cut: i64) -> i64 {
    scratch
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE (kind LIKE 'work.%' OR kind LIKE 'job.%') AND ts >= ?1",
            [cut],
            |r| r.get(0),
        )
        .expect("count events")
}

// ---- reconcile_input ------------------------------------------------------------------------------------------------

#[test]
fn the_input_lists_the_active_supervisor_keys_sorted_and_never_the_system_identity() {
    let mut w = world();
    let _sys = system(&mut w);
    let mut expected = vec![pk_hex(&w.owner), pk_hex(&w.sup)];
    expected.sort();
    assert_eq!(input(&w, NOW).supervisors, expected);
}

#[test]
fn the_input_lists_only_claimed_and_doing_cards_ordered_by_last_update() {
    let mut w = world();
    card_at(&mut w.s, &w.sup, "bk", "core", "alpha", at(0));
    card_at(&mut w.s, &w.sup, "rd", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "rd", "ready", at(1));
    card_at(&mut w.s, &w.sup, "cl", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "cl", "ready", at(1));
    move_card(&mut w.s, &w.alice, "cl", "claimed", at(4));
    card_at(&mut w.s, &w.sup, "dg", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "dg", "ready", at(1));
    move_card(&mut w.s, &w.alice, "dg", "claimed", at(2));
    move_card(&mut w.s, &w.alice, "dg", "doing", at(3));
    card_at(&mut w.s, &w.sup, "bl", "core", "alpha", at(0));
    move_card(&mut w.s, &w.sup, "bl", "blocked", at(5));
    // "dg" sorts after "cl" by id but was updated first, so the order is by last update, not by id.
    let alice = pk_hex(&w.alice);
    assert_eq!(
        input(&w, NOW).cards,
        vec![
            CardSnap {
                id: "dg".into(),
                title: "dg".into(),
                team: "core".into(),
                lane: "alpha".into(),
                state: "doing".into(),
                owner: Some(alice.clone()),
                updated_ms: at(3),
            },
            CardSnap {
                id: "cl".into(),
                title: "cl".into(),
                team: "core".into(),
                lane: "alpha".into(),
                state: "claimed".into(),
                owner: Some(alice),
                updated_ms: at(4),
            },
        ]
    );
}

#[test]
fn the_input_lists_active_sprints_with_the_cards_not_yet_done_or_cancelled() {
    let mut w = world();
    let sup = &w.sup;
    let window =
        |name: &str| json!({"name": name, "goal": "", "starts": 0, "ends": at(50)}).to_string();
    w.s.create_sprint(NOW, &op(sup, "sprint.create", "s-plan", &window("Planned")))
        .expect("plan");
    w.s.create_sprint(NOW, &op(sup, "sprint.create", "s-now", &window("Sprint 4")))
        .expect("plan");
    w.s.start_sprint(NOW, &op(sup, "sprint.start", "s-now", ""))
        .expect("start");
    for card in ["k1", "k2", "k3"] {
        card_at(&mut w.s, sup, card, "core", "alpha", NOW);
    }
    move_card(&mut w.s, sup, "k2", "cancelled", NOW);
    move_card(&mut w.s, sup, "k3", "ready", NOW);
    for card in ["k1", "k2", "k3"] {
        w.s.add_to_sprint(NOW, &op(sup, "sprint.add", "s-now", card))
            .expect("add to sprint");
    }
    assert_eq!(
        input(&w, NOW).sprints,
        vec![SprintSnap {
            id: "s-now".into(),
            name: "Sprint 4".into(),
            ends_ms: at(50),
            state: "active".into(),
            open_cards: 2,
        }]
    );
}

// ---- watchdog_report stages ----------------------------------------------------------------------------------------

#[test]
fn the_watchdog_stage_follows_the_policy_thresholds_at_their_boundaries() {
    // Defaults: stale after 1800 s, escalated after a further 900 s, reclaim due at 3600 s idle.
    let mut w = world();
    claimed_card(&mut w, "w1");
    let claimed_at = at(2);
    let cases: [(i64, Option<(&str, f64)>); 6] = [
        (1_799_999, None),
        (1_800_000, Some(("stale", 1800.0))),
        (2_699_999, Some(("stale", 2699.999))),
        (2_700_000, Some(("escalated", 2700.0))),
        (3_599_999, Some(("escalated", 3599.999))),
        (3_600_000, Some(("reclaim due", 3600.0))),
    ];
    for (idle_ms, expected) in cases {
        let report =
            w.s.watchdog_report(claimed_at + idle_ms, &pk_hex(&w.alice), false)
                .expect("watchdog");
        let got = report.stale.first().map(|c| (c.stage.as_str(), c.idle_s));
        assert_eq!(got, expected, "idle {idle_ms} ms");
    }
}

#[path = "reconcile_metrics/apply.rs"]
mod apply;
#[path = "reconcile_metrics/flow.rs"]
mod flow;
#[path = "reconcile_metrics/jobs.rs"]
mod jobs;
