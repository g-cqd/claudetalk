//! Prediction history, holder claims and leases, queue order, quiet windows and plan output.

use super::*;

#[test]
fn work_history_keeps_five_newest_ran_runs_per_level() {
    let store = store();
    let sig = ["F", "H", "P", "T"];
    for index in 0..7 {
        let ended = 1000 + index * 100;
        insert_run(&store, sig, ended - 50, ended, Some(0), Some(1.0), 1.0);
    }
    insert_run(&store, sig, 8000, 9000, Some(127), Some(1.0), 1.0);
    insert_run(&store, sig, 8000, 9100, Some(126), Some(1.0), 1.0);
    insert_run(&store, sig, 8000, 9200, None, Some(1.0), 1.0);
    insert_run(&store, sig, 8000, 9300, Some(0), None, 1.0);

    let history = store
        .work_history(
            &Signatures {
                full: "F".into(),
                head: "H".into(),
                prog: "P".into(),
                tool: "T".into(),
            },
            4.0,
        )
        .expect("history");
    let full = &history.by_level[0];
    let ends: Vec<i64> = full.iter().map(|run| run.ended_ms).collect();
    assert_eq!(
        ends,
        [1600, 1500, 1400, 1300, 1200],
        "newest first, five at most, RAN only"
    );
    assert_eq!(
        history.by_level[3].len(),
        5,
        "the tool level reads its own column"
    );
}

#[test]
fn work_history_levels_read_their_own_signature_column() {
    let store = store();
    insert_run(
        &store,
        ["F", "Z", "Z", "Z"],
        0,
        1000,
        Some(0),
        Some(1.0),
        1.0,
    );
    insert_run(
        &store,
        ["Z", "H", "Z", "Z"],
        0,
        2000,
        Some(0),
        Some(1.0),
        1.0,
    );
    let history = store
        .work_history(
            &Signatures {
                full: "F".into(),
                head: "H".into(),
                prog: "P".into(),
                tool: "T".into(),
            },
            4.0,
        )
        .expect("history");
    assert_eq!(
        history.by_level[0]
            .iter()
            .map(|r| r.ended_ms)
            .collect::<Vec<_>>(),
        [1000]
    );
    assert_eq!(
        history.by_level[1]
            .iter()
            .map(|r| r.ended_ms)
            .collect::<Vec<_>>(),
        [2000]
    );
    assert!(history.by_level[2].is_empty());
}

#[test]
fn work_history_breaks_ended_ties_by_seq() {
    let store = store();
    let sig = ["F", "F", "F", "F"];
    insert_run(&store, sig, 1000, 5000, Some(0), Some(1.0), 1.0);
    insert_run(&store, sig, 4000, 5000, Some(0), Some(1.0), 1.0);
    let history = store
        .work_history(
            &Signatures {
                full: "F".into(),
                head: "F".into(),
                prog: "F".into(),
                tool: "F".into(),
            },
            4.0,
        )
        .expect("history");
    let starts: Vec<i64> = history.by_level[0].iter().map(|r| r.started_ms).collect();
    assert_eq!(
        starts,
        [4000, 1000],
        "the later insert (higher seq) comes first"
    );
}

#[test]
fn work_history_recent_durations_and_exclusive_runs() {
    let store = store();
    let sig = ["A", "B", "C", "D"];
    insert_run(&store, sig, 0, 2500, Some(0), Some(1.0), 1.0);
    insert_run(&store, sig, 1000, 9000, Some(1), Some(1.0), 4.0);
    insert_run(&store, sig, 0, 9500, Some(126), Some(1.0), 4.0);
    let history = store
        .work_history(
            &Signatures {
                full: "A".into(),
                head: "B".into(),
                prog: "C".into(),
                tool: "D".into(),
            },
            4.0,
        )
        .expect("history");
    assert_eq!(
        history.recent_durations_s,
        [8.0, 2.5],
        "seconds, newest first, RAN only"
    );
    let weights: Vec<bool> = history
        .exclusive_runs
        .iter()
        .map(|run| run.ended_ms == 9000)
        .collect();
    assert_eq!(weights, [true], "only runs with weight >= cpus");
}

