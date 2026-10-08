//! `timer.fire`: the task it enqueues, coalescing, disabling, revoked owners, and who may sign it.

use super::*;
use agora_sched::model::TaskState;

#[test]
fn fire_enqueues_a_task_authored_by_the_owner_with_origin() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let tpl = AddArgs {
        kind: "ci".to_string(),
        size: 2.0,
        command: Some(vec!["make".to_string(), "test".to_string()]),
        cwd: Some("/work".to_string()),
        ..template("nightly build")
    };
    create(&mut f, &alice, Some("nightly"), at(NOW + HOUR), tpl).unwrap();
    let task = fire(&mut f, "nightly", NOW + HOUR, NOW + HOUR)
        .unwrap()
        .unwrap();
    assert_eq!(task.id, "nightly-1");
    assert_eq!(task.author, hex_of(&alice));
    assert_eq!(task.origin.as_deref(), Some("timer:nightly"));
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(task.title, "nightly build");
    assert_eq!(task.kind, "ci");
    assert_eq!(task.size, 2.0);
    assert_eq!(
        task.command,
        Some(vec!["make".to_string(), "test".to_string()])
    );
    let added: i64 =
        f.s.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='work.added' AND subject='nightly-1' AND actor=?",
                [hex_of(&f.sys)],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(added, 1);
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(!rec.enabled);
    assert_eq!(rec.fired, 1);
    assert_eq!(rec.last_fire_ms, Some(NOW + HOUR));
}

#[test]
fn task_ids_skip_taken_names() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let every = TimerSpec::Every {
        every_s: 60,
        start_ms: Some(NOW + MIN),
    };
    create(&mut f, &alice, Some("e"), every, template("a")).unwrap();
    fire(&mut f, "e", NOW + MIN, NOW + MIN).unwrap();
    f.s.conn
        .execute(
            "INSERT INTO work_tasks(id,author,queued,state) VALUES ('e-2',?,?,'queued')",
            params![hex_of(&alice), NOW],
        )
        .unwrap();
    let task = fire(&mut f, "e", NOW + 2 * MIN, NOW + 2 * MIN)
        .unwrap()
        .unwrap();
    assert_eq!(task.id, "e-2-2");
}

#[test]
fn every_timer_coalesces_missed_fires_into_one_task() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let spec = TimerSpec::Every {
        every_s: 60,
        start_ms: Some(NOW + MIN),
    };
    let created = create(&mut f, &alice, Some("tick"), spec, template("a")).unwrap();
    assert_eq!(created.next_fire_ms, Some(NOW + MIN));
    let now = NOW + MIN + 3 * HOUR;
    let task = fire(&mut f, "tick", NOW + MIN, now).unwrap();
    assert!(task.is_some());
    let rec = f.s.timers(None).unwrap().remove(0);
    let next = rec.next_fire_ms.unwrap();
    assert!(next > now);
    assert_eq!((next - (NOW + MIN)) % MIN, 0);
    assert_eq!(next, now + MIN);
    assert!(rec.enabled);
    assert_eq!(rec.fired, 1);
    assert_eq!(
        count(
            &f,
            "SELECT COUNT(*) FROM work_tasks WHERE origin='timer:tick'"
        ),
        1
    );
}

#[test]
fn at_timer_disables_after_it_fires() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("once"), at(NOW + MIN), template("a")).unwrap();
    fire(&mut f, "once", NOW + MIN, NOW + MIN).unwrap();
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(!rec.enabled);
    assert_eq!(rec.next_fire_ms, None);
    assert_eq!(f.s.next_timer_due().unwrap(), None);
    assert!(f.s.due_timers(NOW + HOUR).unwrap().is_empty());
    let again = fire(&mut f, "once", NOW + 2 * MIN, NOW + 2 * MIN).unwrap();
    assert!(again.is_none());
    assert_eq!(count(&f, "SELECT COUNT(*) FROM work_tasks"), 1);
}

#[test]
fn revoked_owner_disables_the_timer_without_a_task() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let every = TimerSpec::Every {
        every_s: 60,
        start_ms: Some(NOW + MIN),
    };
    create(&mut f, &alice, Some("tick"), every, template("a")).unwrap();
    let revoke = op(&f.owner, "identity.revoke", &hex_of(&alice), "", NOW);
    f.s.revoke_identity(NOW, &revoke).unwrap();
    let task = fire(&mut f, "tick", NOW + MIN, NOW + MIN).unwrap();
    assert!(task.is_none());
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(!rec.enabled);
    assert_eq!(rec.next_fire_ms, None);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM work_tasks"), 0);
}

/// The data of the one event `kind` on `subject`, as JSON.
fn event_data(f: &Fixture, kind: &str, subject: &str) -> serde_json::Value {
    let text: String =
        f.s.conn
            .query_row(
                "SELECT data FROM events WHERE kind=? AND subject=?",
                params![kind, subject],
                |r| r.get(0),
            )
            .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn fire_events_carry_the_task_scope_and_the_timer_as_detail() {
    let mut f = fixture();
    f.s.conn
        .execute(
            "INSERT INTO work_scopes(id,kind,task,created,updated) VALUES ('proj','project','',?,?)",
            params![NOW, NOW],
        )
        .unwrap();
    let alice = Identity::from_seed([3; 32]);
    let tpl = AddArgs {
        scope: Some("proj".to_string()),
        ..template("a")
    };
    create(&mut f, &alice, Some("t"), at(NOW + MIN), tpl).unwrap();
    fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap().unwrap();
    let added = event_data(&f, "work.added", "t-1");
    assert_eq!(added["scope"], "proj");
    assert_eq!(added["detail"], "timer t");
    let fired = event_data(&f, "timer.fired", "t");
    assert_eq!(fired["scope"], "proj");
    assert_eq!(fired["detail"], "timer t");
    assert_eq!(fired["task"], "t-1");
}

#[test]
fn fire_events_carry_a_null_scope_when_the_task_has_none() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("t"), at(NOW + MIN), template("a")).unwrap();
    fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap().unwrap();
    assert!(event_data(&f, "work.added", "t-1")["scope"].is_null());
    assert!(event_data(&f, "timer.fired", "t")["scope"].is_null());
}

