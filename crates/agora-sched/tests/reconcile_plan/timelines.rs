use super::*;

// ---- R3: one card over a multi-hour timeline --------------------------------------------------------------

#[test]
fn idle_card_gets_owner_notice_then_supervisor_escalation_then_reclaim_over_three_hours() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[0, 10, 20, 30, 40, 50, 70, 90, 120, 180]);
    assert_eq!(
        fired,
        vec![
            (30, vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "5a", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c1", "aa", NOW, "5b", "50 min 0 s", CLOSE_TO_RECLAIM),
                ],
            ),
            (70, reclaim_actions("c1", "aa", NOW, "1 h 10 min")),
        ]
    );
}

#[test]
fn an_update_restarts_the_stale_cycle_under_keys_that_name_the_new_timestamp() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(
        store.pass(at(30)),
        vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]
    );
    store.touch_cards(at(35));
    assert!(
        store.pass(at(60)).is_empty(),
        "25 minutes after the update the card is not stale yet"
    );
    assert_eq!(
        store.pass(at(65)),
        vec![stale_notice("c1", "aa", at(35), "30 min 0 s")]
    );
}

#[test]
fn escalation_goes_to_the_team_chain_and_other_lanes_fall_back_to_supervisors() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![
            card("c1", "doing", Some("aa"), NOW),
            card_in("c2", "ops", "dev", "doing", Some("dd"), NOW),
        ],
        policies: vec![policy(
            "core",
            "*",
            TeamPolicy {
                escalation_chain: Some(vec![pk("cc"), pk("bb")]),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[30, 50]);
    assert_eq!(
        fired,
        vec![
            (
                30,
                vec![
                    stale_notice("c1", "aa", NOW, "30 min 0 s"),
                    stale_notice("c2", "dd", NOW, "30 min 0 s"),
                ],
            ),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "cc", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c1", "aa", NOW, "bb", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c2", "dd", NOW, "5a", "50 min 0 s", CLOSE_TO_RECLAIM),
                ],
            ),
        ]
    );
}

#[test]
fn reclaim_off_for_the_lane_escalates_with_that_text_and_never_reclaims() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                reclaim: Some(false),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[30, 50, 70, 180]);
    assert_eq!(
        fired,
        vec![
            (30, vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "5a", "50 min 0 s", RECLAIM_OFF),
                    escalation_notice("c1", "aa", NOW, "5b", "50 min 0 s", RECLAIM_OFF),
                ],
            ),
        ]
    );
}

#[test]
fn stale_threshold_resolves_per_lane_through_the_team_and_lane_layers() {
    let idle_10_min = at(-10);
    let i = ReconcileInput {
        cards: vec![
            card_in("c1", "core", "dev", "doing", Some("aa"), idle_10_min),
            card_in("c2", "core", "review", "doing", Some("bb"), idle_10_min),
            card_in("c3", "ops", "dev", "doing", Some("cc"), idle_10_min),
            card_in("c4", "ops", "review", "doing", Some("dd"), idle_10_min),
        ],
        policies: vec![
            policy(
                "core",
                "*",
                TeamPolicy {
                    stale_after_s: Some(300.0),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "*",
                "review",
                TeamPolicy {
                    stale_after_s: Some(1200.0),
                    ..TeamPolicy::default()
                },
                1,
            ),
        ],
        ..input()
    };
    // core/dev takes the team threshold of 5 min; core/review takes the lane threshold of 20 min over the team's;
    // ops/dev keeps the 30 min default; ops/review takes the lane threshold of 20 min.
    assert_eq!(
        notices(&plan(&i)),
        vec![view(
            pk("aa"),
            "stale",
            "c1",
            "card c1 (Title c1) has been doing for 10 min 0 s without activity; move it, add evidence, or release it",
        )]
    );
}

// ---- R1 and R2: leases and quiet windows at their instants ----------------------------------------------

#[test]
fn path_lease_expires_on_the_pass_whose_time_equals_expires_ms() {
    let mut expiring = lease("p1", LeaseKind::Path);
    expiring.expires_ms = Some(at(15));
    let mut store = Store::new(ReconcileInput {
        leases: vec![expiring],
        ..input()
    });
    assert!(
        store.pass(at(15) - 1).is_empty(),
        "one millisecond before expires_ms the lease is live"
    );
    assert_eq!(
        store.pass(at(15)),
        vec![ReconcileAction::ExpireLease {
            key: format!("expire:p1:{}", at(15)),
            lease: "p1".to_owned(),
        }]
    );
}

#[test]
fn quiet_window_closes_once_quiet_max_has_passed_since_its_expected_end() {
    let mut window = lease("q1", LeaseKind::Quiet);
    window.expected_end_ms = Some(NOW);
    let quiet_max_ms = WorkSettings::default().quiet_max as i64 * 1000;
    let mut store = Store::new(ReconcileInput {
        leases: vec![window],
        ..input()
    });
    assert!(store.pass(NOW + quiet_max_ms - 1).is_empty());
    assert_eq!(
        store.pass(NOW + quiet_max_ms),
        vec![ReconcileAction::CloseQuiet {
            key: format!("quiet:q1:{NOW}"),
            lease: "q1".to_owned(),
        }]
    );
}
