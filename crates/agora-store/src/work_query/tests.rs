//! Tests for the work-queue reads. Rows are inserted with plain SQL against an in-memory store.

use super::*;
use agora_sched::claims;
use rusqlite::params;

const PREDICTION: &str = r#"{"cores":1.5,"rss_kib":2048,"rss_p90_kib":null,"duration_s":3.0,"duration_p90_s":null,"level":"full","runs":2}"#;

fn store() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

fn key(byte: u8) -> String {
    crate::hex(&[byte; 32])
}

fn add_scope(store: &Store, id: &str, parent: Option<&str>) {
    store
        .conn
        .execute(
            "INSERT INTO work_scopes(id, kind, parent, task, agent, created, updated) \
             VALUES (?1, 'project', ?2, '', NULL, 1, 2)",
            params![id, parent],
        )
        .expect("insert scope");
}

/// A minimal task row. `queued` orders the queue; `seq` follows insertion.
fn add_task(store: &Store, id: &str, scope: Option<&str>, state: &str, queued: i64) {
    store
        .conn
        .execute(
            "INSERT INTO work_tasks(id, scope, author, queued, state) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, scope, key(1), queued, state],
        )
        .expect("insert task");
}

fn set_column(store: &Store, column: &str, id: &str, value: &dyn rusqlite::ToSql) {
    let sql = format!("UPDATE work_tasks SET {column} = ?1 WHERE id = ?2");
    store
        .conn
        .execute(&sql, params![value, id])
        .expect("update task");
}

fn add_event(store: &Store, ts: i64, actor: &str, kind: &str, subject: &str, data: &str) {
    store
        .conn
        .execute(
            "INSERT INTO events(ts, actor, kind, subject, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, actor, kind, subject, data],
        )
        .expect("insert event");
}

/// A `job_history` row; `sig` gives the four signature keys in level order (full, head, prog, tool).
fn insert_run(
    store: &Store,
    sig: [&str; 4],
    started: i64,
    ended: i64,
    status: Option<i64>,
    cpu: Option<f64>,
    weight: f64,
) {
    store
        .conn
        .execute(
            "INSERT INTO job_history(task, sig_full, sig_head, sig_prog, sig_tool, weight, status, \
             started, ended, cpu_time, rss_peak) VALUES ('job', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 100)",
            params![
                sig[0], sig[1], sig[2], sig[3], weight, status, started, ended, cpu
            ],
        )
        .expect("insert run");
}

fn add_lease(
    store: &Store,
    id: &str,
    kind: &str,
    resource: &str,
    holder: &str,
    created: i64,
    expires: Option<i64>,
) {
    store
        .conn
        .execute(
            "INSERT INTO leases(id, kind, resource, holder, created, expires, heartbeat) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5)",
            params![id, kind, resource, holder, created, expires],
        )
        .expect("insert lease");
}

