//! Unit tests for card extensions: field validation, edit rights, dependency cycles, links,
//! escalation targets, the WIP gate and the board summary.

use super::testkit::*;
use super::*;
use crate::hex;
use rusqlite::params;

#[test]
fn create_card_stores_new_fields_and_deps() {
    let mut c = cast();
    make(&mut c, "a");
    let arg = serde_json::json!({"title": "b", "priority": 5, "value": 2.5, "size": 3,
        "team": "dev", "after": ["a"]})
    .to_string();
    c.s.create_card(NOW, &sign(&c.sup, "card.create", "b", &arg, NOW))
        .unwrap();
    let b = ext(&c, "b");
    assert_eq!((b.priority, b.value, b.size), (5, 2.5, 3.0));
    assert_eq!(b.team, "dev");
    assert_eq!(b.after, ["a"]);
    assert!(ext(&c, "a").after.is_empty());
}

#[test]
fn create_card_defaults_when_fields_are_absent() {
    let mut c = cast();
    make(&mut c, "a");
    let a = ext(&c, "a");
    assert_eq!((a.priority, a.value, a.size), (0, 1.0, 1.0));
    assert!(a.team.is_empty() && a.after.is_empty() && !a.needs_owner);
}

#[test]
fn create_card_rejects_bad_new_fields_and_stores_nothing() {
    let mut c = cast();
    make(&mut c, "a");
    let long_team = "t".repeat(65);
    let bad = [
        (
            r#"{"title":"x","value":0}"#,
            "card value must be a positive number",
        ),
        (
            r#"{"title":"x","size":-1}"#,
            "card size must be a positive number",
        ),
        (
            r#"{"title":"x","priority":"high"}"#,
            "card priority must be an integer",
        ),
        (r#"{"title":"x","team":5}"#, "card team must be a string"),
    ];
    for (arg, msg) in bad {
        let op = sign(&c.sup, "card.create", "x", arg, NOW);
        let err = c.s.create_card(NOW, &op).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(m) if m == msg), "{arg}");
    }
    let arg = serde_json::json!({"title": "x", "team": long_team}).to_string();
    let err =
        c.s.create_card(NOW, &sign(&c.sup, "card.create", "x", &arg, NOW));
    assert!(matches!(err, Err(StoreError::Invalid(_))));
    let arg = r#"{"title":"x","after":["ghost"]}"#;
    let err =
        c.s.create_card(NOW, &sign(&c.sup, "card.create", "x", arg, NOW));
    assert!(matches!(err, Err(StoreError::NotFound(ref g)) if g == "ghost"));
    let arg = r#"{"title":"x","after":["x"]}"#;
    let err =
        c.s.create_card(NOW, &sign(&c.sup, "card.create", "x", arg, NOW));
    assert!(matches!(err, Err(StoreError::Invalid(_))));
    // Nothing was stored: the id is still free.
    assert!(matches!(
        c.s.card_ext(&["x".into()]),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn card_update_follows_owner_and_supervisor_rights() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let arg = r#"{"value":4.0}"#;
    let bob = sign(&c.bob, "card.update", "a", arg, NOW);
    assert!(is_forbidden(
        &c.s.card_update(NOW, &bob).unwrap_err(),
        Role::Worker
    ));
    let alice = sign(&c.alice, "card.update", "a", arg, NOW);
    assert_eq!(c.s.card_update(NOW, &alice).unwrap().value, 4.0);
    let sup = sign(&c.sup, "card.update", "a", r#"{"priority":7}"#, NOW);
    assert_eq!(c.s.card_update(NOW, &sup).unwrap().priority, 7);
    let bob = sign(&c.bob, "card.update", "a", r#"{"owner_action":"x"}"#, NOW);
    assert!(c.s.card_update(NOW, &bob).is_err());
}

#[test]
fn card_update_by_a_worker_on_an_unowned_card_and_bad_values() {
    let mut c = cast();
    make(&mut c, "a");
    let bob = sign(
        &c.bob,
        "card.update",
        "a",
        r#"{"team":"ops","needs_owner":true}"#,
        NOW,
    );
    let updated = c.s.card_update(NOW, &bob).unwrap();
    assert_eq!((updated.team.as_str(), updated.needs_owner), ("ops", true));
    let zero = sign(&c.sup, "card.update", "a", r#"{"size":0}"#, NOW);
    let err = c.s.card_update(NOW, &zero).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m) if m == SIZE_MSG));
    let long = format!(r#"{{"team":"{}"}}"#, "t".repeat(65));
    let op = sign(&c.sup, "card.update", "a", &long, NOW);
    assert!(matches!(
        c.s.card_update(NOW, &op),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(ext(&c, "a").size, 1.0);
}

#[test]
fn card_update_caps_the_owner_action_at_1000_characters() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let at_cap = "é".repeat(1000);
    let arg = serde_json::json!({"owner_action": at_cap}).to_string();
    let ok = sign(&c.sup, "card.update", "a", &arg, NOW);
    c.s.card_update(NOW, &ok).unwrap();
    assert_eq!(ext(&c, "a").owner_action, at_cap);
    let arg = serde_json::json!({"owner_action": "é".repeat(1001)}).to_string();
    let too_long = sign(&c.sup, "card.update", "a", &arg, NOW);
    let err = c.s.card_update(NOW, &too_long).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m)
        if m == "owner action is longer than 1000 characters"));
    assert_eq!(ext(&c, "a").owner_action, at_cap);
}

