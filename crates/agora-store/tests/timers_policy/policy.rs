//! Team/lane policies: set rights, validation, upsert, layering of stored rows, and the notices an escalation leaves (section 3.2).

use super::*;
use agora_sched::model::PolicyRecord;
use agora_sched::policy::{EffectivePolicy, TeamPolicy};

/// Sets the policy of `team`/`lane` as `who` at `at`.
fn set_policy(
    s: &mut Store,
    who: &Identity,
    at: i64,
    team: &str,
    lane: &str,
    policy: Value,
) -> Result<PolicyRecord, StoreError> {
    let subject = format!("{team}/{lane}");
    let arg = json!({"team": team, "lane": lane, "policy": policy}).to_string();
    s.policy_set(at, &op_at(who, "policy.set", &subject, &arg, at))
}

/// Creates a backlog card as the supervisor.
fn backlog_card(s: &mut Store, sup: &Identity, id: &str) {
    s.create_card(
        NOW,
        &op(sup, "card.create", id, r#"{"title":"Port scheduler"}"#),
    )
    .expect("create card");
}

/// Escalates `card` as the supervisor with the default chain, returning the notified targets.
fn escalate(s: &mut Store, sup: &Identity, card: &str) -> Vec<String> {
    s.card_escalate(
        NOW,
        &op(
            sup,
            "card.escalate",
            card,
            r#"{"owner_action":"needs a decision"}"#,
        ),
        false,
    )
    .expect("escalate")
}

// ---- policy rights and validation ------------------------------------------------------------------------------

#[test]
fn a_worker_cannot_set_a_policy() {
    let mut f = fixture();
    let err = set_policy(
        &mut f.s,
        &f.alice,
        NOW,
        "proj",
        "dev",
        json!({"stale_after_s": 600}),
    );
    assert_eq!(forbidden(err), Role::Worker);
    assert!(f.s.policies().expect("policies").is_empty());
}

#[test]
fn a_supervisor_sets_a_lane_policy_and_it_is_read_back() {
    let mut f = fixture();
    let record = set_policy(
        &mut f.s,
        &f.sup,
        NOW,
        "proj",
        "dev",
        json!({"stale_after_s": 600}),
    )
    .expect("policy set");
    assert_eq!(record.team, "proj");
    assert_eq!(record.lane, "dev");
    assert_eq!(record.policy.stale_after_s, Some(600.0));
    assert_eq!(record.updated_ms, NOW);
    assert_eq!(record.by, pk_hex(&f.sup));
    let stored = f.s.policies().expect("policies");
    assert_eq!(stored, [record]);
}

#[test]
fn an_escalation_chain_needs_a_controller() {
    let mut f = fixture();
    let chain = json!({"escalation_chain": [pk_hex(&f.bob)]});
    assert_eq!(
        forbidden(set_policy(
            &mut f.s,
            &f.sup,
            NOW,
            "proj",
            "dev",
            chain.clone()
        )),
        Role::Supervisor
    );
    assert!(f.s.policies().expect("policies").is_empty());
    set_policy(&mut f.s, &f.ctl, NOW, "proj", "dev", chain).expect("controller sets the chain");
    assert_eq!(
        f.s.policies().expect("policies")[0].policy.escalation_chain,
        Some(vec![pk_hex(&f.bob)])
    );
}

#[test]
fn clearing_a_stored_escalation_chain_needs_a_controller() {
    let mut f = fixture();
    set_policy(
        &mut f.s,
        &f.ctl,
        NOW,
        "proj",
        "dev",
        json!({"escalation_chain": [pk_hex(&f.bob)]}),
    )
    .expect("controller sets the chain");
    // The supervisor's replacement leaves the chain out, which would clear it: that is a chain change.
    let err = set_policy(
        &mut f.s,
        &f.sup,
        NOW,
        "proj",
        "dev",
        json!({"stale_after_s": 600}),
    );
    assert_eq!(forbidden(err), Role::Supervisor);
    assert_eq!(
        f.s.policies().expect("policies")[0].policy.escalation_chain,
        Some(vec![pk_hex(&f.bob)])
    );
}

#[test]
fn a_policy_arg_must_name_the_team_and_lane_of_its_subject() {
    let mut f = fixture();
    let arg = json!({"team": "other", "lane": "dev", "policy": {}}).to_string();
    let err =
        f.s.policy_set(NOW, &op(&f.sup, "policy.set", "proj/dev", &arg));
    assert_eq!(
        invalid(err),
        "policy arg team and lane must match the subject"
    );
    assert!(f.s.policies().expect("policies").is_empty());
}

#[test]
fn a_policy_value_outside_its_limits_is_refused_with_the_validation_message() {
    let mut f = fixture();
    assert_eq!(
        refusal(set_policy(
            &mut f.s,
            &f.sup,
            NOW,
            "proj",
            "dev",
            json!({"stale_after_s": 0}),
        )),
        "stale_after_s must be a finite number greater than zero"
    );
    assert!(f.s.policies().expect("policies").is_empty());
}

// ---- upsert and layering ---------------------------------------------------------------------------------------

#[test]
fn setting_a_policy_again_replaces_the_stored_row_whole() {
    let mut f = fixture();
    set_policy(
        &mut f.s,
        &f.sup,
        NOW,
        "proj",
        "dev",
        json!({"stale_after_s": 600, "wip_limit": 3}),
    )
    .expect("first");
    let second = set_policy(
        &mut f.s,
        &f.sup,
        NOW + 1_000,
        "proj",
        "dev",
        json!({"escalate_after_s": 900}),
    )
    .expect("second");
    let stored = f.s.policies().expect("policies");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0], second);
    assert_eq!(
        stored[0].policy,
        TeamPolicy {
            escalate_after_s: Some(900.0),
            ..TeamPolicy::default()
        }
    );
    assert_eq!(stored[0].updated_ms, NOW + 1_000);
}

