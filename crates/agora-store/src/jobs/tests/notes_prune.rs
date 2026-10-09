use super::*;

#[test]
fn job_note_replaces_note_and_keeps_state() {
    let (mut s, system, _) = setup();
    insert_waiting(&s, "t1");
    start(&mut s, &system, "t1");
    let arg = serde_json::json!({"note": "its supervisor is gone"}).to_string();

    s.job_note(NOW + 1, &signed(&system, "job.note", "t1", &arg, NOW + 1))
        .unwrap();

    let note: String = s
        .conn
        .query_row("SELECT note FROM work_tasks WHERE id='t1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(note, "its supervisor is gone");
    assert_eq!(state_of(&s, "t1"), "running");
    assert_eq!(event_count(&s, "job.noted", "t1"), 1);
}

#[test]
fn job_events_carry_the_task_scope_and_a_detail() {
    let (mut s, system, _) = setup();
    s.conn
        .execute(
            "INSERT INTO work_scopes(id,kind,task,created,updated) VALUES ('proj','project','',?,?)",
            params![NOW, NOW],
        )
        .unwrap();
    insert_waiting(&s, "t1");
    s.conn
        .execute("UPDATE work_tasks SET scope='proj' WHERE id='t1'", [])
        .unwrap();
    start(&mut s, &system, "t1");
    let arg = serde_json::json!({"note": "slow disk"}).to_string();
    s.job_note(NOW + 1, &signed(&system, "job.note", "t1", &arg, NOW + 1))
        .unwrap();
    finish(&mut s, &system, "t1", Some(3), false).unwrap();

    let data = |kind: &str| -> serde_json::Value {
        let text: String = s
            .conn
            .query_row(
                "SELECT data FROM events WHERE kind=? AND subject='t1'",
                [kind],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    };
    for kind in ["job.started", "job.noted", "job.finished"] {
        assert_eq!(data(kind)["scope"], "proj", "{kind} carries the scope");
    }
    assert_eq!(data("job.started")["detail"], "process 4242");
    assert_eq!(data("job.noted")["detail"], "slow disk");
    assert_eq!(data("job.finished")["detail"], "status 3");
}

#[test]
fn job_sample_updates_only_running_tasks() {
    let (mut s, _, _) = setup();
    insert_waiting(&s, "t1");
    s.conn
        .execute("UPDATE work_tasks SET state='running' WHERE id='t1'", [])
        .unwrap();
    insert_waiting(&s, "t2");

    s.job_sample("t1", 12.5, 1000, 2000, NOW + 2_000).unwrap();
    s.job_sample("t2", 9.0, 1, 1, NOW + 2_000).unwrap();

    let sampled: (f64, i64, i64, i64) = s
        .conn
        .query_row(
            "SELECT cpu, rss, rss_peak, sampled FROM work_tasks WHERE id='t1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(sampled, (12.5, 1000, 2000, NOW + 2_000));
    let untouched: Option<f64> = s
        .conn
        .query_row("SELECT cpu FROM work_tasks WHERE id='t2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(untouched, None);
}

#[test]
fn work_prune_drops_old_events_and_returns_ended_logs() {
    let (mut s, _, _) = setup();
    let old = NOW - 10_000;
    s.conn
        .execute_batch(&format!(
            "INSERT INTO events(ts, actor, kind, subject) VALUES
                ({old}, 'a', 'work.added', 't-old'),
                ({old}, 'a', 'job.started', 't-old'),
                ({old}, 'a', 'card.created', 'c-old'),
                ({NOW}, 'a', 'work.added', 't-new');
             INSERT INTO reconcile_marks(key, last) VALUES ('mark-old', {old}), ('mark-new', {NOW});
             INSERT INTO notices(ts, target, kind, subject, text) VALUES
                ({old}, 'a', 'job_sync', 'c-old', 'old'),
                ({NOW}, 'a', 'job_sync', 'c-new', 'new');
             INSERT INTO job_history(task, weight, status, started, ended) VALUES ('old-done', 1, 0, {old}, {old});
             INSERT INTO work_tasks(id, author, queued, state, ended, log) VALUES
                ('old-done', 'a', 0, 'done', {old}, '/logs/old-done.log'),
                ('old-failed', 'a', 0, 'failed', {old}, '/logs/old-failed.log'),
                ('old-no-log', 'a', 0, 'done', {old}, NULL),
                ('old-running', 'a', 0, 'running', NULL, '/logs/old-running.log'),
                ('new-done', 'a', 0, 'done', {NOW}, '/logs/new-done.log');"
        ))
        .unwrap();

    let ids = s.work_prune(NOW - 1_000).unwrap();

    assert_eq!(ids, ["old-done", "old-failed"]);
    let log_of = |id: &str| -> Option<String> {
        s.conn
            .query_row("SELECT log FROM work_tasks WHERE id=?", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(log_of("old-done"), None);
    assert_eq!(log_of("old-failed"), None);
    assert_eq!(log_of("new-done").as_deref(), Some("/logs/new-done.log"));
    assert_eq!(
        log_of("old-running").as_deref(),
        Some("/logs/old-running.log")
    );
    // Old work.* and job.* events go; the old card event and the recent work event stay.
    assert_eq!(event_count(&s, "work.added", "t-old"), 0);
    assert_eq!(event_count(&s, "job.started", "t-old"), 0);
    assert_eq!(event_count(&s, "card.created", "c-old"), 1);
    assert_eq!(event_count(&s, "work.added", "t-new"), 1);
    // Reconcile marks and notices older than the cutoff go; the recent ones stay.
    let count = |sql: &str| -> i64 { s.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM reconcile_marks WHERE key='mark-old'"),
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM reconcile_marks WHERE key='mark-new'"),
        1
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM notices WHERE subject='c-old'"),
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM notices WHERE subject='c-new'"),
        1
    );
    // Job history is never pruned by retention.
    assert_eq!(
        count("SELECT COUNT(*) FROM job_history WHERE task='old-done'"),
        1
    );
}

#[test]
fn work_prune_drops_old_coordination_events_and_released_leases_only() {
    let (mut s, _, _) = setup();
    let old = NOW - 10_000;
    s.conn
        .execute_batch(&format!(
            "INSERT INTO events(ts, actor, kind, subject) VALUES
                ({old}, 'a', 'lease.acquired', 'old-lease'),
                ({old}, 'a', 'timer.created', 'old-timer'),
                ({old}, 'a', 'quiet.opened', 'old-quiet'),
                ({old}, 'a', 'pool.defined', 'old-pool'),
                ({old}, 'a', 'policy.set', 'old-policy'),
                ({old}, 'a', 'reconcile.blocked', 'old-card'),
                ({old}, 'a', 'card.created', 'c-keep'),
                ({NOW}, 'a', 'lease.renewed', 'new-lease');
             INSERT INTO leases(id, kind, resource, holder, created, expires, heartbeat, released) VALUES
                ('released-old', 'path', '/a/b', 'h', {old}, {NOW}, {old}, {old}),
                ('active-old', 'path', '/a/c', 'h', {old}, {NOW} + 1000000, {old}, NULL),
                ('released-new', 'path', '/a/d', 'h', {old}, {NOW} + 1000000, {old}, {NOW});
             INSERT INTO job_history(task, weight, status, started, ended) VALUES ('keep', 1, 0, {old}, {old});"
        ))
        .unwrap();

    s.work_prune(NOW - 1_000).unwrap();

    for (kind, subject) in [
        ("lease.acquired", "old-lease"),
        ("timer.created", "old-timer"),
        ("quiet.opened", "old-quiet"),
        ("pool.defined", "old-pool"),
        ("policy.set", "old-policy"),
        ("reconcile.blocked", "old-card"),
    ] {
        assert_eq!(event_count(&s, kind, subject), 0, "{kind} {subject}");
    }
    assert_eq!(event_count(&s, "card.created", "c-keep"), 1);
    assert_eq!(event_count(&s, "lease.renewed", "new-lease"), 1);
    let lease_exists = |id: &str| -> i64 {
        s.conn
            .query_row("SELECT COUNT(*) FROM leases WHERE id=?", [id], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(lease_exists("released-old"), 0);
    assert_eq!(lease_exists("active-old"), 1);
    assert_eq!(lease_exists("released-new"), 1);
    let history: i64 = s
        .conn
        .query_row(
            "SELECT COUNT(*) FROM job_history WHERE task='keep'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(history, 1);
}