#[test]
fn card_dep_adds_and_removes_a_prerequisite() {
    let mut c = cast();
    make(&mut c, "a");
    make(&mut c, "b");
    let add = sign(&c.sup, "card.dep", "b", r#"{"after":"a"}"#, NOW);
    c.s.card_dep(NOW, &add).unwrap();
    assert_eq!(ext(&c, "b").after, ["a"]);
    let del = sign(
        &c.sup,
        "card.dep",
        "b",
        r#"{"after":"a","remove":true}"#,
        NOW,
    );
    c.s.card_dep(NOW, &del).unwrap();
    assert!(ext(&c, "b").after.is_empty());
    let again = sign(
        &c.sup,
        "card.dep",
        "b",
        r#"{"after":"a","remove":true}"#,
        NOW,
    );
    let err = c.s.card_dep(NOW, &again).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m) if m == "card has no such dependency"));
}

#[test]
fn card_dep_refuses_a_cycle_and_keeps_the_graph() {
    let mut c = cast();
    for id in ["a", "b", "c"] {
        make(&mut c, id);
    }
    for (card, after) in [("b", "a"), ("c", "b")] {
        let arg = format!(r#"{{"after":"{after}"}}"#);
        c.s.card_dep(NOW, &sign(&c.sup, "card.dep", card, &arg, NOW))
            .unwrap();
    }
    let arg = r#"{"after":"c"}"#;
    let err =
        c.s.card_dep(NOW, &sign(&c.sup, "card.dep", "a", arg, NOW))
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m) if m == CYCLE));
    assert!(ext(&c, "a").after.is_empty());
}

#[test]
fn card_dep_rejects_self_and_unknown_prerequisites() {
    let mut c = cast();
    make(&mut c, "a");
    let self_dep = sign(&c.sup, "card.dep", "a", r#"{"after":"a"}"#, NOW);
    assert!(matches!(
        c.s.card_dep(NOW, &self_dep),
        Err(StoreError::Invalid(_))
    ));
    let ghost = sign(&c.sup, "card.dep", "a", r#"{"after":"ghost"}"#, NOW);
    assert!(matches!(c.s.card_dep(NOW, &ghost), Err(StoreError::NotFound(g)) if g == "ghost"));
}

#[test]
fn card_link_adds_and_removes_typed_links() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let add = r#"{"rel":"blocks","target":"b"}"#;
    c.s.card_link(NOW, &sign(&c.alice, "card.link", "a", add, NOW))
        .unwrap();
    let links = ext(&c, "a").links;
    assert_eq!(links.len(), 1);
    assert_eq!(
        (links[0].rel, links[0].target.as_str()),
        (LinkRel::Blocks, "b")
    );
    let dup = sign(&c.alice, "card.link", "a", add, NOW);
    assert!(matches!(
        c.s.card_link(NOW, &dup),
        Err(StoreError::Invalid(_))
    ));
    let del = r#"{"rel":"blocks","target":"b","remove":true}"#;
    c.s.card_link(NOW, &sign(&c.alice, "card.link", "a", del, NOW))
        .unwrap();
    assert!(ext(&c, "a").links.is_empty());
    let bob = sign(&c.bob, "card.link", "a", add, NOW);
    assert!(is_forbidden(
        &c.s.card_link(NOW, &bob).unwrap_err(),
        Role::Worker
    ));
}

#[test]
fn escalate_without_targets_notifies_the_supervisors() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let arg = r#"{"owner_action":"need a decision"}"#;
    let notified =
        c.s.card_escalate(NOW, &sign(&c.alice, "card.escalate", "a", arg, NOW), false)
            .unwrap();
    let mut want = vec![hex(&c.owner.public()), hex(&c.sup.public())];
    want.sort();
    assert_eq!(notified, want);
    let e = ext(&c, "a");
    assert!(e.needs_owner && e.owner_action == "need a decision");
    let notes = notices_for(&c, "a");
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().all(|(_, kind, text)| kind == "escalation"
        && text == "card a (a) needs its owner: need a decision"));
}

