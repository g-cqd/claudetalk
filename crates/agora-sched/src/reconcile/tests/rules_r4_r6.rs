//! Planner tests for rules R4 to R6: WIP limits, machine floors, and sprint rollovers.

use super::*;
#[test]
fn r4_wip_over_limit_notifies_each_supervisor_with_exact_text() {
    let i = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
        )],
        cards: vec![
            card("w1", "doing", Some("aa"), 0),
            card("w2", "claimed", Some("aa"), 0),
            card("w3", "doing", Some("aa"), 0),
            card("r1", "review", None, 0),
        ],
        supervisors: vec![key("5b"), key("5a")],
        ..input()
    };
    let wip = |target: &str| {
        notice(
            &format!("wip:core/dev:3:500000:{target}"),
            target,
            "wip",
            "core/dev",
            "lane core/dev has 3 cards in progress, above its WIP limit of 2",
        )
    };
    assert_eq!(plan(&i), vec![wip(&key("5a")), wip(&key("5b"))]);
}

#[test]
fn r4_at_the_limit_or_with_other_states_or_no_limit_emits_nothing() {
    let at_limit = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
        )],
        cards: vec![
            card("w1", "doing", Some("aa"), 0),
            card("w2", "claimed", Some("aa"), 0),
            card("r1", "ready", None, 0),
            card("r2", "review", None, 0),
            card("r3", "done", None, 0),
        ],
        supervisors: vec![key("5a")],
        ..input()
    };
    assert!(plan(&at_limit).is_empty());
    let no_limit = ReconcileInput {
        cards: (0..5)
            .map(|n| card(&format!("d{n}"), "doing", Some("aa"), 0))
            .collect(),
        supervisors: vec![key("5a")],
        ..input()
    };
    assert!(plan(&no_limit).is_empty());
}

#[test]
fn r5_disk_breach_pauses_admission_and_opens_a_blocker() {
    let i = ReconcileInput {
        machine: Some(machine(GIB_KIB)),
        ..input()
    };
    let text = "disk free 1.0 GiB below the floor of 5.0 GiB";
    assert_eq!(
        plan(&i),
        vec![
            ReconcileAction::Admission {
                key: "pause:3000000".to_owned(),
                paused: Some(text.to_owned()),
            },
            ReconcileAction::Blocker {
                key: "blocker:3000000".to_owned(),
                open: true,
                title: format!("Machine floor breached: {text}"),
                owner_action: DISK_OWNER_ACTION.to_owned(),
            },
        ]
    );
}

#[test]
fn r5_breaches_join_in_disk_memory_cpu_order() {
    let mut m = machine(GIB_KIB);
    m.total_memory_kib = Some(16 * GIB_KIB);
    m.signals = Some(Signals {
        idle_pct: Some(4.0),
        memory_level: None,
        load1: None,
        load_gate: false,
        memory_used_kib: Some(15 * GIB_KIB),
    });
    let i = ReconcileInput {
        machine: Some(m),
        settings: WorkSettings {
            memory_floor_kib: Some(2 * GIB_KIB),
            idle_floor_pct: Some(10.0),
            ..WorkSettings::default()
        },
        ..input()
    };
    let text = "disk free 1.0 GiB below the floor of 5.0 GiB; available memory 1.0 GiB below the floor of 2.0 GiB; CPU 4% idle below the floor of 10%";
    let actions = plan(&i);
    assert_eq!(
        actions[0],
        ReconcileAction::Admission {
            key: "pause:3000000".to_owned(),
            paused: Some(text.to_owned()),
        }
    );
    assert!(matches!(&actions[1], ReconcileAction::Blocker { title, .. }
        if title == &format!("Machine floor breached: {text}")));
}