#[test]
fn effective_policy_takes_the_most_specific_stored_row_for_each_field() {
    let mut f = fixture();
    // Each layer sets the same field, so the most specific row must win: team/lane > */lane > team/* > */*.
    for (team, lane, stale) in [
        ("*", "*", 100),
        ("proj", "*", 200),
        ("*", "dev", 300),
        ("proj", "dev", 400),
    ] {
        set_policy(
            &mut f.s,
            &f.sup,
            NOW,
            team,
            lane,
            json!({"stale_after_s": stale}),
        )
        .expect("set layer");
    }
    let stale = |f: &Fixture, team: &str, lane: &str| {
        f.s.effective_policy(team, lane, false)
            .expect("effective")
            .stale_after_s
    };
    assert_eq!(stale(&f, "proj", "dev"), 400.0);
    assert_eq!(stale(&f, "proj", "qa"), 200.0);
    assert_eq!(stale(&f, "other", "dev"), 300.0);
    assert_eq!(stale(&f, "other", "qa"), 100.0);
}

#[test]
fn effective_policy_merges_layers_field_by_field() {
    let mut f = fixture();
    set_policy(
        &mut f.s,
        &f.sup,
        NOW,
        "*",
        "*",
        json!({"reclaim_after_s": 300}),
    )
    .expect("general layer");
    set_policy(
        &mut f.s,
        &f.sup,
        NOW,
        "proj",
        "dev",
        json!({"stale_after_s": 50}),
    )
    .expect("specific layer");
    let effective =
        f.s.effective_policy("proj", "dev", false)
            .expect("effective");
    // The specific row sets only stale_after_s, so the general row's reclaim_after_s still applies; the rest
    // are the defaults.
    assert_eq!(effective.stale_after_s, 50.0);
    assert_eq!(effective.reclaim_after_s, 300.0);
    assert_eq!(effective.escalate_after_s, 900.0);
}

#[test]
fn effective_policy_without_stored_rows_is_the_defaults() {
    let f = fixture();
    let effective =
        f.s.effective_policy("proj", "dev", true)
            .expect("effective");
    assert_eq!(
        effective,
        EffectivePolicy {
            stale_after_s: 1800.0,
            escalate_after_s: 900.0,
            reclaim_after_s: 3600.0,
            reclaim: true,
            require_review: true,
            wip_limit: None,
            escalation_chain: Vec::new(),
            sprint_rollover: true,
        }
    );
}

// ---- notices ---------------------------------------------------------------------------------------------------

#[test]
fn notices_page_forward_after_a_cursor_in_seq_order() {
    let mut f = fixture();
    let alice = pk_hex(&f.alice);
    set_policy(
        &mut f.s,
        &f.ctl,
        NOW,
        "*",
        "*",
        json!({"escalation_chain": [alice.clone()]}),
    )
    .expect("chain");
    for card in ["c1", "c2", "c3"] {
        backlog_card(&mut f.s, &f.sup, card);
        assert_eq!(escalate(&mut f.s, &f.sup, card), vec![alice.clone()]);
    }
    let all = f.s.notices(&alice, 0, 10).expect("notices");
    assert_eq!(
        all.iter().map(|n| n.subject.as_str()).collect::<Vec<_>>(),
        ["c1", "c2", "c3"]
    );
    assert!(all.iter().all(|n| n.kind == "escalation"
        && n.target == alice
        && n.ts_ms == NOW
        && n.text.contains("needs a decision")));
    let first_page = f.s.notices(&alice, 0, 2).expect("first page");
    assert_eq!(
        first_page
            .iter()
            .map(|n| n.subject.as_str())
            .collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    let cursor = first_page.last().expect("two notices").seq;
    let second_page = f.s.notices(&alice, cursor, 2).expect("second page");
    assert_eq!(
        second_page
            .iter()
            .map(|n| n.subject.as_str())
            .collect::<Vec<_>>(),
        ["c3"]
    );
    let end = second_page.last().expect("one notice").seq;
    assert!(
        f.s.notices(&alice, end, 2)
            .expect("past the end")
            .is_empty()
    );
    assert!(
        f.s.notices(&pk_hex(&f.bob), 0, 10)
            .expect("bob's")
            .is_empty()
    );
}

#[test]
fn a_notices_page_is_capped_at_500() {
    let mut f = fixture();
    let alice = pk_hex(&f.alice);
    set_policy(
        &mut f.s,
        &f.ctl,
        NOW,
        "*",
        "*",
        json!({"escalation_chain": [alice.clone()]}),
    )
    .expect("chain");
    for i in 0..501 {
        let card = format!("k{i}");
        backlog_card(&mut f.s, &f.sup, &card);
        escalate(&mut f.s, &f.sup, &card);
    }
    let page = f.s.notices(&alice, 0, u32::MAX).expect("notices");
    assert_eq!(page.len(), 500);
    assert_eq!(page.first().map(|n| n.subject.as_str()), Some("k0"));
    assert_eq!(page.last().map(|n| n.subject.as_str()), Some("k499"));
}
