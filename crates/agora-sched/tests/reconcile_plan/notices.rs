use super::*;

// ---- R4: WIP limits by lane, through the team and lane layers -------------------------------------------

#[test]
fn wip_limit_resolves_per_lane_through_the_team_and_global_layers() {
    let mut cards = three_in_progress("a");
    cards.push(card_in("b1", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b2", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("c1", "ops", "dev", "doing", Some("aa"), NOW));
    cards.push(card_in("c2", "ops", "dev", "doing", Some("aa"), NOW));
    let i = ReconcileInput {
        cards,
        policies: vec![
            policy(
                "*",
                "*",
                TeamPolicy {
                    wip_limit: Some(1),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "*",
                TeamPolicy {
                    wip_limit: Some(2),
                    ..TeamPolicy::default()
                },
                1,
            ),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    // core/dev has 3 against the team's 2; core/review has 2, at the team's limit; ops/dev has 2 against the
    // global 1, because no team layer covers ops.
    assert_eq!(
        notices(&plan(&i)),
        vec![
            view(pk("5a"), "wip", "core/dev", WIP_TEXT),
            view(
                pk("5a"),
                "wip",
                "ops/dev",
                "lane ops/dev has 2 cards in progress, above its WIP limit of 1",
            ),
        ]
    );
}

#[test]
fn lane_layer_overrides_the_team_layer_for_the_wip_limit() {
    let mut cards = three_in_progress("a");
    cards.push(card_in("b1", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b2", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b3", "core", "review", "claimed", Some("aa"), NOW));
    let lane_limit = |limit: u32| TeamPolicy {
        wip_limit: Some(limit),
        ..TeamPolicy::default()
    };
    let i = ReconcileInput {
        cards,
        policies: vec![
            policy("*", "*", lane_limit(1), 1),
            policy("core", "*", lane_limit(2), 1),
            policy("*", "dev", lane_limit(4), 1),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    // core/dev: the lane layer (4) beats the team layer (2), so its 3 cards are under the limit.
    // core/review: no lane layer, so the team layer (2) applies to its 3 cards.
    assert_eq!(
        notices(&plan(&i)),
        vec![view(
            pk("5a"),
            "wip",
            "core/review",
            "lane core/review has 3 cards in progress, above its WIP limit of 2",
        )]
    );
}

#[test]
fn wip_notice_is_sent_once_per_clock_hour() {
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
    let first = store.pass(at(0));
    assert_eq!(notices(&first).len(), 1);
    store.touch_cards(at(59));
    assert!(store.pass(at(59)).is_empty(), "same hour bucket");
    store.touch_cards(at(60));
    let next_hour = store.pass(at(60));
    assert_eq!(notices(&next_hour), notices(&first));
    assert_eq!(
        next_hour.len(),
        1,
        "the next hour bucket re-sends the notice"
    );
}

// ---- R6: sprints past their end -------------------------------------------------------------------------

#[test]
fn ended_active_sprints_notify_each_supervisor_in_sorted_order() {
    let i = ReconcileInput {
        sprints: vec![
            sprint("s1", "Sprint 1", "active", at(-10), 2),
            sprint("s2", "Sprint 2", "closed", at(-10), 0),
            sprint("s3", "Sprint 3", "active", at(1), 5),
        ],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    };
    assert_eq!(
        notices(&plan(&i)),
        vec![
            view(pk("5a"), "sprint_end", "s1", SPRINT_TEXT),
            view(pk("5b"), "sprint_end", "s1", SPRINT_TEXT),
        ]
    );
}

#[test]
fn an_ended_sprint_is_not_announced_again_once_its_notices_are_applied() {
    let mut store = Store::new(ReconcileInput {
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(store.pass(at(0)).len(), 1);
    assert!(store.pass(at(5)).is_empty());
}
