//! Unit tests of the run, cancel, reclaim, update, assign, settings, card and scope operations.

use super::*;
use agora_sched::model_ops::{AddArgs, RunArgs};

#[test]
fn a_queued_run_needs_acceptance_and_then_is_requested_with_a_prediction() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let argv = vec!["true".to_string()];
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add c");
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("bob claims");
    let run = op(&bob, "work.run", "c", json!({"task": "c"}));
    let err = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect_err("not accepted");
    assert!(
        err.to_string().contains("you have not accepted it"),
        "{err}"
    );
    let digest = claims::digest(&argv, "/", &[]);
    w.work_accept(
        NOW,
        &op(&bob, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("accept");
    let run = op(&bob, "work.run", "c", json!({"task": "c"}));
    let requested = w.work_request(NOW, &run, 4.0, "/logs").expect("run");
    assert_eq!(requested.requested_ms, Some(NOW));
    assert_eq!(requested.log.as_deref(), Some("/logs/c.log"));
    assert!(requested.prediction.is_some());
}

#[test]
fn an_inline_run_creates_a_claimed_and_accepted_job() {
    let mut w = world();
    let alice = alice();
    let args = RunArgs {
        task: None,
        add: Some(AddArgs {
            command: Some(vec!["true".to_string()]),
            cwd: Some("/".to_string()),
            ..AddArgs::default()
        }),
    };
    let run = op(
        &alice,
        "work.run",
        "",
        serde_json::to_value(args).expect("json"),
    );
    let job = w.work_request(NOW, &run, 4.0, "/logs").expect("inline run");
    assert_eq!(job.id, "job-1");
    assert_eq!(job.state, TaskState::Claimed);
    assert_eq!(job.agent, Some(pk(&alice)));
    assert_eq!(job.accepted_by, Some(pk(&alice)));
    assert_eq!(job.requested_ms, Some(NOW));
    assert_eq!(job.log.as_deref(), Some("/logs/job-1.log"));
}

#[test]
fn an_inline_run_refuses_a_weight_above_the_cpus() {
    let mut w = world();
    let alice = alice();
    let args = json!({"add": {"command": ["true"], "cwd": "/", "weight": 2.0}});
    let err = w
        .work_request(NOW, &op(&alice, "work.run", "", args), 1.0, "/logs")
        .expect_err("too heavy");
    assert!(
        err.to_string()
            .contains("more than the 1 effective CPUs of this machine"),
        "{err}"
    );
}

#[test]
fn cancel_of_a_running_task_flags_it_and_returns_the_stop_target() {
    let mut w = world();
    let alice = alice();
    let argv = vec!["sleep".to_string(), "9".to_string()];
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "r", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add r");
    w.conn
        .execute(
            "UPDATE work_tasks SET state='running', agent=?1, accepted=?1, requested=?2, supervisor_pid=4242, \
             supervisor_start=1700 WHERE id='r'",
            rusqlite::params![pk(&alice), NOW],
        )
        .expect("mark running");
    let cancel = op(&alice, "work.cancel", "r", json!({"note": "stop"}));
    let outcome = w.work_cancel(NOW, &cancel).expect("cancel running");
    assert_eq!(outcome.task.state, TaskState::Running);
    assert!(outcome.task.cancel_requested);
    let stop = outcome.stop.expect("stop target");
    assert_eq!(stop.supervisor_pid, 4242);
    assert_eq!(stop.supervisor_start_s, Some(1700));
    assert_eq!(stop.job_dir, "");
}

#[test]
fn reclaim_needs_a_supervisor_and_returns_the_claim_to_the_queue() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let sup = sup();
    queue(&mut w, &alice, "t1");
    w.work_claim(NOW, &op(&bob, "work.claim", "t1", json!({})))
        .expect("claim");
    let worker = op(&bob, "work.reclaim", "task:t1", json!({"note": "mine"}));
    let err = w.work_reclaim(NOW, &worker).expect_err("worker reclaim");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let by_sup = op(&sup, "work.reclaim", "task:t1", json!({"note": "stale"}));
    w.work_reclaim(NOW, &by_sup).expect("supervisor reclaim");
    let task = w.work_task("t1").expect("task");
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(task.agent, None);
}

#[test]
fn update_refuses_a_prerequisite_cycle() {
    let mut w = world();
    let alice = alice();
    queue(&mut w, &alice, "a");
    let b = op(
        &alice,
        "work.add",
        "",
        json!({"id": "b", "scope": "proj", "after": ["a"]}),
    );
    w.work_add(NOW, &b).expect("add b");
    let cycle = op(&alice, "work.update", "a", json!({"after": ["b"]}));
    let err = w.work_update(NOW, &cycle).expect_err("cycle");
    assert!(matches!(err, StoreError::Refused(_)), "{err}");
}

