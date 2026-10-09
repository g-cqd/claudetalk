use super::*;

#[test]
fn card_create_stores_priority_value_size_team_and_prerequisites() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(
        &mut s,
        &c.sup,
        "c2",
        json!({"priority": 5, "value": 3.5, "size": 2.0, "team": "core", "after": ["c1"]}),
    );
    assert_eq!(
        s.card_ext(&["c2".to_string()]).expect("card ext"),
        vec![CardExt {
            id: "c2".into(),
            priority: 5,
            value: 3.5,
            size: 2.0,
            team: "core".into(),
            needs_owner: false,
            owner_action: String::new(),
            after: vec!["c1".into()],
            links: vec![],
            jobs: vec![],
        }]
    );
}

#[test]
fn card_create_refuses_invalid_fields_and_stores_no_card() {
    let (mut s, c) = in_memory();
    let cases: [(Value, &str); 9] = [
        (
            json!({"priority": "high"}),
            "card priority must be an integer",
        ),
        (json!({"value": 0}), "card value must be a positive number"),
        (
            json!({"value": -1.5}),
            "card value must be a positive number",
        ),
        (json!({"size": 0}), "card size must be a positive number"),
        (
            json!({"team": "t".repeat(65)}),
            "card team must be at most 64 characters",
        ),
        (json!({"team": 7}), "card team must be a string"),
        (
            json!({"after": "c1"}),
            "card after must be a list of card ids",
        ),
        (json!({"after": ["c9"]}), "a card cannot depend on itself"),
        (json!({"after": ["nope"]}), "nope not found"),
    ];
    for (extra, message) in cases {
        let arg = card_arg(extra.clone());
        let err = failure(s.create_card(NOW, &op(&c.sup, "card.create", "c9", &arg)));
        assert_eq!(err, message, "for {extra}");
    }
    assert_eq!(not_found(s.card_ext(&["c9".to_string()])), "c9");
}

// ---- update ----------------------------------------------------------------------------------------------------

#[test]
fn card_update_by_the_owner_changes_priority_value_size_and_team() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 3, "value": 2.5, "size": 4.0, "team": "core"}).to_string();
    let updated = s
        .card_update(NOW, &op(&c.alice, "card.update", "c1", &arg))
        .expect("owner update");
    assert_eq!(
        (
            updated.priority,
            updated.value,
            updated.size,
            updated.team.as_str()
        ),
        (3, 2.5, 4.0, "core")
    );
    assert_eq!(ext_of(&s, "c1"), updated);
}

#[test]
fn card_update_by_another_worker_is_forbidden_even_for_priority() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 9}).to_string();
    let role = forbidden(s.card_update(NOW, &op(&c.bob, "card.update", "c1", &arg)));
    assert_eq!(role, Role::Worker);
    assert_eq!(ext_of(&s, "c1").priority, 0);
}

#[test]
fn a_supervisor_may_update_a_card_another_worker_owns() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 9}).to_string();
    let updated = s
        .card_update(NOW, &op(&c.sup, "card.update", "c1", &arg))
        .expect("supervisor update");
    assert_eq!(updated.priority, 9);
}

#[test]
fn card_update_refuses_a_non_positive_value_and_changes_nothing() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"value": 0.0}).to_string();
    assert_eq!(
        failure(s.card_update(NOW, &op(&c.alice, "card.update", "c1", &arg))),
        "card value must be a positive number"
    );
    assert_eq!(ext_of(&s, "c1").value, 1.0);
}

// ---- dependencies ----------------------------------------------------------------------------------------------

#[test]
fn card_dep_adds_then_removes_a_prerequisite() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    dep(&mut s, &c.sup, "c1", "c2", false).expect("add");
    assert_eq!(after_of(&s, "c1"), vec!["c2"]);
    dep(&mut s, &c.sup, "c1", "c2", true).expect("remove");
    assert!(after_of(&s, "c1").is_empty());
}

#[test]
fn card_dep_refuses_a_cycle_and_keeps_the_graph_unchanged() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    dep(&mut s, &c.sup, "c1", "c2", false).expect("c1 after c2");
    assert_eq!(
        failure(dep(&mut s, &c.sup, "c2", "c1", false)),
        "card dependencies would form a cycle"
    );
    assert!(after_of(&s, "c2").is_empty());
    assert_eq!(after_of(&s, "c1"), vec!["c2"]);
}

#[test]
fn card_dep_on_an_unknown_card_is_not_found() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    assert_eq!(not_found(dep(&mut s, &c.sup, "c1", "nope", false)), "nope");
}

#[test]
fn card_dep_removal_of_a_missing_prerequisite_is_refused() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    assert_eq!(
        failure(dep(&mut s, &c.sup, "c1", "c2", true)),
        "card has no such dependency"
    );
}

// ---- links -----------------------------------------------------------------------------------------------------

#[test]
fn card_link_adds_and_removes_each_link_rel() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    let rels = [
        ("re", LinkRel::Re),
        ("blocks", LinkRel::Blocks),
        ("relates", LinkRel::Relates),
        ("duplicates", LinkRel::Duplicates),
        ("caused_by", LinkRel::CausedBy),
        ("job", LinkRel::Job),
        ("message", LinkRel::Message),
        ("note", LinkRel::Note),
    ];
    for (name, rel) in rels {
        link(&mut s, &c.sup, "c1", name, "c2", false).expect(name);
        assert_eq!(
            ext_of(&s, "c1").links,
            vec![CardLink {
                rel,
                target: "c2".into()
            }],
            "{name} is stored"
        );
        link(&mut s, &c.sup, "c1", name, "c2", true).expect(name);
        assert!(ext_of(&s, "c1").links.is_empty(), "{name} is removed");
    }
}

#[test]
fn card_link_refuses_a_repeated_link() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    link(&mut s, &c.sup, "c1", "relates", "c2", false).expect("first");
    assert_eq!(
        failure(link(&mut s, &c.sup, "c1", "relates", "c2", false)),
        "card already has that link"
    );
}

#[test]
fn card_link_removal_of_a_missing_link_is_refused() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    assert_eq!(
        failure(link(&mut s, &c.sup, "c1", "blocks", "c2", true)),
        "card has no such link"
    );
}

#[test]
fn card_ext_reports_defaults_prerequisites_links_and_the_jobs_linked_to_the_card() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c0", json!({}));
    new_card(&mut s, &c.sup, "c1", json!({}));
    dep(&mut s, &c.sup, "c1", "c0", false).expect("dep");
    link(&mut s, &c.sup, "c1", "relates", "c2", false).expect("link");
    queue_task(&mut s, &c.sup, "t1", Some("c1"));
    assert_eq!(
        s.card_ext(&["c1".to_string()]).expect("card ext"),
        vec![CardExt {
            id: "c1".into(),
            priority: 0,
            value: 1.0,
            size: 1.0,
            team: String::new(),
            needs_owner: false,
            owner_action: String::new(),
            after: vec!["c0".into()],
            links: vec![CardLink {
                rel: LinkRel::Relates,
                target: "c2".into()
            }],
            jobs: vec!["t1".into()],
        }]
    );
}

// ---- escalation ------------------------------------------------------------------------------------------------
