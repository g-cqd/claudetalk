use super::*;

// ---- Pass cap ---------------------------------------------------------------------------------------------

#[test]
fn a_pass_stops_at_the_action_cap_in_card_id_order_with_100_stale_cards() {
    let cards: Vec<CardSnap> = (0..100)
        .map(|n| card(&format!("c{n:03}"), "doing", Some("aa"), at(-40)))
        .collect();
    let actions = plan(&ReconcileInput { cards, ..input() });
    let subjects: Vec<String> = notices(&actions)
        .into_iter()
        .map(|(_, _, subject, _)| subject)
        .collect();
    let first_64: Vec<String> = (0..MAX_ACTIONS_PER_PASS)
        .map(|n| format!("c{n:03}"))
        .collect();
    assert_eq!(actions.len(), MAX_ACTIONS_PER_PASS);
    assert_eq!(subjects, first_64);
}

#[test]
fn cards_past_the_cap_are_noticed_on_the_next_pass() {
    let cards: Vec<CardSnap> = (0..100)
        .map(|n| card(&format!("c{n:03}"), "doing", Some("aa"), at(-40)))
        .collect();
    let mut store = Store::new(ReconcileInput { cards, ..input() });
    assert_eq!(store.pass(at(0)).len(), MAX_ACTIONS_PER_PASS);
    let rest: Vec<String> = notices(&store.pass(at(0)))
        .into_iter()
        .map(|(_, _, subject, _)| subject)
        .collect();
    let expected: Vec<String> = (MAX_ACTIONS_PER_PASS..100)
        .map(|n| format!("c{n:03}"))
        .collect();
    assert_eq!(rest, expected);
}

// ---- Input order and duplicate policy rows ----------------------------------------------------------------

#[test]
fn reversing_or_rotating_every_input_vector_gives_the_same_plan() {
    let mut path = lease("p1", LeaseKind::Path);
    path.expires_ms = Some(NOW - 1);
    let mut pool = lease("p2", LeaseKind::Pool);
    pool.expires_ms = Some(NOW - 1);
    let mut quiet = lease("q1", LeaseKind::Quiet);
    quiet.expected_end_ms = Some(NOW - 200_000);
    let base = ReconcileInput {
        claims: vec![],
        now_ms: at(70),
        settings: WorkSettings::default(),
        machine: Some(disk(DISK_BREACH_KIB)),
        cards: vec![
            card("c1", "doing", Some("aa"), NOW),
            card("c2", "claimed", Some("bb"), at(35)),
            card_in("c3", "ops", "dev", "doing", Some("cc"), at(65)),
            card_in("c4", "core", "dev", "review", None, at(70)),
            card("c5", "doing", Some("dd"), at(70)),
            card("c6", "claimed", Some("ee"), at(70)),
        ],
        leases: vec![pool, path, quiet],
        policies: vec![
            policy(
                "*",
                "*",
                TeamPolicy {
                    wip_limit: Some(2),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "dev",
                TeamPolicy {
                    escalation_chain: Some(vec![pk("cc"), pk("bb")]),
                    ..TeamPolicy::default()
                },
                2,
            ),
        ],
        require_review_default: true,
        marks: vec![
            Mark {
                key: format!("expire:p2:{}", NOW - 1),
                last_ms: NOW,
                count: 1,
            },
            Mark {
                key: "unrelated".to_owned(),
                last_ms: NOW,
                count: 1,
            },
        ],
        supervisors: vec![pk("bb"), pk("5a"), pk("bb")],
        admission_paused: None,
        blocker_card: None,
        sprints: vec![
            sprint("s1", "Sprint 1", "active", NOW, 3),
            sprint("s2", "Sprint 2", "active", at(-5), 0),
        ],
    };
    let expected = plan(&base);
    let kinds: BTreeSet<&str> = expected.iter().map(ReconcileAction::op_kind).collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            "reconcile.expire",
            "reconcile.quiet_close",
            "reconcile.notice",
            "reconcile.reclaim",
            "reconcile.blocker",
            "reconcile.admission",
        ]),
        "the scenario exercises every rule"
    );

    let mut reversed = base.clone();
    reversed.cards.reverse();
    reversed.leases.reverse();
    reversed.policies.reverse();
    reversed.supervisors.reverse();
    reversed.sprints.reverse();
    reversed.marks.reverse();
    assert_eq!(plan(&reversed), expected);

    let mut rotated = base.clone();
    rotated.cards.rotate_left(2);
    rotated.leases.rotate_left(1);
    assert_eq!(plan(&rotated), expected);
}