#[test]
fn assigning_needs_a_supervisor_and_an_active_identity() {
    let mut w = world();
    let alice = alice();
    let sup = sup();
    queue(&mut w, &alice, "t1");
    let bob_pk = pk(&bob());
    let worker = op(&alice, "work.update", "t1", json!({"agent": bob_pk}));
    let err = w.work_update(NOW, &worker).expect_err("worker assigns");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let assign = op(&sup, "work.update", "t1", json!({"agent": bob_pk}));
    let assigned = w.work_update(NOW, &assign).expect("supervisor assigns");
    assert_eq!(assigned.state, TaskState::Claimed);
    assert_eq!(assigned.agent, Some(bob_pk.clone()));
    let unknown = op(&sup, "work.update", "t1", json!({"agent": pk(&eve())}));
    let err = w.work_update(NOW, &unknown).expect_err("unknown identity");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "agent is not an active worker identity"),
        "{err}"
    );
}

#[test]
fn settings_need_a_controller_and_refuse_bad_values() {
    let mut w = world();
    let sup = sup();
    let ctl = ctl();
    let patch = json!({"patch": {"cpu_budget": "2"}});
    let err = w
        .work_settings_set(NOW, &op(&sup, "work.settings", "", patch.clone()))
        .expect_err("supervisor");
    assert!(is_forbidden(&err, Role::Supervisor), "{err}");
    let settings = w
        .work_settings_set(NOW, &op(&ctl, "work.settings", "", patch))
        .expect("controller");
    assert_eq!(settings.cpu_budget, Some(2.0));
    let bad = op(
        &ctl,
        "work.settings",
        "",
        json!({"patch": {"self_accept": "maybe"}}),
    );
    let err = w.work_settings_set(NOW, &bad).expect_err("bad value");
    assert!(matches!(err, StoreError::Refused(_)), "{err}");
}

#[test]
fn a_task_linked_to_a_card_needs_the_card_to_exist_and_the_actor_to_own_it() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let sup = sup();
    let missing = op(
        &alice,
        "work.add",
        "",
        json!({"id": "x", "scope": "proj", "card": "nope"}),
    );
    let err = w.work_add(NOW, &missing).expect_err("no card");
    assert!(matches!(err, StoreError::NotFound(_)), "{err}");
    let card = op(&sup, "card.create", "k1", json!({"title": "card"}));
    w.create_card(NOW, &card).expect("card");
    let by_worker = op(
        &bob,
        "work.add",
        "",
        json!({"id": "y", "scope": "proj", "card": "k1"}),
    );
    let err = w.work_add(NOW, &by_worker).expect_err("not the owner");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let by_sup = op(
        &sup,
        "work.add",
        "",
        json!({"id": "z", "scope": "proj", "card": "k1"}),
    );
    let linked = w.work_add(NOW, &by_sup).expect("supervisor links");
    assert_eq!(linked.card.as_deref(), Some("k1"));
}

#[test]
fn a_scope_cannot_be_its_own_parent() {
    let mut w = world();
    let alice = alice();
    let cycle = op(
        &alice,
        "work.scope",
        "proj",
        json!({"id": "proj", "parent": "proj"}),
    );
    let err = w.work_scope_set(NOW, &cycle).expect_err("cycle");
    assert!(
        is_refused(
            &err,
            "scope proj cannot be under proj, which is proj or under it"
        ),
        "{err}"
    );
}

#[test]
fn a_running_cancel_notes_who_asked_for_the_stop_and_why() {
    let mut w = world();
    let alice = alice();
    let argv = vec!["sleep".to_string(), "9".to_string()];
    for (id, note) in [("r", "too slow"), ("r2", "")] {
        let add = op(
            &alice,
            "work.add",
            "",
            json!({"id": id, "scope": "proj", "command": argv, "cwd": "/"}),
        );
        w.work_add(NOW, &add).expect("add");
        w.conn
            .execute(
                "UPDATE work_tasks SET state='running', agent=?1, accepted=?1, requested=?2, supervisor_pid=4242, \
                 supervisor_start=1700 WHERE id=?3",
                rusqlite::params![pk(&alice), NOW, id],
            )
            .expect("mark running");
        let cancel = op(&alice, "work.cancel", id, json!({"note": note}));
        let outcome = w.work_cancel(NOW, &cancel).expect("cancel running");
        let by = agora_crypto::pseudonym(&alice.public());
        let expected = if note.is_empty() {
            format!("cancelled by {by}")
        } else {
            format!("cancelled by {by}: {note}")
        };
        assert_eq!(outcome.task.note, expected);
        assert!(outcome.task.cancel_requested);
    }
}

#[test]
fn reclaiming_a_quiet_window_is_refused_and_the_window_stays_open() {
    let mut w = world();
    let arg = json!({"expected_end_ms": NOW + 3_600_000, "note": "tests"}).to_string();
    let lease = w
        .quiet_open(NOW, &op_raw(&ctl(), "quiet.open", "", &arg))
        .expect("open quiet");
    let reclaim = op(
        &sup(),
        "work.reclaim",
        &format!("lease:{}", lease.id),
        json!({"note": "stale"}),
    );
    let err = w.work_reclaim(NOW, &reclaim).expect_err("quiet lease");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "use quiet.close"),
        "{err}"
    );
    let released: Option<i64> = w
        .conn
        .query_row(
            "SELECT released FROM leases WHERE id=?",
            [lease.id.as_str()],
            |r| r.get(0),
        )
        .expect("lease row");
    assert_eq!(released, None);
}

