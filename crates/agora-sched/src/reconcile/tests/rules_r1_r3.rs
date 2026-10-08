//! Planner tests for rules R1 to R3: leases, quiet windows, and stale, escalated and reclaimed cards.

use super::*;
#[test]
fn r1_expires_path_and_pool_leases_once_expiry_has_passed() {
    let mut path = lease("p1", LeaseKind::Path);
    path.expires_ms = Some(NOW);
    let mut pool = lease("q1", LeaseKind::Pool);
    pool.expires_ms = Some(NOW - 1);
    let i = ReconcileInput {
        leases: vec![pool, path],
        ..input()
    };
    assert_eq!(plan(&i), vec![expire("p1", NOW), expire("q1", NOW - 1)]);
}

#[test]
fn r1_key_carries_the_deadline_so_a_renewed_lease_is_expired_again_at_its_new_one() {
    let mut lease_l = lease("L", LeaseKind::Path);
    lease_l.expires_ms = Some(NOW - 1);
    let renewed_to = NOW + 60_000;
    let mut i = ReconcileInput {
        leases: vec![lease_l],
        ..input()
    };
    // The first pass expires L at T and the store marks the key it applied.
    assert_eq!(plan(&i), vec![expire("L", NOW - 1)]);
    i.marks.push(mark(&format!("expire:L:{}", NOW - 1)));
    assert!(plan(&i).is_empty(), "the marked key is not repeated");
    // L is renewed to a later deadline T2; the old mark stays. Before T2 nothing is due.
    i.leases[0].expires_ms = Some(renewed_to);
    assert!(
        plan(&i).is_empty(),
        "a renewed lease is live before its new deadline"
    );
    // At T2 the new key is not marked, so L is expired again.
    i.now_ms = renewed_to;
    assert_eq!(plan(&i), vec![expire("L", renewed_to)]);
}

#[test]
fn r1_skips_quiet_released_unexpiring_and_future_leases() {
    let mut quiet = lease("k1", LeaseKind::Quiet);
    quiet.expires_ms = Some(NOW - 1);
    let mut released = lease("r1", LeaseKind::Path);
    released.expires_ms = Some(NOW - 1);
    released.released_ms = Some(NOW - 5);
    let open = lease("o1", LeaseKind::Path);
    let mut future = lease("f1", LeaseKind::Pool);
    future.expires_ms = Some(NOW + 1);
    let i = ReconcileInput {
        leases: vec![quiet, released, open, future],
        ..input()
    };
    assert!(plan(&i).is_empty());
}

#[test]
fn r2_closes_quiet_windows_once_quiet_max_has_passed_since_expected_end() {
    let mut boundary = lease("q2", LeaseKind::Quiet);
    boundary.expected_end_ms = Some(NOW - 180_000);
    let mut overdue = lease("q1", LeaseKind::Quiet);
    overdue.expected_end_ms = Some(NOW - 200_000);
    let i = ReconcileInput {
        leases: vec![boundary, overdue],
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![
            ReconcileAction::CloseQuiet {
                key: format!("quiet:q1:{}", NOW - 200_000),
                lease: "q1".to_owned(),
            },
            ReconcileAction::CloseQuiet {
                key: format!("quiet:q2:{}", NOW - 180_000),
                lease: "q2".to_owned(),
            },
        ]
    );
}

#[test]
fn r2_keeps_windows_inside_quiet_max_and_skips_other_kinds_and_released() {
    let mut inside = lease("q1", LeaseKind::Quiet);
    inside.expected_end_ms = Some(NOW - 179_000);
    let mut path = lease("p1", LeaseKind::Path);
    path.expected_end_ms = Some(NOW - 900_000);
    let mut closed = lease("q2", LeaseKind::Quiet);
    closed.expected_end_ms = Some(NOW - 900_000);
    closed.released_ms = Some(NOW - 100_000);
    let i = ReconcileInput {
        leases: vec![inside, path, closed],
        ..input()
    };
    assert!(plan(&i).is_empty());
}

#[test]
fn r3_stale_card_notifies_owner_from_the_threshold_on_with_exact_text() {
    let i = ReconcileInput {
        cards: vec![
            card("c0", "doing", Some("abcdef12"), 1799),
            card("c1", "doing", Some("abcdef12"), 1800),
        ],
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![notice(
            &format!("stale1:c1:{}", NOW - 1_800_000),
            &key("abcdef12"),
            "stale",
            "c1",
            "card c1 (Title c1) has been doing for 30 min 0 s without activity; move it, add evidence, or release it",
        )]
    );
}

#[test]
fn r3_escalation_goes_to_the_chain_in_its_own_order_with_exact_text() {
    let chain = vec![key("c0ffee02"), key("c0ffee01")];
    let i = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                escalation_chain: Some(chain),
                ..TeamPolicy::default()
            },
        )],
        cards: vec![card("c1", "doing", Some("abcdef12"), 2700)],
        supervisors: vec![key("5a")],
        ..input()
    };
    let updated = NOW - 2_700_000;
    let escalation = |target: &str| {
        notice(
            &format!("stale2:c1:{updated}:{target}"),
            target,
            "escalation",
            "c1",
            "card c1 (Title c1) owned by abcdef12 has been doing for 45 min 0 s; it will be reclaimed after 1 h 0 min",
        )
    };
    assert_eq!(
        plan(&i),
        vec![
            notice(
                &format!("stale1:c1:{updated}"),
                &key("abcdef12"),
                "stale",
                "c1",
                "card c1 (Title c1) has been doing for 45 min 0 s without activity; move it, add evidence, or release it",
            ),
            escalation(&key("c0ffee02")),
            escalation(&key("c0ffee01")),
        ]
    );
}