#[test]
fn the_most_recently_updated_duplicate_policy_row_wins_in_either_input_order() {
    let mut i = ReconcileInput {
        cards: vec![
            card("c1", "claimed", Some("aa"), NOW),
            card("c2", "doing", Some("bb"), NOW),
        ],
        policies: vec![
            policy(
                "core",
                "dev",
                TeamPolicy {
                    wip_limit: Some(1),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "dev",
                TeamPolicy {
                    wip_limit: Some(3),
                    ..TeamPolicy::default()
                },
                2,
            ),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let forward = plan(&i);
    i.policies.reverse();
    assert_eq!(plan(&i), forward);
    assert!(
        forward.is_empty(),
        "the newer row sets the limit to 3, and two cards are under it"
    );
}

// ---- Keys (SPEC section 6) ------------------------------------------------------------------------------

#[test]
fn action_keys_follow_the_section_6_formats() {
    let mut path = lease("p1", LeaseKind::Path);
    path.expires_ms = Some(NOW - 1);
    let mut quiet = lease("q1", LeaseKind::Quiet);
    quiet.expected_end_ms = Some(NOW - 200_000);
    let i = ReconcileInput {
        now_ms: at(70),
        machine: Some(disk(DISK_BREACH_KIB)),
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        leases: vec![path, quiet],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let keys: Vec<String> = plan(&i).iter().map(|a| a.key().to_owned()).collect();
    assert_eq!(
        keys,
        vec![
            format!("expire:p1:{}", NOW - 1),
            format!("quiet:q1:{}", NOW - 200_000),
            format!("stale1:c1:{NOW}"),
            format!("stale2:c1:{NOW}:{}", pk("5a")),
            format!("stale3:c1:{NOW}"),
            format!("reclaim:c1:{NOW}"),
            format!("pause:{}", BUCKET_10_MIN + 7),
            format!("blocker:{}", BUCKET_10_MIN + 7),
        ]
    );
}

// ---- Blocker, WIP and sprint keys (M5 decisions O-1 and O-2) --------------------------------------------------

#[test]
fn a_stale_blocker_card_is_closed_even_when_admission_was_never_paused() {
    let i = ReconcileInput {
        machine: Some(disk(HEALTHY_KIB)),
        blocker_card: Some("blocker-7".to_owned()),
        ..input()
    };
    let actions = plan(&i);
    assert!(
        matches!(
            actions.as_slice(),
            [ReconcileAction::Blocker { key, open: false, .. }] if key == "unblock:blocker-7"
        ),
        "got {actions:?}"
    );
}

#[test]
fn a_wip_notice_to_one_supervisor_does_not_suppress_one_added_later() {
    let mut store = Store::new(ReconcileInput {
        cards: three_in_progress("a"),
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(notices(&store.pass(at(0))).len(), 1);
    store.input.supervisors = vec![pk("5a"), pk("5b")];
    assert_eq!(
        notices(&store.pass(at(0))),
        vec![view(pk("5b"), "wip", "core/dev", WIP_TEXT)]
    );
}

#[test]
fn a_sprint_notice_to_one_supervisor_does_not_suppress_one_added_later() {
    let mut store = Store::new(ReconcileInput {
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(store.pass(at(0)).len(), 1);
    store.input.supervisors = vec![pk("5a"), pk("5b")];
    assert_eq!(
        notices(&store.pass(at(0))),
        vec![view(pk("5b"), "sprint_end", "s1", SPRINT_TEXT)]
    );
}

#[test]
fn wip_and_sprint_notice_keys_name_their_target() {
    let i = ReconcileInput {
        cards: three_in_progress("a"),
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
            1,
        )],
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let keys: Vec<String> = plan(&i).iter().map(|a| a.key().to_owned()).collect();
    assert_eq!(
        keys,
        vec![
            format!("wip:core/dev:3:{BUCKET_HOUR}:{}", pk("5a")),
            format!("sprint:s1:{}", pk("5a")),
        ]
    );
}