#[test]
fn task_from_row_maps_every_column() {
    let store = store();
    add_scope(&store, "proj", None);
    store
        .conn
        .execute(
            r#"INSERT INTO work_tasks (id, title, kind, scope, author, queued, after, size, value, files,
                command, cwd, weight, state, agent, accepted, claimed, requested, started, supervisor_pid,
                pid, log, qos, cpu, rss, rss_peak, cpu_time, sampled, note, prediction, origin, exclusive,
                quiet_until, cancel_requested)
               VALUES ('t1', 'Fix the lexer', 'validity', 'proj', ?1, 100, '["t0"]', 2.5, 3.0, '["/a/b"]',
                '["cargo","test"]', '/work', 1.5, 'running', ?2, ?3, 110, 120, 130, 4000,
                4242, '/logs/t1.log', 'utility', 12.5, 2048, 4096, 7.25, 150, 'a note', ?4, 'job-origin', 1,
                500, 1)"#,
            params![key(1), key(2), key(3), PREDICTION],
        )
        .expect("insert task");

    let record = store.work_task("t1").expect("task");
    assert!(record.seq > 0);
    assert_eq!(record.id, "t1");
    assert_eq!(record.title, "Fix the lexer");
    assert_eq!(record.kind, "validity");
    assert_eq!(record.tier, 0, "validity is tier 0");
    assert_eq!(record.scope.as_deref(), Some("proj"));
    assert_eq!(record.author, key(1));
    assert_eq!(record.author_name, agora_crypto::pseudonym(&[1; 32]));
    assert_eq!(record.state, TaskState::Running);
    assert_eq!(record.agent.as_deref(), Some(key(2).as_str()));
    assert_eq!(
        record.agent_name.as_deref(),
        Some(agora_crypto::pseudonym(&[2; 32]).as_str())
    );
    assert_eq!(record.accepted_by.as_deref(), Some(key(3).as_str()));
    assert!(
        record.needs_acceptance,
        "accepted by someone other than the claimant"
    );
    assert_eq!(record.after, vec!["t0".to_string()]);
    assert_eq!(record.size, 2.5);
    assert_eq!(record.value, 3.0);
    assert_eq!(record.files, vec!["/a/b".to_string()]);
    assert_eq!(record.weight, 1.5);
    let argv = vec!["cargo".to_string(), "test".to_string()];
    assert_eq!(record.command.as_ref(), Some(&argv));
    assert_eq!(record.cwd.as_deref(), Some("/work"));
    assert_eq!(
        record.digest,
        Some(claims::digest(&argv, "/work", &["/a/b".to_string()]))
    );
    assert_eq!(record.queued_ms, 100);
    assert_eq!(record.claimed_ms, Some(110));
    assert_eq!(record.requested_ms, Some(120));
    assert_eq!(record.started_ms, Some(130));
    assert_eq!(record.ended_ms, None);
    assert_eq!(record.status, None);
    assert_eq!(record.pid, Some(4242));
    assert_eq!(record.supervisor_pid, Some(4000));
    assert_eq!(record.log.as_deref(), Some("/logs/t1.log"));
    assert_eq!(record.qos.as_deref(), Some("utility"));
    assert_eq!(record.prediction.as_ref().map(|p| p.runs), Some(2));
    assert_eq!(record.prediction.as_ref().map(|p| p.cores), Some(1.5));
    assert_eq!(record.cpu_percent, Some(12.5));
    assert_eq!(record.rss_kib, Some(2048));
    assert_eq!(record.rss_peak_kib, Some(4096));
    assert_eq!(record.cpu_seconds, Some(7.25));
    assert_eq!(record.sampled_ms, Some(150));
    assert_eq!(record.note, "a note");
    assert_eq!(record.card, None);
    assert_eq!(record.origin.as_deref(), Some("job-origin"));
    assert!(record.exclusive);
    assert_eq!(record.quiet_until_ms, Some(500));
    assert!(record.cancel_requested);

    set_column(&store, "accepted", "t1", &key(2));
    let accepted = store.work_task("t1").expect("task");
    assert!(!accepted.needs_acceptance, "accepted by the claimant");
}

#[test]
fn task_from_row_reports_malformed_json_as_an_error() {
    let store = store();
    add_task(&store, "bad", None, "queued", 1);
    set_column(&store, "after", "bad", &"not json");
    assert!(store.work_task("bad").is_err());
    set_column(&store, "after", "bad", &"[]");
    set_column(&store, "prediction", "bad", &"{");
    assert!(store.work_task("bad").is_err());
}

#[test]
fn work_task_reports_not_found() {
    let store = store();
    assert!(matches!(
        store.work_task("nope"),
        Err(StoreError::NotFound(message)) if message == "task nope"
    ));
}

#[test]
fn work_tasks_scope_filter_includes_descendants_and_hides_null_scope() {
    let store = store();
    add_scope(&store, "root", None);
    add_scope(&store, "child", Some("root"));
    add_scope(&store, "grand", Some("child"));
    add_scope(&store, "other", None);
    add_task(&store, "a", Some("root"), "queued", 1);
    add_task(&store, "b", Some("child"), "queued", 2);
    add_task(&store, "c", Some("grand"), "queued", 3);
    add_task(&store, "d", Some("other"), "queued", 4);
    add_task(&store, "e", None, "queued", 5);

    let ids = |scope: Option<&str>| -> Vec<String> {
        let filter = TaskFilter {
            scope: scope.map(str::to_string),
            ..TaskFilter::default()
        };
        store
            .work_tasks(&filter)
            .expect("tasks")
            .into_iter()
            .map(|task| task.id)
            .collect()
    };
    assert_eq!(ids(Some("root")), ["a", "b", "c"]);
    assert_eq!(ids(Some("child")), ["b", "c"]);
    assert_eq!(
        ids(None),
        ["a", "b", "c", "d", "e"],
        "no filter keeps NULL-scope tasks"
    );
    assert!(ids(Some("missing")).is_empty());
}