#[test]
fn r5_unknown_readings_or_floors_are_never_breaches() {
    let settings = WorkSettings {
        memory_floor_kib: Some(2 * GIB_KIB),
        idle_floor_pct: Some(10.0),
        ..WorkSettings::default()
    };
    let mut no_total = healthy();
    no_total.signals = Some(Signals {
        idle_pct: None,
        memory_level: None,
        load1: None,
        load_gate: false,
        memory_used_kib: Some(15 * GIB_KIB),
    });
    let mut no_signals = healthy();
    no_signals.total_memory_kib = Some(16 * GIB_KIB);
    for m in [no_total, no_signals] {
        let i = ReconcileInput {
            machine: Some(m),
            settings: settings.clone(),
            ..input()
        };
        assert!(plan(&i).is_empty());
    }
}

#[test]
fn r5_memory_used_above_total_counts_as_no_available_memory() {
    let mut m = healthy();
    m.total_memory_kib = Some(GIB_KIB);
    m.signals = Some(Signals {
        idle_pct: None,
        memory_level: None,
        load1: None,
        load_gate: false,
        memory_used_kib: Some(3 * GIB_KIB),
    });
    let i = ReconcileInput {
        machine: Some(m),
        settings: WorkSettings {
            memory_floor_kib: Some(2 * GIB_KIB),
            ..WorkSettings::default()
        },
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![
            ReconcileAction::Admission {
                key: "pause:3000000".to_owned(),
                paused: Some("available memory 0.0 GiB below the floor of 2.0 GiB".to_owned()),
            },
            ReconcileAction::Blocker {
                key: "blocker:3000000".to_owned(),
                open: true,
                title:
                    "Machine floor breached: available memory 0.0 GiB below the floor of 2.0 GiB"
                        .to_owned(),
                owner_action:
                    "free memory (0.0 GiB < floor 2.0 GiB) — admission is paused until then"
                        .to_owned(),
            },
        ]
    );
}

#[test]
fn r5_breach_while_already_paused_refreshes_the_blocker_only() {
    let i = ReconcileInput {
        machine: Some(machine(GIB_KIB)),
        admission_paused: Some("earlier reason".to_owned()),
        blocker_card: Some("blocker-3".to_owned()),
        ..input()
    };
    let actions = plan(&i);
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0],
        ReconcileAction::Blocker { key, open: true, .. } if key == "blocker:3000000"));
}

#[test]
fn r5_recovery_resumes_admission_and_closes_the_blocker_card() {
    let i = ReconcileInput {
        machine: Some(healthy()),
        admission_paused: Some("disk free 1.0 GiB below the floor of 5.0 GiB".to_owned()),
        blocker_card: Some("blocker-3".to_owned()),
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![
            ReconcileAction::Admission {
                key: "resume:3000000".to_owned(),
                paused: None,
            },
            ReconcileAction::Blocker {
                key: "unblock:blocker-3".to_owned(),
                open: false,
                title: String::new(),
                owner_action: String::new(),
            },
        ]
    );
}

#[test]
fn r5_recovery_without_a_blocker_card_only_resumes_admission() {
    let i = ReconcileInput {
        machine: Some(healthy()),
        admission_paused: Some("old".to_owned()),
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![ReconcileAction::Admission {
            key: "resume:3000000".to_owned(),
            paused: None,
        }]
    );
}

#[test]
fn r5_without_a_machine_snapshot_no_floor_action_is_planned() {
    let i = ReconcileInput {
        machine: None,
        admission_paused: Some("old".to_owned()),
        blocker_card: Some("blocker-3".to_owned()),
        ..input()
    };
    assert!(plan(&i).is_empty());
}

#[test]
fn r6_ended_active_sprint_notifies_each_supervisor_with_exact_text() {
    let i = ReconcileInput {
        sprints: vec![sprint("s4", "Sprint 4", "active", NOW - 5_400_000, 3)],
        supervisors: vec![key("5b"), key("5a")],
        ..input()
    };
    let text =
        "sprint Sprint 4 ended 1 h 30 min ago with 3 open cards; close it and roll them over";
    assert_eq!(
        plan(&i),
        vec![
            notice(
                &format!("sprint:s4:{}", key("5a")),
                &key("5a"),
                "sprint_end",
                "s4",
                text
            ),
            notice(
                &format!("sprint:s4:{}", key("5b")),
                &key("5b"),
                "sprint_end",
                "s4",
                text
            ),
        ]
    );
}