#[test]
fn escalate_uses_the_policy_chain_then_explicit_targets() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let bob_hex = hex(&c.bob.public());
    let chain = serde_json::json!({"escalation_chain": [bob_hex]}).to_string();
    c.s.conn
        .execute(
            "INSERT INTO policies(team, lane, json, updated, by) VALUES ('*','*',?,?,?)",
            params![chain, NOW, "owner"],
        )
        .unwrap();
    let arg = r#"{"owner_action":"look"}"#;
    let notified =
        c.s.card_escalate(NOW, &sign(&c.alice, "card.escalate", "a", arg, NOW), false)
            .unwrap();
    assert_eq!(notified, [bob_hex.as_str()]);
    assert_eq!(notices_for(&c, "a")[0].0, bob_hex);

    make(&mut c, "b");
    let to = serde_json::json!({"owner_action": "look", "to": [hex(&c.sup.public())]});
    let notified =
        c.s.card_escalate(
            NOW,
            &sign(&c.sup, "card.escalate", "b", &to.to_string(), NOW),
            false,
        )
        .unwrap();
    assert_eq!(notified, [hex(&c.sup.public())]);
    let bad = r#"{"owner_action":"look","to":["NOT-HEX"]}"#;
    let err =
        c.s.card_escalate(NOW, &sign(&c.sup, "card.escalate", "b", bad, NOW), false)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m) if m == TARGET_MSG));
}

/// Card `a` claimed by alice, then the escalate arg with `owner_action` and explicit `to`.
fn escalate_to(c: &mut Cast, owner_action: &str, to: &[String]) -> Result<Vec<String>, StoreError> {
    let arg = serde_json::json!({"owner_action": owner_action, "to": to}).to_string();
    c.s.card_escalate(NOW, &sign(&c.alice, "card.escalate", "a", &arg, NOW), false)
}

#[test]
fn escalate_caps_the_owner_action_at_1000_characters() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let to = [hex(&c.sup.public())];
    let at_cap = "é".repeat(1000);
    assert!(escalate_to(&mut c, &at_cap, &to).is_ok());
    assert_eq!(ext(&c, "a").owner_action, at_cap);
    let too_long = "é".repeat(1001);
    let err = escalate_to(&mut c, &too_long, &to).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m)
        if m == "owner action is longer than 1000 characters"));
    assert_eq!(ext(&c, "a").owner_action, at_cap);
}

#[test]
fn escalate_caps_explicit_targets_at_16() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    // Sixteen well-formed but unknown keys pass the count and fail on activity; seventeen fail
    // on the count first.
    let keys: Vec<String> = (1..=17).map(|i| format!("{i:064x}")).collect();
    let err = escalate_to(&mut c, "look", &keys[..16]).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m)
        if m == "escalation target is not an active identity"));
    let err = escalate_to(&mut c, "look", &keys).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(m) if m == "too many escalation targets"));
    assert!(!ext(&c, "a").needs_owner);
}

#[test]
fn escalate_refuses_a_revoked_system_or_unknown_target() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    let bob = c.bob.public();
    let sys = c.sys.public();
    c.s.conn
        .execute(
            "UPDATE identities SET revoked=1 WHERE pubkey=?",
            [bob.as_slice()],
        )
        .unwrap();
    let unknown = format!("{:064x}", 77);
    for target in [hex(&bob), hex(&sys), unknown] {
        let err = escalate_to(&mut c, "look", &[target]).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(m)
            if m == "escalation target is not an active identity"));
    }
    // A refused escalation writes nothing: the card is not marked and no notice is posted.
    assert!(!ext(&c, "a").needs_owner);
    assert!(notices_for(&c, "a").is_empty());
    // An active supervisor is still accepted.
    let sup = hex(&c.sup.public());
    assert!(escalate_to(&mut c, "look", &[sup]).is_ok());
}

#[test]
fn escalate_rights_follow_ownership() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    make(&mut c, "u");
    let arg = r#"{"owner_action":"help"}"#;
    let bob = sign(&c.bob, "card.escalate", "a", arg, NOW);
    let err = c.s.card_escalate(NOW, &bob, false).unwrap_err();
    assert!(is_forbidden(&err, Role::Worker));
    let alice_unowned = sign(&c.alice, "card.escalate", "u", arg, NOW);
    let err = c.s.card_escalate(NOW, &alice_unowned, false).unwrap_err();
    assert!(is_forbidden(&err, Role::Worker));
    let sup = sign(&c.sup, "card.escalate", "a", arg, NOW);
    assert!(c.s.card_escalate(NOW, &sup, false).is_ok());
}

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
