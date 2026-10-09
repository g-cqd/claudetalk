use super::*;

#[test]
fn escalate_by_the_owner_marks_the_card_and_stores_the_owner_action() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    escalate(&mut s, &c.alice, "c1", "decide the API shape", &[]).expect("owner escalates");
    let ext = ext_of(&s, "c1");
    assert!(ext.needs_owner);
    assert_eq!(ext.owner_action, "decide the API shape");
}

#[test]
fn escalate_is_allowed_to_the_owner_and_supervisors_but_forbidden_to_other_workers() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    assert_eq!(
        forbidden(escalate(&mut s, &c.bob, "c1", "help", &[])),
        Role::Worker
    );
    assert!(!ext_of(&s, "c1").needs_owner);
    escalate(&mut s, &c.sup, "c1", "help", &[]).expect("supervisor escalates");
    assert!(ext_of(&s, "c1").needs_owner);
}

#[test]
fn escalate_without_targets_or_chain_notifies_every_supervisor_and_no_one_else() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[]).expect("escalate");
    let mut expected = vec![pk_hex(&c.owner), pk_hex(&c.sup)];
    expected.sort();
    assert_eq!(targets, expected);
    for who in [&c.owner, &c.sup] {
        let notice = only_notice(&s, who);
        assert_eq!(notice.kind, "escalation");
        assert_eq!(notice.subject, "c1");
        assert!(notice.text.contains("decide"), "{}", notice.text);
    }
    assert!(notices_for(&s, &c.bob).is_empty());
}

#[test]
fn escalate_without_targets_uses_the_policy_chain_instead_of_the_supervisors() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    set_policy(
        &mut s,
        &ctl,
        "core",
        "default",
        json!({"escalation_chain": [pk_hex(&c.carol)]}),
    );
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[]).expect("escalate");
    assert_eq!(targets, vec![pk_hex(&c.carol)]);
    assert_eq!(only_notice(&s, &c.carol).kind, "escalation");
    assert!(notices_for(&s, &c.sup).is_empty());
}

#[test]
fn escalate_with_explicit_targets_notifies_them_instead_of_the_policy_chain() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    set_policy(
        &mut s,
        &ctl,
        "core",
        "default",
        json!({"escalation_chain": [pk_hex(&c.carol)]}),
    );
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[pk_hex(&c.sup)]).expect("escalate");
    assert_eq!(targets, vec![pk_hex(&c.sup)]);
    assert_eq!(only_notice(&s, &c.sup).kind, "escalation");
    assert!(notices_for(&s, &c.carol).is_empty());
}

#[test]
fn escalate_refuses_an_owner_action_longer_than_1000_characters() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let too_long = "a".repeat(1001);
    assert_eq!(
        failure(escalate(&mut s, &c.alice, "c1", &too_long, &[])),
        "owner action is longer than 1000 characters"
    );
    assert!(!ext_of(&s, "c1").needs_owner);
    let at_cap = "a".repeat(1000);
    escalate(&mut s, &c.alice, "c1", &at_cap, &[]).expect("1000 characters is allowed");
    assert_eq!(ext_of(&s, "c1").owner_action.chars().count(), 1000);
}

#[test]
fn escalate_accepts_at_most_16_explicit_targets() {
    let (mut s, c) = in_memory();
    let mut keys = Vec::new();
    for i in 0u8..16 {
        let id = Identity::from_seed([0x40 + i; 32]);
        s.register_identity(NOW, &id.public(), &format!("target{i}"))
            .expect("setup: register target");
        keys.push(pk_hex(&id));
    }
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let mut seventeen = keys.clone();
    seventeen.push(pk_hex(&c.sup));
    assert_eq!(
        failure(escalate(&mut s, &c.alice, "c1", "decide", &seventeen)),
        "too many escalation targets"
    );
    let notified = escalate(&mut s, &c.alice, "c1", "decide", &keys).expect("16 targets");
    assert_eq!(notified.len(), 16);
}

#[test]
fn escalate_refuses_a_revoked_or_system_identity_as_a_target() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.carol), ""))
        .expect("setup: revoke carol");
    for key in [pk_hex(&c.carol), pk_hex(&c.sys)] {
        assert_eq!(
            failure(escalate(
                &mut s,
                &c.alice,
                "c1",
                "decide",
                std::slice::from_ref(&key)
            )),
            "escalation target is not an active identity",
            "target {key}"
        );
    }
    assert!(!ext_of(&s, "c1").needs_owner);
}

#[test]
fn escalate_refuses_a_target_that_is_not_a_public_key() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    assert_eq!(
        failure(escalate(
            &mut s,
            &c.alice,
            "c1",
            "decide",
            &["NOT-A-KEY".to_string()]
        )),
        "escalation target must be a 64-character lowercase hex public key"
    );
}

// ---- WIP gate --------------------------------------------------------------------------------------------------