#[test]
fn work_tasks_filters_states_ids_and_limit() {
    let store = store();
    add_task(&store, "q1", None, "queued", 1);
    add_task(&store, "c1", None, "claimed", 2);
    add_task(&store, "d1", None, "done", 3);
    add_task(&store, "q2", None, "queued", 4);

    let listed = |filter: TaskFilter| -> Vec<String> {
        store
            .work_tasks(&filter)
            .expect("tasks")
            .into_iter()
            .map(|task| task.id)
            .collect()
    };
    let by_state = TaskFilter {
        states: vec![TaskState::Queued, TaskState::Claimed],
        ..TaskFilter::default()
    };
    assert_eq!(listed(by_state), ["q1", "c1", "q2"]);
    let by_id = TaskFilter {
        ids: vec!["d1".to_string()],
        ..TaskFilter::default()
    };
    assert_eq!(listed(by_id), ["d1"]);
    let first_two = TaskFilter {
        limit: 2,
        ..TaskFilter::default()
    };
    assert_eq!(listed(first_two), ["q1", "c1"]);
    let all = TaskFilter::default();
    assert_eq!(listed(all).len(), 4, "limit 0 means no limit");
}

#[test]
fn work_scopes_lists_subtree_with_own_open_counts() {
    let store = store();
    add_scope(&store, "root", None);
    add_scope(&store, "child", Some("root"));
    add_scope(&store, "other", None);
    add_task(&store, "r_q", Some("root"), "queued", 1);
    add_task(&store, "r_d", Some("root"), "done", 2);
    add_task(&store, "c_r", Some("child"), "running", 3);
    add_task(&store, "c_c", Some("child"), "claimed", 4);

    let subtree_scopes = store.work_scopes(Some("root")).expect("scopes");
    let mut ids: Vec<&str> = subtree_scopes.iter().map(|s| s.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["child", "root"]);
    let root = subtree_scopes
        .iter()
        .find(|s| s.id == "root")
        .expect("root");
    assert_eq!(root.kind, ScopeKind::Project);
    assert_eq!(root.counts.get("queued"), Some(&1));
    assert!(
        !root.counts.contains_key("done"),
        "final tasks are not counted"
    );
    let child = subtree_scopes
        .iter()
        .find(|s| s.id == "child")
        .expect("child");
    assert_eq!(child.parent.as_deref(), Some("root"));
    assert_eq!(child.counts.get("running"), Some(&1));
    assert_eq!(child.counts.get("claimed"), Some(&1));
    assert_eq!(store.work_scopes(None).expect("all").len(), 3);
    assert!(store.work_scopes(Some("missing")).expect("none").is_empty());
}