#[test]
fn an_unassign_clears_the_acceptance_so_a_reclaim_needs_a_fresh_accept() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let argv = vec!["true".to_string()];
    let digest = claims::digest(&argv, "/", &[]);
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add c");
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("bob claims");
    w.work_accept(
        NOW,
        &op(&bob, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("bob accepts");
    let unassign = op(&sup(), "work.update", "c", json!({"agent": null}));
    let back = w.work_update(NOW, &unassign).expect("unassign");
    assert_eq!(back.state, TaskState::Queued);
    assert_eq!(back.accepted_by, None);
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("bob claims again");
    let run = op(&bob, "work.run", "c", json!({"task": "c"}));
    let err = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect_err("stale accept");
    assert!(
        err.to_string().contains("you have not accepted it"),
        "{err}"
    );
    w.work_accept(
        NOW,
        &op(&bob, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("fresh accept");
    let retry = op(&bob, "work.run", "c", json!({"task": "c"}));
    let requested = w.work_request(NOW, &retry, 4.0, "/logs").expect("run");
    assert_eq!(requested.requested_ms, Some(NOW));
}

#[test]
fn assigning_to_an_observer_a_system_or_an_unknown_identity_is_refused() {
    let mut w = world();
    let alice = alice();
    let sup = sup();
    queue(&mut w, &alice, "t1");
    let system = ident(9);
    w.register_system_identity(NOW, &system.public())
        .expect("register system");
    for who in [olive(), system, eve()] {
        let assign = op(&sup, "work.update", "t1", json!({"agent": pk(&who)}));
        let err = w.work_update(NOW, &assign).expect_err("not a worker");
        assert!(
            matches!(err, StoreError::Invalid(m) if m == "agent is not an active worker identity"),
            "{err}"
        );
    }
    let task = w.work_task("t1").expect("task");
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(task.agent, None);
}

#[test]
fn an_inline_run_with_prerequisites_is_refused() {
    let mut w = world();
    let alice = alice();
    queue(&mut w, &alice, "t1");
    let args = RunArgs {
        task: None,
        add: Some(AddArgs {
            command: Some(vec!["true".to_string()]),
            cwd: Some("/".to_string()),
            after: vec!["t1".to_string()],
            ..AddArgs::default()
        }),
    };
    let run = op(
        &alice,
        "work.run",
        "",
        serde_json::to_value(args).expect("json"),
    );
    let err = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect_err("prerequisites");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "a new job takes no prerequisites"),
        "{err}"
    );
    assert!(w.work_task("job-1").is_err());
}

#[test]
fn added_claimed_and_accepted_events_carry_a_detail_and_their_scope() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let argv = vec!["true".to_string()];
    let digest = claims::digest(&argv, "/", &[]);
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "title": "build the thing", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add c");
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("claim");
    w.work_accept(
        NOW,
        &op(&bob, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("accept");
    let log = w.work_log(Some("c"), None, None, 50).expect("log");
    for kind in ["work.added", "work.claimed", "work.accepted"] {
        let event = log
            .iter()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} event"));
        assert!(!event.detail.is_empty(), "{kind} has no detail");
        assert_eq!(event.scope.as_deref(), Some("proj"), "{kind} scope");
    }
    let added = log.iter().find(|e| e.kind == "work.added").expect("added");
    assert_eq!(added.detail, "build the thing");
    let accepted = log
        .iter()
        .find(|e| e.kind == "work.accepted")
        .expect("accepted");
    assert_eq!(accepted.detail, format!("digest {digest}"));
}

/// The canonical form of `/tmp`, which is a symlink on macOS.
fn canonical_tmp() -> String {
    std::fs::canonicalize("/tmp")
        .expect("/tmp exists")
        .to_str()
        .expect("a UTF-8 path")
        .to_string()
}

#[test]
fn add_stores_and_digests_the_canonical_working_directory() {
    let mut w = world();
    let alice = alice();
    let argv = vec!["true".to_string()];
    let canonical = canonical_tmp();
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "scope": "proj", "command": argv, "cwd": "/tmp"}),
    );
    w.work_add(NOW, &add).expect("add c");
    let stored = w.work_task("c").expect("task c");
    assert_eq!(stored.cwd.as_deref(), Some(canonical.as_str()));
    assert_eq!(stored.digest, Some(claims::digest(&argv, &canonical, &[])));
}

#[test]
fn an_inline_run_stores_the_canonical_working_directory() {
    let mut w = world();
    let alice = alice();
    let canonical = canonical_tmp();
    let args = RunArgs {
        task: None,
        add: Some(AddArgs {
            command: Some(vec!["true".to_string()]),
            cwd: Some("/tmp".to_string()),
            ..AddArgs::default()
        }),
    };
    let run = op(
        &alice,
        "work.run",
        "",
        serde_json::to_value(args).expect("json"),
    );
    let job = w.work_request(NOW, &run, 4.0, "/logs").expect("inline run");
    assert_eq!(job.cwd.as_deref(), Some(canonical.as_str()));
}