#[test]
fn r3_escalation_without_a_chain_notifies_each_supervisor_once_in_sorted_order() {
    let i = ReconcileInput {
        cards: vec![card("c1", "claimed", Some("abcdef12"), 2700)],
        supervisors: vec![key("bb"), key("aa"), key("bb")],
        ..input()
    };
    let actions = plan(&i);
    let targets: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            ReconcileAction::Notice { kind, target, .. } if kind == "escalation" => {
                Some(target.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(targets, vec![key("aa").as_str(), key("bb").as_str()]);
    assert_eq!(actions.len(), 3, "one stale notice and two escalations");
}

#[test]
fn r3_escalation_with_no_chain_and_no_supervisors_sends_nothing_extra() {
    let i = ReconcileInput {
        cards: vec![card("c1", "doing", Some("abcdef12"), 2700)],
        ..input()
    };
    let actions = plan(&i);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].op_kind(), "reconcile.notice");
}

#[test]
fn r3_reclaim_emits_reclaim_and_owner_notice_at_the_reclaim_threshold() {
    let i = ReconcileInput {
        cards: vec![card("c1", "doing", Some("abcdef12"), 3600)],
        supervisors: vec![key("5a")],
        ..input()
    };
    let updated = NOW - 3_600_000;
    let owner = key("abcdef12");
    assert_eq!(
        plan(&i),
        vec![
            notice(
                &format!("stale1:c1:{updated}"),
                &owner,
                "stale",
                "c1",
                "card c1 (Title c1) has been doing for 1 h 0 min without activity; move it, add evidence, or release it",
            ),
            notice(
                &format!("stale2:c1:{updated}:{}", key("5a")),
                &key("5a"),
                "escalation",
                "c1",
                "card c1 (Title c1) owned by abcdef12 has been doing for 1 h 0 min; it will be reclaimed after 1 h 0 min",
            ),
            notice(
                &format!("stale3:c1:{updated}"),
                &owner,
                "reclaimed",
                "c1",
                "card c1 (Title c1) was reclaimed after 1 h 0 min without activity; it is ready again and no longer yours",
            ),
            ReconcileAction::ReclaimCard {
                key: format!("reclaim:c1:{updated}"),
                card: "c1".to_owned(),
                owner: owner.clone(),
            },
        ]
    );
}

#[test]
fn r3_the_owner_is_told_of_the_reclaim_before_the_card_is_reclaimed() {
    let i = ReconcileInput {
        cards: vec![card("c1", "doing", Some("abcdef12"), 3600)],
        ..input()
    };
    let actions = plan(&i);
    let notice_at = actions
        .iter()
        .position(|a| matches!(a, ReconcileAction::Notice { kind, .. } if kind == "reclaimed"));
    let reclaim_at = actions
        .iter()
        .position(|a| matches!(a, ReconcileAction::ReclaimCard { .. }));
    assert!(
        notice_at.is_some() && notice_at < reclaim_at,
        "the reclaimed notice must come before the reclaim, got {actions:?}"
    );
}

#[test]
fn r3_with_reclaim_off_the_escalation_says_so_and_nothing_is_reclaimed() {
    let i = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                reclaim: Some(false),
                ..TeamPolicy::default()
            },
        )],
        cards: vec![card("c1", "doing", Some("abcdef12"), 3600)],
        supervisors: vec![key("5a")],
        ..input()
    };
    let actions = plan(&i);
    assert_eq!(actions.len(), 2);
    assert!(
        matches!(&actions[1], ReconcileAction::Notice { text, .. }
            if text.ends_with("; reclaim is off for this lane")),
        "{:?}",
        actions[1]
    );
}

#[test]
fn r3_ignores_other_states_and_cards_without_an_owner() {
    let i = ReconcileInput {
        cards: vec![
            card("c1", "ready", Some("abcdef12"), 9999),
            card("c2", "review", Some("abcdef12"), 9999),
            card("c3", "done", Some("abcdef12"), 9999),
            card("c4", "claimed", None, 9999),
        ],
        supervisors: vec![key("5a")],
        ..input()
    };
    assert!(plan(&i).is_empty());
}

#[test]
fn r3_thresholds_come_from_the_most_specific_policy_layer() {
    let i = ReconcileInput {
        policies: vec![
            policy("*", "*", policy_of(100.0)),
            policy("core", "*", policy_of(200.0)),
            policy("*", "dev", policy_of(300.0)),
            policy("core", "dev", policy_of(400.0)),
        ],
        cards: vec![
            lane_card("a", "other", "ops", 350),
            lane_card("b", "core", "ops", 350),
            lane_card("c", "other", "dev", 350),
            lane_card("d", "core", "dev", 350),
        ],
        ..input()
    };
    let subjects: Vec<String> = plan(&i)
        .into_iter()
        .filter_map(|a| match a {
            ReconcileAction::Notice { subject, kind, .. } if kind == "stale" => Some(subject),
            _ => None,
        })
        .collect();
    assert_eq!(subjects, vec!["a", "b", "c"]);
}
