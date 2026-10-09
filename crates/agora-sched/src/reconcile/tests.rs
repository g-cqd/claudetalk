//! Unit tests for the reconciler planner: each rule, its thresholds and texts, marks, the pass cap, and
//! input-order independence.

use super::*;
use crate::admit::{MachineSnapshot, Signals};
use crate::model::{LeaseKind, LeaseRecord, PolicyRecord};
use crate::policy::TeamPolicy;
use crate::settings::WorkSettings;

const NOW: i64 = 1_800_000_000_000;
const GIB_KIB: u64 = 1024 * 1024;
const DISK_OWNER_ACTION: &str =
    "free disk space (1.0 GiB < floor 5.0 GiB) — admission is paused until then";

/// A 64-character lowercase hex key starting with `tag`; its pseudonym is the first eight characters.
fn key(tag: &str) -> String {
    format!("{tag:0<64}")
}

fn input() -> ReconcileInput {
    ReconcileInput {
        claims: vec![],
        now_ms: NOW,
        settings: WorkSettings::default(),
        machine: None,
        cards: Vec::new(),
        leases: Vec::new(),
        policies: Vec::new(),
        require_review_default: false,
        marks: Vec::new(),
        supervisors: Vec::new(),
        admission_paused: None,
        blocker_card: None,
        sprints: Vec::new(),
    }
}

/// A doing card in core/dev, owned by `key(owner)` and last updated `idle_s` seconds ago.
fn card(id: &str, state: &str, owner: Option<&str>, idle_s: i64) -> CardSnap {
    CardSnap {
        id: id.to_owned(),
        title: format!("Title {id}"),
        team: "core".to_owned(),
        lane: "dev".to_owned(),
        state: state.to_owned(),
        owner: owner.map(key),
        updated_ms: NOW - idle_s * 1000,
    }
}

fn lane_card(id: &str, team: &str, lane: &str, idle_s: i64) -> CardSnap {
    let mut c = card(id, "doing", Some("aa"), idle_s);
    c.team = team.to_owned();
    c.lane = lane.to_owned();
    c
}

fn lease(id: &str, kind: LeaseKind) -> LeaseRecord {
    LeaseRecord {
        id: id.to_owned(),
        kind,
        resource: "res".to_owned(),
        amount: 1.0,
        holder: key("aa"),
        holder_name: "holder".to_owned(),
        card: None,
        task: None,
        created_ms: NOW - 1000,
        expires_ms: None,
        heartbeat_ms: NOW - 1000,
        expected_end_ms: None,
        released_ms: None,
        note: String::new(),
    }
}

fn sprint(id: &str, name: &str, state: &str, ends_ms: i64, open_cards: u32) -> SprintSnap {
    SprintSnap {
        id: id.to_owned(),
        name: name.to_owned(),
        ends_ms,
        state: state.to_owned(),
        open_cards,
    }
}

fn policy(team: &str, lane: &str, policy: TeamPolicy) -> PolicyRecord {
    PolicyRecord {
        team: team.to_owned(),
        lane: lane.to_owned(),
        policy,
        updated_ms: 0,
        by: key("aa"),
    }
}

fn machine(disk_free_kib: u64) -> MachineSnapshot {
    MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: None,
        disk_free_kib: Some(disk_free_kib),
        signals: None,
    }
}

fn healthy() -> MachineSnapshot {
    machine(100 * GIB_KIB)
}

/// The R1 action for lease `id` expiring at `expires` (its key carries the deadline).
fn expire(id: &str, expires: i64) -> ReconcileAction {
    ReconcileAction::ExpireLease {
        key: format!("expire:{id}:{expires}"),
        lease: id.to_owned(),
    }
}

fn notice(dedupe: &str, target: &str, kind: &str, subject: &str, text: &str) -> ReconcileAction {
    ReconcileAction::Notice {
        key: dedupe.to_owned(),
        target: target.to_owned(),
        kind: kind.to_owned(),
        subject: subject.to_owned(),
        text: text.to_owned(),
    }
}

fn mark(dedupe: &str) -> Mark {
    Mark {
        key: dedupe.to_owned(),
        last_ms: NOW - 1,
        count: 1,
    }
}

fn policy_of(stale_s: f64) -> TeamPolicy {
    TeamPolicy {
        stale_after_s: Some(stale_s),
        ..TeamPolicy::default()
    }
}

mod rules_claims;
mod rules_r1_r3;
mod rules_r4_r6;