#[test]
fn work_holders_lists_active_claims_and_path_leases() {
    let store = store();
    add_task(&store, "c1", None, "claimed", 1);
    add_task(&store, "r1", None, "running", 2);
    add_task(&store, "q1", None, "queued", 3);
    add_task(&store, "c2", None, "claimed", 4);
    add_task(&store, "d1", None, "done", 5);
    set_column(&store, "files", "c1", &r#"["/p/a"]"#);
    set_column(&store, "files", "r1", &r#"["/p/b"]"#);
    set_column(&store, "files", "q1", &r#"["/p/q"]"#);
    set_column(&store, "files", "d1", &r#"["/p/d"]"#);
    add_lease(&store, "L1", "path", "/l/x", &key(2), 1, None);
    add_lease(&store, "L2", "path", "/l/y", &key(3), 2, Some(1001));
    add_lease(&store, "L3", "path", "/l/z", &key(3), 3, Some(1000));
    add_lease(&store, "L4", "path", "/l/w", &key(3), 4, None);
    store
        .conn
        .execute("UPDATE leases SET released = 5 WHERE id = 'L4'", [])
        .expect("release");
    add_lease(&store, "L5", "pool", "gpu", &key(3), 5, None);
    add_lease(&store, "L6", "path", "/l/v", &key(9), 6, None);

    let holders = store.work_holders(1000, None).expect("holders");
    let expected: Vec<(String, Vec<String>)> = vec![
        ("c1".into(), vec!["/p/a".into()]),
        ("r1".into(), vec!["/p/b".into()]),
        ("lease:L1".into(), vec!["/l/x".into()]),
        ("lease:L2".into(), vec!["/l/y".into()]),
        ("lease:L6".into(), vec!["/l/v".into()]),
    ];
    assert_eq!(holders, expected);
    let excluded = store
        .work_holders(1000, Some(&key(2)))
        .expect("holders excluding one holder");
    assert!(!excluded.iter().any(|(id, _)| id == "lease:L1"));
    assert!(
        excluded.iter().any(|(id, _)| id == "c1"),
        "tasks are not excluded"
    );
}

#[test]
fn work_running_and_waiting_are_ordered() {
    let store = store();
    add_task(&store, "r_late", None, "running", 1);
    add_task(&store, "r_early", None, "running", 2);
    add_task(&store, "w_b", None, "claimed", 3);
    add_task(&store, "w_a", None, "claimed", 4);
    add_task(&store, "not_asked", None, "claimed", 5);
    store
        .conn
        .execute_batch(
            "UPDATE work_tasks SET started = 300 WHERE id = 'r_late';
             UPDATE work_tasks SET started = 200 WHERE id = 'r_early';
             UPDATE work_tasks SET requested = 500 WHERE id = 'w_b';
             UPDATE work_tasks SET requested = 400 WHERE id = 'w_a';",
        )
        .expect("update");

    let ids = |tasks: Vec<TaskRecord>| -> Vec<String> { tasks.into_iter().map(|t| t.id).collect() };
    assert_eq!(
        ids(store.work_running().expect("running")),
        ["r_early", "r_late"]
    );
    assert_eq!(ids(store.work_waiting().expect("waiting")), ["w_a", "w_b"]);
}

#[test]
fn work_window_closes_takes_the_earlier_of_window_end_and_job_end() {
    let store = store();
    add_task(&store, "a", None, "running", 1);
    add_task(&store, "b", None, "done", 2);
    add_task(&store, "c", None, "done", 3);
    add_task(&store, "d", None, "queued", 4);
    store
        .conn
        .execute_batch(
            "UPDATE work_tasks SET quiet_until = 1000 WHERE id IN ('a', 'b', 'c');
             UPDATE work_tasks SET ended = 600 WHERE id = 'b';
             UPDATE work_tasks SET ended = 2000 WHERE id = 'c';",
        )
        .expect("update");
    assert_eq!(
        store.work_window_closes().expect("closes"),
        [
            ("a".to_string(), 1000),
            ("b".to_string(), 600),
            ("c".to_string(), 1000)
        ]
    );
}

#[test]
fn work_plan_orders_queued_tasks_fifo_as_json_and_text() {
    let store = store();
    add_task(&store, "done1", None, "done", 1);
    add_task(&store, "item-gamma", None, "queued", 30);
    add_task(&store, "item-alpha", None, "queued", 10);
    add_task(&store, "item-beta", None, "queued", 20);
    set_column(&store, "after", "item-beta", &r#"["done1", "not-queued"]"#);

    let (json, text) = store
        .work_plan(None, OrderPolicy::Fifo, &Mode::Order)
        .expect("plan");
    let position = |haystack: &str, needle: &str| haystack.find(needle).expect("present");
    assert!(position(&json, "item-alpha") < position(&json, "item-beta"));
    assert!(position(&json, "item-beta") < position(&json, "item-gamma"));
    assert!(position(&text, "item-alpha") < position(&text, "item-beta"));
    assert!(position(&text, "item-beta") < position(&text, "item-gamma"));
}

#[test]
fn work_plan_reports_a_cycle_as_refused() {
    let store = store();
    add_task(&store, "a", None, "queued", 1);
    add_task(&store, "b", None, "queued", 2);
    set_column(&store, "after", "a", &r#"["b"]"#);
    set_column(&store, "after", "b", &r#"["a"]"#);
    let Err(err) = store.work_plan(None, OrderPolicy::Fifo, &Mode::Order) else {
        panic!("a dependency cycle must be refused");
    };
    assert!(
        matches!(err, StoreError::Refused(ref m) if m.starts_with("the queue cannot be ordered: "))
    );
}
