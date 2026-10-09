use super::*;

// ---- scopes ----------------------------------------------------------------------------------------------------

#[test]
fn a_new_scope_is_a_project_at_the_top_and_an_agent_scope_under_a_parent() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let top = scope(s, alice, PROJ, None);
    assert_eq!((top.kind, top.parent), (ScopeKind::Project, None));
    let child = scope(s, alice, "proj-a", Some(PROJ));
    assert_eq!(child.kind, ScopeKind::Agent);
    assert_eq!(child.parent.as_deref(), Some(PROJ));
}

#[test]
fn listing_a_root_gives_its_subtree_in_creation_order_with_open_task_counts() {
    let mut w = world();
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    scope(s, alice, "other", None);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"scope": "proj-a"}));
    claim(s, bob, "t2");
    let listed = s.work_scopes(Some(PROJ)).expect("scopes");
    let rows: Vec<(&str, BTreeMap<String, u64>)> = listed
        .iter()
        .map(|row| (row.id.as_str(), row.counts.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (PROJ, BTreeMap::from([("queued".to_string(), 1)])),
            ("proj-a", BTreeMap::from([("claimed".to_string(), 1)])),
        ]
    );
}

#[test]
fn a_scope_cannot_be_put_under_itself_or_under_its_own_descendant() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    let under_child = refusal(set_scope(s, alice, json!({"id": PROJ, "parent": "proj-a"})));
    assert_eq!(
        under_child,
        "scope proj cannot be under proj-a, which is proj or under it"
    );
    let under_self = refusal(set_scope(s, alice, json!({"id": PROJ, "parent": PROJ})));
    assert_eq!(
        under_self,
        "scope proj cannot be under proj, which is proj or under it"
    );
}

#[test]
fn a_scope_with_an_unknown_parent_is_refused_and_not_created() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let msg = refusal(set_scope(s, alice, json!({"id": "x", "parent": "nope"})));
    assert_eq!(msg, "unknown parent scope nope");
    assert!(s.work_scopes(Some("x")).expect("scopes").is_empty());
}

#[test]
fn an_empty_parent_makes_a_scope_a_root_and_keeps_its_kind() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    let cleared = set_scope(s, alice, json!({"id": "proj-a", "parent": ""})).expect("clear");
    assert_eq!((cleared.parent, cleared.kind), (None, ScopeKind::Agent));
}