#[test]
fn marks_suppress_matching_keys_only() {
    let mut a = lease("l1", LeaseKind::Path);
    a.expires_ms = Some(NOW - 1);
    let mut b = lease("l2", LeaseKind::Path);
    b.expires_ms = Some(NOW - 1);
    let i = ReconcileInput {
        leases: vec![a, b],
        marks: vec![mark(&format!("expire:l1:{}", NOW - 1)), mark("unrelated")],
        ..input()
    };
    assert_eq!(plan(&i), vec![expire("l2", NOW - 1)]);
}

#[test]
fn marks_do_not_consume_the_pass_cap() {
    let leases: Vec<LeaseRecord> = (0..65)
        .map(|n| {
            let mut l = lease(&format!("l{n:02}"), LeaseKind::Path);
            l.expires_ms = Some(NOW - 1);
            l
        })
        .collect();
    let i = ReconcileInput {
        leases,
        marks: vec![mark(&format!("expire:l00:{}", NOW - 1))],
        ..input()
    };
    let actions = plan(&i);
    assert_eq!(actions.len(), MAX_ACTIONS_PER_PASS);
    assert_eq!(actions.first(), Some(&expire("l01", NOW - 1)));
    assert_eq!(actions.last(), Some(&expire("l64", NOW - 1)));
}

#[test]
fn pass_is_capped_and_later_rules_are_the_ones_dropped() {
    let leases: Vec<LeaseRecord> = (0..70)
        .map(|n| {
            let mut l = lease(&format!("l{n:02}"), LeaseKind::Path);
            l.expires_ms = Some(NOW - 1);
            l
        })
        .collect();
    let i = ReconcileInput {
        leases,
        sprints: vec![sprint("s1", "Sprint 1", "active", NOW - 1, 0)],
        supervisors: vec![key("5a")],
        ..input()
    };
    let actions = plan(&i);
    assert_eq!(actions.len(), MAX_ACTIONS_PER_PASS);
    assert!(
        actions
            .iter()
            .all(|a| matches!(a, ReconcileAction::ExpireLease { .. }))
    );
    assert_eq!(actions.last(), Some(&expire("l63", NOW - 1)));
}

#[test]
fn plan_is_independent_of_input_order() {
    let mut a = lease("l1", LeaseKind::Path);
    a.expires_ms = Some(NOW - 1);
    let mut q = lease("q1", LeaseKind::Quiet);
    q.expected_end_ms = Some(NOW - 900_000);
    let base = ReconcileInput {
        claims: vec![],
        now_ms: NOW,
        settings: WorkSettings::default(),
        machine: Some(machine(GIB_KIB)),
        cards: vec![
            card("c1", "doing", Some("aa"), 1900),
            card("c2", "claimed", Some("bb"), 0),
            lane_card("c3", "other", "ops", 2700),
        ],
        leases: vec![a, lease("l2", LeaseKind::Pool), q],
        policies: vec![
            policy("*", "*", policy_of(1800.0)),
            policy("core", "dev", TeamPolicy::default()),
        ],
        require_review_default: false,
        marks: vec![mark("unrelated")],
        supervisors: vec![key("bb"), key("5a"), key("bb")],
        admission_paused: None,
        blocker_card: None,
        sprints: vec![
            sprint("s1", "Sprint 1", "active", NOW - 1, 2),
            sprint("s2", "Sprint 2", "active", NOW - 2, 0),
        ],
    };
    let mut reversed = base.clone();
    reversed.cards.reverse();
    reversed.leases.reverse();
    reversed.policies.reverse();
    reversed.supervisors.reverse();
    reversed.sprints.reverse();
    reversed.marks.reverse();
    let mut rotated = base.clone();
    rotated.cards.rotate_left(1);
    rotated.leases.rotate_left(1);
    let expected = plan(&base);
    assert!(!expected.is_empty());
    assert_eq!(plan(&reversed), expected);
    assert_eq!(plan(&rotated), expected);
}