#[test]
fn work_counts_tallies_states_and_ready_queue() {
    let store = store();
    add_task(&store, "p1", None, "done", 1);
    add_task(&store, "c1", None, "claimed", 2);
    add_task(&store, "r1", None, "running", 3);
    add_task(&store, "f1", None, "failed", 4);
    add_task(&store, "x1", None, "cancelled", 5);
    add_task(&store, "q_ready", None, "queued", 6);
    add_task(&store, "q_free", None, "queued", 7);
    add_task(&store, "q_wait", None, "queued", 8);
    add_task(&store, "q_missing", None, "queued", 9);
    set_column(&store, "after", "q_ready", &r#"["p1"]"#);
    set_column(&store, "after", "q_wait", &r#"["c1"]"#);
    set_column(&store, "after", "q_missing", &r#"["ghost"]"#);

    let counts = store.work_counts(None).expect("counts");
    assert_eq!(counts.queued, 4);
    assert_eq!(counts.claimed, 1);
    assert_eq!(counts.running, 1);
    assert_eq!(counts.done, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.cancelled, 1);
    assert_eq!(counts.ready, 2, "q_ready (prerequisite done) and q_free");
}

#[test]
fn work_counts_respects_scope() {
    let store = store();
    add_scope(&store, "s", None);
    add_task(&store, "in_scope", Some("s"), "queued", 1);
    add_task(&store, "no_scope", None, "queued", 2);

    let counts = store.work_counts(Some("s")).expect("counts");
    assert_eq!(
        counts.queued, 1,
        "a NULL-scope task is hidden by a scope filter"
    );
    assert_eq!(counts.ready, 1);
    assert_eq!(store.work_counts(None).expect("all").queued, 2);
}

#[test]
fn work_log_returns_the_last_events_oldest_first() {
    let store = store();
    add_task(&store, "t1", None, "queued", 1);
    let actor = key(1);
    add_event(&store, 1, &actor, "work.added", "t1", "{}");
    add_event(&store, 2, &actor, "job.started", "t1", "{}");
    add_event(&store, 3, &actor, "card.created", "t1", "{}");
    add_event(&store, 4, &actor, "work.claimed", "t1", "{}");
    add_event(&store, 5, &actor, "job.finished", "t1", "{}");
    add_event(&store, 6, &actor, "work.done", "t1", "{}");

    let kinds = |limit: u32| -> Vec<String> {
        store
            .work_log(None, None, None, limit)
            .expect("log")
            .into_iter()
            .map(|event| event.kind)
            .collect()
    };
    assert_eq!(
        kinds(3),
        ["work.claimed", "job.finished", "work.done"],
        "the last three work and job events, oldest first"
    );
    assert_eq!(kinds(50).len(), 5, "card events are not work events");
    assert!(kinds(0).is_empty());
}

#[test]
fn work_log_filters_by_task_actor_and_scope() {
    let store = store();
    add_scope(&store, "root", None);
    add_task(&store, "t1", Some("root"), "queued", 1);
    add_task(&store, "t2", None, "queued", 2);
    add_event(
        &store,
        1,
        &key(1),
        "work.added",
        "t1",
        r#"{"detail":"hello"}"#,
    );
    add_event(&store, 2, &key(2), "job.started", "t2", "{}");
    add_event(
        &store,
        3,
        &key(2),
        "work.claimed",
        "t1",
        r#"{"scope":"other"}"#,
    );
    add_event(&store, 4, &key(1), "work.scoped", "root", "{}");
    add_event(&store, 5, &key(1), "card.created", "root", "{}");

    let log = |task: Option<&str>, actor: Option<&str>, scope: Option<&str>| {
        store.work_log(task, actor, scope, 50).expect("log")
    };
    let by_task: Vec<String> = log(Some("t1"), None, None)
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(by_task, ["work.added", "work.claimed"]);
    let by_actor: Vec<String> = log(None, Some(&key(2)), None)
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(by_actor, ["job.started", "work.claimed"]);
    let by_scope = log(None, None, Some("root"));
    let kinds: Vec<&str> = by_scope.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["work.added", "work.scoped"],
        "the data scope overrides the task scope; a NULL scope is hidden"
    );
    let first = log(Some("t1"), None, None)
        .into_iter()
        .next()
        .expect("first");
    assert_eq!(first.task.as_deref(), Some("t1"));
    assert_eq!(first.detail, "hello");
    assert_eq!(first.actor_name, agora_crypto::pseudonym(&[1; 32]));
    let scope_event = log(None, None, Some("root")).pop().expect("scope event");
    assert_eq!(scope_event.task, None, "a scope event has no task");
    assert_eq!(scope_event.scope.as_deref(), Some("root"));
}

#[test]
fn work_settings_defaults_then_reads_stored_json() {
    let store = store();
    assert_eq!(
        store.work_settings().expect("defaults"),
        WorkSettings::default()
    );
    store
        .conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, '{\"keep_days\": 7.0}')",
            [],
        )
        .expect("insert settings");
    let stored = store.work_settings().expect("stored");
    assert_eq!(stored.keep_days, 7.0);
    assert_eq!(stored.idle_min, WorkSettings::default().idle_min);
}

#[test]
fn work_settings_rejects_corrupt_json() {
    let store = store();
    store
        .conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, '{\"keep_days\": \"soon\"}')",
            [],
        )
        .expect("insert settings");
    assert!(store.work_settings().is_err());
}

mod queue;