#[cfg(unix)]
#[test]
fn fired_task_cwd_is_the_canonical_path_behind_a_symlink() {
    let root = std::env::temp_dir().join(format!("agora-fire-cwd-{}", std::process::id()));
    let real = root.join("real");
    let link = root.join("link");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let canonical = std::fs::canonicalize(&real).unwrap();
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let tpl = AddArgs {
        command: Some(vec!["make".to_string()]),
        cwd: Some(link.to_str().unwrap().to_string()),
        ..template("a")
    };
    create(&mut f, &alice, Some("t"), at(NOW + MIN), tpl).unwrap();
    let task = fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap().unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(task.cwd.as_deref(), canonical.to_str());
}

#[test]
fn fire_keeps_a_cwd_that_does_not_resolve() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let missing = "/agora-no-such-dir-for-timer-fire/work";
    let tpl = AddArgs {
        command: Some(vec!["make".to_string()]),
        cwd: Some(missing.to_string()),
        ..template("a")
    };
    create(&mut f, &alice, Some("t"), at(NOW + MIN), tpl).unwrap();
    let task = fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap().unwrap();
    assert_eq!(task.cwd.as_deref(), Some(missing));
}

#[test]
fn a_worker_cannot_sign_timer_fire() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("t"), at(NOW + MIN), template("a")).unwrap();
    let arg = serde_json::to_string(&TimerFireArgs { fire_ms: NOW + MIN }).unwrap();
    let err =
        f.s.timer_fire(NOW + MIN, &op(&alice, "timer.fire", "t", &arg, NOW + MIN))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(rec.enabled);
    assert_eq!(rec.fired, 0);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM work_tasks"), 0);
}

/// The event and task counts, to show a fire changed nothing.
fn snapshot(f: &Fixture) -> (i64, i64) {
    (
        count(f, "SELECT COUNT(*) FROM events"),
        count(f, "SELECT COUNT(*) FROM work_tasks"),
    )
}

#[test]
fn fire_before_the_next_fire_is_a_no_op() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("t"), at(NOW + HOUR), template("a")).unwrap();
    let before = snapshot(&f);
    let task = fire(&mut f, "t", NOW + HOUR, NOW).unwrap();
    assert!(task.is_none());
    assert_eq!(snapshot(&f), before);
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(rec.enabled);
    assert_eq!(rec.next_fire_ms, Some(NOW + HOUR));
    assert_eq!(rec.fired, 0);
    assert_eq!(rec.last_fire_ms, None);
}

#[test]
fn fire_of_a_disabled_timer_is_a_no_op_even_when_due() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("t"), at(NOW + MIN), template("a")).unwrap();
    f.s.conn
        .execute("UPDATE timers SET enabled=0 WHERE id='t'", [])
        .unwrap();
    let before = snapshot(&f);
    let task = fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap();
    assert!(task.is_none());
    assert_eq!(snapshot(&f), before);
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(!rec.enabled);
    assert_eq!(rec.next_fire_ms, Some(NOW + MIN));
    assert_eq!(rec.fired, 0);
}

#[test]
fn fire_of_an_enabled_timer_without_a_next_fire_is_a_no_op() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("t"), at(NOW + MIN), template("a")).unwrap();
    f.s.conn
        .execute("UPDATE timers SET next_fire=NULL WHERE id='t'", [])
        .unwrap();
    let before = snapshot(&f);
    let task = fire(&mut f, "t", NOW + MIN, NOW + HOUR).unwrap();
    assert!(task.is_none());
    assert_eq!(snapshot(&f), before);
    let rec = f.s.timers(None).unwrap().remove(0);
    assert!(rec.enabled);
    assert_eq!(rec.fired, 0);
}

#[test]
fn fire_drops_a_scope_that_no_longer_exists_and_says_so() {
    let mut f = fixture();
    f.s.conn
        .execute(
            "INSERT INTO work_scopes(id,kind,task,created,updated) VALUES ('proj','project','',?,?)",
            params![NOW, NOW],
        )
        .unwrap();
    let alice = Identity::from_seed([3; 32]);
    let tpl = AddArgs {
        scope: Some("proj".to_string()),
        ..template("a")
    };
    create(&mut f, &alice, Some("t"), at(NOW + MIN), tpl).unwrap();
    f.s.conn
        .execute("DELETE FROM work_scopes WHERE id='proj'", [])
        .unwrap();
    let task = fire(&mut f, "t", NOW + MIN, NOW + MIN).unwrap().unwrap();
    assert_eq!(task.scope, None);
    assert!(task.note.contains("scope proj no longer exists"));
}