#[test]
fn rules_emit_in_r1_to_r6_order() {
    let mut x = lease("x1", LeaseKind::Path);
    x.expires_ms = Some(NOW - 1);
    let mut q = lease("q1", LeaseKind::Quiet);
    q.expected_end_ms = Some(NOW - 900_000);
    let i = ReconcileInput {
        machine: Some(machine(GIB_KIB)),
        cards: vec![
            card("c1", "doing", Some("aa"), 1900),
            card("c2", "doing", Some("aa"), 0),
        ],
        leases: vec![q, x],
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                wip_limit: Some(1),
                ..TeamPolicy::default()
            },
        )],
        supervisors: vec![key("5a")],
        sprints: vec![sprint("s1", "Sprint 1", "active", NOW - 1, 0)],
        ..input()
    };
    let kinds: Vec<&str> = plan(&i).iter().map(ReconcileAction::op_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "reconcile.expire",
            "reconcile.quiet_close",
            "reconcile.notice",
            "reconcile.notice",
            "reconcile.admission",
            "reconcile.blocker",
            "reconcile.notice",
        ]
    );
}

/// Knuth's 64-bit linear congruential step; the high bits are the well-mixed ones. Deterministic, no host input.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// A pseudo-random input drawn from small pools, so that card, lease and sprint ids, supervisors and owners
/// repeat, floors breach or not, and the blocker and pause are sometimes open.
fn generated_input(state: &mut u64) -> ReconcileInput {
    let mut draw = |n: usize| usize::try_from(lcg(state) % n as u64).expect("a small draw");
    let mut cards = Vec::new();
    for _ in 0..draw(12) {
        let owner = ["aa", "bb", "cc"][draw(3)];
        let card_state = ["claimed", "doing", "review"][draw(3)];
        let idle_s = (draw(8) * 1200) as i64;
        cards.push(card(
            &format!("c{}", draw(4)),
            card_state,
            Some(owner),
            idle_s,
        ));
    }
    let mut leases = Vec::new();
    for _ in 0..draw(4) {
        let id = format!("l{}", draw(3));
        let mut l = match draw(3) {
            0 => lease(&id, LeaseKind::Path),
            1 => lease(&id, LeaseKind::Pool),
            _ => lease(&id, LeaseKind::Quiet),
        };
        l.expires_ms = Some(NOW - 1);
        l.expected_end_ms = Some(NOW - 900_000);
        leases.push(l);
    }
    let mut sprints = Vec::new();
    for _ in 0..draw(3) {
        let id = format!("s{}", draw(2));
        sprints.push(sprint(&id, "Sprint", "active", NOW - 1, 1));
    }
    let mut supervisors = Vec::new();
    for _ in 0..draw(4) {
        supervisors.push(key(["5a", "5b"][draw(2)]));
    }
    let machine = MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: Some(16 * GIB_KIB),
        disk_free_kib: Some(draw(8) as u64 * GIB_KIB),
        signals: Some(Signals {
            idle_pct: Some(draw(20) as f64),
            memory_used_kib: Some(draw(16) as u64 * GIB_KIB),
            ..Signals::default()
        }),
    };
    let settings = WorkSettings {
        memory_floor_kib: Some(2 * GIB_KIB),
        idle_floor_pct: Some(10.0),
        ..WorkSettings::default()
    };
    let marks = if draw(2) == 0 {
        vec![mark(&format!("expire:l1:{}", NOW - 1))]
    } else {
        Vec::new()
    };
    ReconcileInput {
        claims: vec![],
        now_ms: NOW,
        settings,
        machine: Some(machine),
        cards,
        leases,
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                stale_after_s: Some(600.0),
                escalate_after_s: Some(600.0),
                reclaim_after_s: Some(1800.0),
                reclaim: Some(true),
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
        )],
        require_review_default: false,
        marks,
        supervisors,
        admission_paused: (draw(2) == 0).then(|| "earlier reason".to_owned()),
        blocker_card: (draw(2) == 0).then(|| "blocker-1".to_owned()),
        sprints,
    }
}

#[test]
fn plan_keys_are_unique_across_fifty_generated_inputs() {
    let mut state = 0x5eed_u64;
    let mut emitted = 0;
    for _ in 0..50 {
        let actions = plan(&generated_input(&mut state));
        let keys: BTreeSet<&str> = actions.iter().map(ReconcileAction::key).collect();
        assert_eq!(keys.len(), actions.len(), "a key repeats in {actions:?}");
        emitted += actions.len();
    }
    assert!(emitted > 0, "the generated inputs emitted no actions");
}

#[test]
fn push_refuses_a_key_already_emitted_in_the_pass() {
    let i = input();
    let mut planner = Planner::new(&i);
    let expire_x = || ReconcileAction::ExpireLease {
        key: "expire:x".to_owned(),
        lease: "x".to_owned(),
    };
    planner.push(expire_x());
    planner.push(expire_x());
    assert_eq!(planner.out, vec![expire_x()]);
}
