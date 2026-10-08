//! Tests for `reconcile_input` and `watchdog_report`.

use super::*;
use agora_sched::settings::WorkSettings;

#[test]
fn reconcile_input_snapshots_the_small_world() {
    let (mut s, _sys) = world();
    let sup = person(3);
    let ctl = person(4);
    let retired = person(7);
    let worker = person(5);
    enroll(&mut s, &sup, "supervisor", false);
    enroll(&mut s, &ctl, "controller", false);
    enroll(&mut s, &retired, "supervisor", true);
    enroll(&mut s, &worker, "worker", false);
    let (sp, cp, wp) = (pk_hex(&sup), pk_hex(&ctl), pk_hex(&worker));
    card(
        &s,
        "c-claimed",
        "t",
        "l",
        "claimed",
        Some(&wp),
        NOW - 5 * MIN,
    );
    card(&s, "c-doing", "t", "l", "doing", Some(&sp), NOW - MIN);
    card(&s, "c-backlog", "t", "l", "backlog", None, NOW - MIN);
    card(&s, "c-done", "t", "l", "done", Some(&wp), NOW - MIN);
    card(&s, "blocker-1", "", "ops", "blocked", None, NOW - MIN);
    lease(&s, "L-live", "path", &wp, Some(NOW + MIN), None);
    lease(&s, "L-done", "path", &wp, Some(NOW + MIN), Some(NOW - SEC));
    s.conn
        .execute(
            "INSERT INTO reconcile_marks(key, last, count) VALUES ('expire:L-done', ?1, 1)",
            [NOW - SEC],
        )
        .expect("mark");
    s.conn
        .execute(
            "INSERT INTO work_state(key, value) VALUES ('admission_paused', 'disk low')",
            [],
        )
        .expect("pause");
    s.conn
        .execute(
            "INSERT INTO work_state(key, value) VALUES ('blocker_card', 'blocker-1')",
            [],
        )
        .expect("blocker");
    s.conn
        .execute(
            "INSERT INTO sprints(id, name, goal, starts, ends, state) VALUES ('sp1', 'Sprint 1', '', ?1, ?2, 'active')",
            params![NOW - MIN, NOW - SEC],
        )
        .expect("sprint");
    s.conn
        .execute(
            "INSERT INTO sprints(id, name, goal, starts, ends, state) VALUES ('sp2', 'Later', '', ?1, ?2, 'planned')",
            params![NOW, NOW + MIN],
        )
        .expect("planned sprint");
    for card_id in ["c-claimed", "c-backlog", "c-done"] {
        s.conn
            .execute(
                "INSERT INTO sprint_cards(sprint, card) VALUES ('sp1', ?1)",
                [card_id],
            )
            .expect("sprint card");
    }
    policy(&s, "t", "l", r#"{"wip_limit":2}"#);

    let machine = MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: Some(16_000_000),
        disk_free_kib: Some(1_000),
        signals: None,
    };
    let input = s
        .reconcile_input(NOW, Some(machine.clone()), true)
        .expect("input");
    assert_eq!(input.now_ms, NOW);
    assert_eq!(input.settings, WorkSettings::default());
    assert_eq!(input.machine, Some(machine));
    assert!(input.require_review_default);

    let ids: Vec<&str> = input.cards.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-claimed", "c-doing"]);
    assert_eq!(
        input.cards[0],
        CardSnap {
            id: "c-claimed".into(),
            title: "c-claimed".into(),
            team: "t".into(),
            lane: "l".into(),
            state: "claimed".into(),
            owner: Some(wp.clone()),
            updated_ms: NOW - 5 * MIN,
        }
    );

    assert_eq!(input.leases.len(), 1);
    assert_eq!(input.leases[0].id, "L-live");
    assert_eq!(input.leases[0].kind, LeaseKind::Path);
    assert_eq!(input.leases[0].holder_name, pseudonym(&worker.public()));

    assert_eq!(
        input.marks,
        vec![Mark {
            key: "expire:L-done".into(),
            last_ms: NOW - SEC,
            count: 1
        }]
    );
    assert_eq!(input.policies.len(), 1);
    assert_eq!(input.policies[0].policy.wip_limit, Some(2));

    let mut expected = vec![sp.clone(), cp.clone()];
    expected.sort();
    assert_eq!(input.supervisors, expected);
    assert_eq!(input.admission_paused, Some("disk low".into()));
    assert_eq!(input.blocker_card, Some("blocker-1".into()));
    assert_eq!(
        input.sprints,
        vec![SprintSnap {
            id: "sp1".into(),
            name: "Sprint 1".into(),
            ends_ms: NOW - SEC,
            state: "active".into(),
            open_cards: 2,
        }]
    );

    s.conn
        .execute(
            "UPDATE cards SET state = 'cancelled' WHERE id = 'blocker-1'",
            [],
        )
        .expect("cancel blocker");
    let closed = s.reconcile_input(NOW, None, false).expect("input");
    assert_eq!(closed.blocker_card, None);
}

