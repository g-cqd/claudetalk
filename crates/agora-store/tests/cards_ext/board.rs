use super::*;

#[test]
fn wip_limit_refuses_a_claim_that_would_pass_the_lane_limit() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 1}));
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c2", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    move_card(&mut s, &c.sup, "c2", "ready");
    assert_eq!(
        failure(s.transition(NOW, &op(&c.bob, "card.move", "c2", "claimed"), false)),
        "wip limit reached: lane core/default has 1 of 1"
    );
    assert_eq!(s.card_state("c2").expect("state"), CardState::Ready);
}

#[test]
fn a_card_already_claimed_in_an_over_limit_lane_can_still_move_to_doing() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 2}));
    for id in ["c1", "c2", "c3"] {
        new_card(&mut s, &c.sup, id, json!({"team": "core"}));
        move_card(&mut s, &c.sup, id, "ready");
    }
    move_card(&mut s, &c.alice, "c1", "claimed");
    move_card(&mut s, &c.bob, "c2", "claimed");
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 1}));
    assert_eq!(
        failure(s.transition(NOW, &op(&c.carol, "card.move", "c3", "claimed"), false)),
        "wip limit reached: lane core/default has 2 of 1"
    );
    move_card(&mut s, &c.alice, "c1", "doing");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}

// ---- board -----------------------------------------------------------------------------------------------------

#[test]
fn board_lists_the_cards_that_need_an_owner_with_their_action() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    escalate(&mut s, &c.alice, "c1", "decide scope", &[]).expect("escalate");
    assert_eq!(
        s.board_extras(NOW).expect("board").needs_owner,
        vec![NeedsOwner {
            card: "c1".into(),
            title: TITLE.into(),
            owner_action: "decide scope".into(),
        }]
    );
}

#[test]
fn board_reports_wip_per_lane_with_its_limit_and_shows_a_limited_idle_lane() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 2}));
    set_policy(&mut s, &c.sup, "core", "ops", json!({"wip_limit": 3}));
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c2", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c3", json!({"team": "core", "lane": "ops"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    claim_card(&mut s, &c.sup, &c.bob, "c2");
    assert_eq!(
        s.board_extras(NOW).expect("board").wip,
        vec![
            WipView {
                team: "core".into(),
                lane: "default".into(),
                count: 2,
                limit: Some(2),
            },
            WipView {
                team: "core".into(),
                lane: "ops".into(),
                count: 0,
                limit: Some(3),
            },
        ]
    );
}

#[test]
fn board_counts_queued_waiting_and_running_jobs() {
    let (mut s, c) = in_memory();
    queue_task(&mut s, &c.alice, "q1", None);
    queue_task(&mut s, &c.alice, "w1", None);
    claim_accept_run(&mut s, &c.bob, "w1");
    queue_task(&mut s, &c.alice, "r1", None);
    start_as_carol(&mut s, &c, "r1");
    let board = s.board_extras(NOW).expect("board");
    assert_eq!(
        (board.queued_tasks, board.waiting_jobs, board.running_jobs),
        (1, 1, 1)
    );
}

#[test]
fn board_counts_open_leases_and_drops_one_at_its_expiry() {
    let (mut s, c) = in_memory();
    let arg = json!({"kind": "path", "resource": "/proj/docs", "ttl_s": 60.0}).to_string();
    s.lease_acquire(NOW, &op(&c.alice, "lease.acquire", "", &arg))
        .expect("setup: lease");
    assert_eq!(s.board_extras(NOW).expect("board").open_leases, 1);
    assert_eq!(s.board_extras(NOW + 60_000).expect("board").open_leases, 0);
}

#[test]
fn board_shows_the_open_manual_quiet_window_and_its_seconds_left() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    let arg = json!({"expected_end_ms": NOW + 90_000, "note": ""}).to_string();
    let lease = s
        .quiet_open(NOW, &op(&ctl, "quiet.open", "", &arg))
        .expect("quiet open");
    assert_eq!(
        s.board_extras(NOW).expect("board").quiet,
        Some(QuietView {
            state: "manual".into(),
            job: None,
            lease: Some(lease.id.clone()),
            seconds_left: 90.0,
            text: format!("manual quiet window {}", lease.id),
        })
    );
}

#[test]
fn board_shows_the_admission_pause_until_it_is_cleared() {
    let (mut s, c) = in_memory();
    let pause =
        json!({"action": "admission", "key": "admission:disk", "paused": "disk low"}).to_string();
    assert!(
        s.apply_reconcile(NOW, &op(&c.sys, "reconcile.admission", "", &pause))
            .expect("pause")
    );
    assert_eq!(
        s.board_extras(NOW).expect("board").paused,
        Some("disk low".to_string())
    );
    let clear =
        json!({"action": "admission", "key": "admission:clear", "paused": null}).to_string();
    s.apply_reconcile(NOW, &op(&c.sys, "reconcile.admission", "", &clear))
        .expect("clear");
    assert_eq!(s.board_extras(NOW).expect("board").paused, None);
}

// ---- job sync (D7) ---------------------------------------------------------------------------------------------
