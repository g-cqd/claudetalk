//! Unit tests for the WIP gate and the board summary.

use super::*;

/// Lane `dev/default` gets a WIP limit of 1 from a policy row.
fn set_dev_wip_limit(c: &Cast, limit: u32) {
    let json = serde_json::json!({"wip_limit": limit}).to_string();
    c.s.conn
        .execute(
            "INSERT INTO policies(team, lane, json, updated, by) VALUES ('dev','*',?,?,?)",
            params![json, NOW, "owner"],
        )
        .unwrap();
}

#[test]
fn wip_limit_refuses_a_claim_past_the_limit() {
    let mut c = cast();
    for id in ["a", "b", "c"] {
        make_in(&mut c, id, "dev");
        mv(&mut c.s, &c.sup, id, "ready").unwrap();
    }
    // No policy: two claims in the lane are fine.
    mv(&mut c.s, &c.alice, "a", "claimed").unwrap();
    mv(&mut c.s, &c.alice, "b", "claimed").unwrap();
    set_dev_wip_limit(&c, 1);
    let err = mv(&mut c.s, &c.alice, "c", "claimed").unwrap_err();
    assert!(
        matches!(&err, StoreError::Refused(m)
        if m == "wip limit reached: lane dev/default has 2 of 1"),
        "{err:?}"
    );
    assert_eq!(state_of(&c, "c"), "ready");
    // A card already in progress may move claimed -> doing in an over-limit lane.
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    assert_eq!(state_of(&c, "a"), "doing");
}

#[test]
fn wip_gate_refuses_entering_claimed_from_ready_in_an_over_limit_lane() {
    let mut c = cast();
    for id in ["a", "b"] {
        make_in(&mut c, id, "dev");
        mv(&mut c.s, &c.sup, id, "ready").unwrap();
    }
    mv(&mut c.s, &c.alice, "a", "claimed").unwrap();
    set_dev_wip_limit(&c, 1);
    let err = mv(&mut c.s, &c.alice, "b", "claimed").unwrap_err();
    assert!(
        matches!(&err, StoreError::Refused(m)
        if m == "wip limit reached: lane dev/default has 1 of 1"),
        "{err:?}"
    );
    assert_eq!(state_of(&c, "b"), "ready");
    // Claimed -> doing for a is allowed: a is already in progress, so the gate does not apply.
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    assert_eq!(state_of(&c, "a"), "doing");
}

#[test]
fn wip_gate_applies_when_review_card_re_enters_work() {
    let mut c = cast();
    for id in ["a", "b"] {
        make_in(&mut c, id, "dev");
        mv(&mut c.s, &c.sup, id, "ready").unwrap();
    }
    mv(&mut c.s, &c.alice, "a", "claimed").unwrap();
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    mv(&mut c.s, &c.alice, "a", "review").unwrap();
    mv(&mut c.s, &c.alice, "b", "claimed").unwrap();
    set_dev_wip_limit(&c, 1);
    // Review is outside claimed and doing, so a re-entering doing is gated by b.
    let err = mv(&mut c.s, &c.alice, "a", "doing").unwrap_err();
    assert!(
        matches!(&err, StoreError::Refused(m)
        if m == "wip limit reached: lane dev/default has 1 of 1"),
        "{err:?}"
    );
    assert_eq!(state_of(&c, "a"), "review");
}

#[test]
fn wip_gate_does_not_count_the_card_itself() {
    let mut c = cast();
    make_in(&mut c, "a", "dev");
    make_in(&mut c, "b", "dev");
    set_dev_wip_limit(&c, 1);
    mv(&mut c.s, &c.sup, "a", "ready").unwrap();
    mv(&mut c.s, &c.sup, "b", "ready").unwrap();
    mv(&mut c.s, &c.alice, "a", "claimed").unwrap();
    // a moves on to doing: it is excluded from its own count, so the move passes.
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    let err = mv(&mut c.s, &c.alice, "b", "claimed").unwrap_err();
    assert!(matches!(err, StoreError::Refused(_)));
    // Review is not counted: once a leaves doing, b can be claimed.
    mv(&mut c.s, &c.alice, "a", "review").unwrap();
    mv(&mut c.s, &c.alice, "b", "claimed").unwrap();
}

#[test]
fn board_extras_summarise_the_board() {
    let mut c = cast();
    make_in(&mut c, "a", "dev");
    let esc = r#"{"owner_action":"need help","to":[]}"#;
    c.s.card_escalate(NOW, &sign(&c.sup, "card.escalate", "a", esc, NOW), false)
        .unwrap();
    make_in(&mut c, "b", "dev");
    mv(&mut c.s, &c.sup, "b", "ready").unwrap();
    mv(&mut c.s, &c.alice, "b", "claimed").unwrap();
    set_dev_wip_limit(&c, 3);
    add_task(&c, "q1", None, "queued");
    add_task(&c, "r1", None, "running");
    c.s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state, requested) VALUES ('w1','x',0,'claimed',?)",
            [NOW],
        )
        .unwrap();
    c.s.conn
        .execute_batch(&format!(
            "INSERT INTO leases(id, kind, resource, holder, created, heartbeat, expected_end) \
             VALUES ('quiet-1','quiet','','x',{NOW},{NOW},{end}); \
             INSERT INTO leases(id, kind, resource, holder, created, heartbeat, expires) \
             VALUES ('p1','path','/x','x',{NOW},{NOW},{later}); \
             INSERT INTO leases(id, kind, resource, holder, created, heartbeat, expires) \
             VALUES ('p2','path','/y','x',{NOW},{NOW},{past}); \
             INSERT INTO work_state(key, value) VALUES ('admission_paused','disk low');",
            end = NOW + 60_000,
            later = NOW + 1_000,
            past = NOW - 1,
        ))
        .unwrap();

    let x = c.s.board_extras(NOW).unwrap();
    assert_eq!(x.needs_owner.len(), 1);
    assert_eq!(x.needs_owner[0].card, "a");
    assert_eq!(x.needs_owner[0].owner_action, "need help");
    assert_eq!(x.wip.len(), 1);
    assert_eq!(
        (x.wip[0].team.as_str(), x.wip[0].lane.as_str()),
        ("dev", "default")
    );
    assert_eq!((x.wip[0].count, x.wip[0].limit), (1, Some(3)));
    assert_eq!((x.running_jobs, x.waiting_jobs, x.queued_tasks), (1, 1, 1));
    assert_eq!(x.open_leases, 2);
    let quiet = x.quiet.expect("open quiet lease");
    assert_eq!(
        (quiet.state.as_str(), quiet.lease.as_deref()),
        ("manual", Some("quiet-1"))
    );
    assert_eq!(quiet.seconds_left, 60.0);
    assert_eq!(x.paused.as_deref(), Some("disk low"));
}