#[test]
fn watchdog_stages_stale_claims_by_their_thresholds() {
    let (s, _sys) = world();
    let w = pk_hex(&person(5));
    card(&s, "fresh", "t", "l", "claimed", Some(&w), NOW - 100 * SEC);
    card(&s, "stale", "t", "l", "doing", Some(&w), NOW - 1800 * SEC);
    card(
        &s,
        "escalated",
        "t",
        "l",
        "claimed",
        Some(&w),
        NOW - 2700 * SEC,
    );
    card(&s, "reclaim", "t", "l", "doing", Some(&w), NOW - 3600 * SEC);
    let report = s.watchdog_report(NOW, &w, false).expect("report");
    let stages: BTreeMap<&str, &str> = report
        .stale
        .iter()
        .map(|c| (c.id.as_str(), c.stage.as_str()))
        .collect();
    assert_eq!(
        stages,
        BTreeMap::from([
            ("escalated", "escalated"),
            ("reclaim", "reclaim due"),
            ("stale", "stale")
        ])
    );
    let stale = report
        .stale
        .iter()
        .find(|c| c.id == "stale")
        .expect("stale card");
    assert_eq!(stale.idle_s, 1800.0);
    assert_eq!(stale.owner, Some(w));
}

#[test]
fn watchdog_reports_escalated_instead_of_reclaim_due_when_reclaim_is_off() {
    let (s, _sys) = world();
    let w = pk_hex(&person(5));
    policy(&s, "t", "off", r#"{"reclaim":false}"#);
    card(&s, "c", "t", "off", "claimed", Some(&w), NOW - 4000 * SEC);
    let report = s.watchdog_report(NOW, &w, false).expect("report");
    assert_eq!(report.stale.len(), 1);
    assert_eq!(report.stale[0].stage, "escalated");
}

#[test]
fn watchdog_lists_expiring_leases_needs_owner_wip_and_the_pause() {
    let (s, _sys) = world();
    let w = pk_hex(&person(5));
    lease(&s, "soon", "path", &w, Some(NOW + 5 * MIN), None);
    lease(&s, "overdue", "pool", &w, Some(NOW - SEC), None);
    lease(&s, "later", "path", &w, Some(NOW + 30 * MIN), None);
    lease(&s, "released", "path", &w, Some(NOW - SEC), Some(NOW - SEC));
    policy(&s, "t", "l", r#"{"wip_limit":1}"#);
    card(&s, "a", "t", "l", "claimed", Some(&w), NOW - MIN);
    card(&s, "b", "t", "l", "doing", Some(&w), NOW - MIN);
    card(&s, "needs", "t", "l", "blocked", None, NOW - MIN);
    s.conn
        .execute(
            "UPDATE cards SET needs_owner = 1, owner_action = 'pick one' WHERE id = 'needs'",
            [],
        )
        .expect("needs owner");
    s.conn
        .execute(
            "INSERT INTO work_state(key, value) VALUES ('admission_paused', 'disk low')",
            [],
        )
        .expect("pause");

    let report = s.watchdog_report(NOW, &w, false).expect("report");
    let mut expiring: Vec<&str> = report
        .expiring_leases
        .iter()
        .map(|l| l.id.as_str())
        .collect();
    expiring.sort();
    assert_eq!(expiring, ["overdue", "soon"]);
    assert_eq!(
        report.needs_owner,
        vec![NeedsOwner {
            card: "needs".into(),
            title: "needs".into(),
            owner_action: "pick one".into(),
        }]
    );
    assert_eq!(
        report.wip,
        vec![WipView {
            team: "t".into(),
            lane: "l".into(),
            count: 2,
            limit: Some(1)
        }]
    );
    assert_eq!(report.paused, Some("disk low".into()));
    assert!(report.orphaned_jobs.is_empty());
}

#[test]
fn watchdog_returns_my_last_fifty_notices_oldest_first() {
    let (s, _sys) = world();
    let me = pk_hex(&person(5));
    let other = pk_hex(&person(6));
    for i in 1..=55 {
        s.conn
            .execute(
                "INSERT INTO notices(ts, target, kind, subject, text) VALUES (?1, ?2, 'stale', 'c', ?3)",
                params![NOW + i, me, format!("n{i}")],
            )
            .expect("notice");
    }
    s.conn
        .execute(
            "INSERT INTO notices(ts, target, kind, subject, text) VALUES (?1, ?2, 'stale', 'c', 'theirs')",
            params![NOW, other],
        )
        .expect("other notice");
    let report = s.watchdog_report(NOW, &me, false).expect("report");
    assert_eq!(report.notices.len(), 50);
    assert_eq!(report.notices.first().map(|n| n.text.as_str()), Some("n6"));
    assert_eq!(report.notices.last().map(|n| n.text.as_str()), Some("n55"));
    assert!(report.notices.iter().all(|n| n.target == me));
}

#[test]
fn watchdog_lists_running_jobs_whose_supervisor_is_gone() {
    let (s, _sys) = world();
    let w = pk_hex(&person(5));
    let gone = "its supervisor, process 4242, is gone; the command runs on as process 4243, and its status will not be recorded";
    for (id, state, note) in [
        ("orphan", "running", gone),
        ("healthy", "running", "started"),
        ("queued", "queued", gone),
    ] {
        s.conn
            .execute(
                "INSERT INTO work_tasks(id, author, queued, state, note) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, w, NOW, state, note],
            )
            .expect("task row");
    }
    let report = s.watchdog_report(NOW, &w, false).expect("report");
    assert_eq!(report.orphaned_jobs, vec!["orphan".to_string()]);
}
