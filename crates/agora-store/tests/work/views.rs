use super::*;

// ---- log -------------------------------------------------------------------------------------------------------

#[test]
fn the_log_returns_the_newest_events_oldest_first_up_to_the_limit() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    s.work_claim(NOW + 2, &op_at(bob, "work.claim", "t1", "{}", NOW + 2))
        .expect("claim");
    let last_three = s.work_log(None, None, None, 3).expect("log");
    assert_eq!(
        kinds(&last_three),
        vec![
            ("work.added".to_string(), Some("t1".to_string())),
            ("work.added".to_string(), Some("t2".to_string())),
            ("work.claimed".to_string(), Some("t1".to_string())),
        ]
    );
    let newest = s.work_log(None, None, None, 1).expect("log");
    assert_eq!(
        kinds(&newest),
        vec![("work.claimed".to_string(), Some("t1".to_string()))]
    );
}

#[test]
fn the_log_filtered_to_one_task_lists_only_its_events() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    s.work_claim(NOW + 2, &op_at(bob, "work.claim", "t1", "{}", NOW + 2))
        .expect("claim");
    let only_t2 = s.work_log(Some("t2"), None, None, 10).expect("log");
    assert_eq!(
        kinds(&only_t2),
        vec![("work.added".to_string(), Some("t2".to_string()))]
    );
}

// ---- plan ------------------------------------------------------------------------------------------------------

#[test]
fn the_plan_orders_only_the_queued_tasks_by_policy_as_json() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({"kind": "feature"}));
    queue_at(s, alice, "t2", NOW + 1, json!({"kind": "security"}));
    queue_at(
        s,
        alice,
        "t3",
        NOW + 2,
        json!({"kind": "feature", "after": ["t1"]}),
    );
    queue_at(s, alice, "t4", NOW + 3, json!({"kind": "feature"}));
    claim(s, bob, "t4");
    let (text, _) = s
        .work_plan(Some(PROJ), OrderPolicy::Fifo, &Mode::Order)
        .expect("plan");
    let plan: Value = serde_json::from_str(&text).expect("plan json");
    assert_eq!(
        plan,
        json!({"policy": "fifo", "order": [
            {"id": "t2", "tier": 0, "moved_up_for": null},
            {"id": "t1", "tier": 1, "moved_up_for": null},
            {"id": "t3", "tier": 1, "moved_up_for": null},
        ]})
    );
}

// ---- holders ---------------------------------------------------------------------------------------------------

#[test]
fn holders_list_claimed_files_and_active_path_leases_but_not_queued_tasks() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"files": ["/proj/src/a.rs"]}));
    claim(s, bob, "t1");
    queue(s, alice, "t2", json!({"files": ["/proj/src/b.rs"]}));
    let lease = path_lease(s, alice, "/proj/docs", None);
    assert_eq!(
        s.work_holders(NOW, None).expect("holders"),
        vec![
            ("t1".to_string(), vec!["/proj/src/a.rs".to_string()]),
            (
                format!("lease:{}", lease.id),
                vec!["/proj/docs".to_string()]
            ),
        ]
    );
}

#[test]
fn excluding_an_identity_leaves_out_its_own_path_leases_only() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"files": ["/proj/src/a.rs"]}));
    claim(s, bob, "t1");
    path_lease(s, alice, "/proj/docs", None);
    let bobs = path_lease(s, bob, "/proj/out", None);
    let alice_pk = pk_hex(alice);
    let holders = s
        .work_holders(NOW, Some(alice_pk.as_str()))
        .expect("holders");
    assert_eq!(
        holders,
        vec![
            ("t1".to_string(), vec!["/proj/src/a.rs".to_string()]),
            (format!("lease:{}", bobs.id), vec!["/proj/out".to_string()]),
        ]
    );
}

#[test]
fn a_path_lease_stops_holding_at_its_expiry() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    path_lease(s, alice, "/proj/docs", Some(60.0));
    let just_before = s.work_holders(NOW + 59_999, None).expect("holders");
    assert_eq!(just_before.len(), 1);
    let at_expiry = s.work_holders(NOW + 60_000, None).expect("holders");
    assert!(at_expiry.is_empty());
}