#[test]
fn r6_uses_only_the_global_rollover_and_skips_inactive_or_future_sprints() {
    let sprints = vec![
        sprint("planned", "Planned", "planned", NOW - 1, 1),
        sprint("future", "Future", "active", NOW + 1, 1),
        sprint("done", "Done", "closed", NOW - 1, 1),
    ];
    let team_off = ReconcileInput {
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                sprint_rollover: Some(false),
                ..TeamPolicy::default()
            },
        )],
        sprints: vec![sprint("s1", "Sprint 1", "active", NOW - 1, 1)],
        supervisors: vec![key("5a")],
        ..input()
    };
    assert_eq!(
        plan(&team_off).len(),
        1,
        "a lane-level switch does not apply"
    );
    let global_off = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                sprint_rollover: Some(false),
                ..TeamPolicy::default()
            },
        )],
        sprints: vec![sprint("s1", "Sprint 1", "active", NOW - 1, 1)],
        supervisors: vec![key("5a")],
        ..input()
    };
    assert!(plan(&global_off).is_empty());
    let inactive = ReconcileInput {
        sprints,
        supervisors: vec![key("5a")],
        ..input()
    };
    assert!(plan(&inactive).is_empty());
}

#[test]
fn r4_and_r6_notice_keys_differ_per_supervisor_so_a_later_one_is_still_notified() {
    let i = ReconcileInput {
        policies: vec![policy(
            "*",
            "*",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
        )],
        cards: vec![
            card("w1", "doing", Some("aa"), 0),
            card("w2", "doing", Some("aa"), 0),
            card("w3", "doing", Some("aa"), 0),
        ],
        sprints: vec![sprint("s1", "Sprint 1", "active", NOW - 1, 0)],
        supervisors: vec![key("5a"), key("5b")],
        ..input()
    };
    let keys: Vec<String> = plan(&i).iter().map(|a| a.key().to_owned()).collect();
    assert_eq!(
        keys,
        vec![
            format!("wip:core/dev:3:500000:{}", key("5a")),
            format!("wip:core/dev:3:500000:{}", key("5b")),
            format!("sprint:s1:{}", key("5a")),
            format!("sprint:s1:{}", key("5b")),
        ]
    );
}

#[test]
fn r5_recovery_without_a_pause_still_closes_a_stale_blocker_card() {
    let i = ReconcileInput {
        machine: Some(healthy()),
        blocker_card: Some("blocker-7".to_owned()),
        ..input()
    };
    assert_eq!(
        plan(&i),
        vec![ReconcileAction::Blocker {
            key: "unblock:blocker-7".to_owned(),
            open: false,
            title: String::new(),
            owner_action: String::new(),
        }]
    );
}

#[test]
fn r5_owner_action_names_each_breached_resource_then_the_pause() {
    let mut m = machine(GIB_KIB);
    m.total_memory_kib = Some(16 * GIB_KIB);
    m.signals = Some(Signals {
        idle_pct: Some(4.0),
        memory_level: None,
        load1: None,
        load_gate: false,
        memory_used_kib: Some(15 * GIB_KIB),
    });
    let i = ReconcileInput {
        machine: Some(m),
        settings: WorkSettings {
            memory_floor_kib: Some(2 * GIB_KIB),
            idle_floor_pct: Some(10.0),
            ..WorkSettings::default()
        },
        ..input()
    };
    let owner_action = match plan(&i).as_slice() {
        [_, ReconcileAction::Blocker { owner_action, .. }] => owner_action.clone(),
        other => panic!("expected a pause and a blocker, got {other:?}"),
    };
    assert_eq!(
        owner_action,
        "free disk space (1.0 GiB < floor 5.0 GiB); free memory (1.0 GiB < floor 2.0 GiB); free CPU (4% < floor 10%) — admission is paused until then"
    );
}
