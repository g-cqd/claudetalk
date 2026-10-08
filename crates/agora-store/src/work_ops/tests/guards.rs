//! Unit tests of the abuse guards on the run path and the add path: the run role (S-1), the self-accept flip (S-1),
//! the waiting cap (S-5), the weight floor (S-7) and the depth of claimed paths (S-8).

use super::*;
use agora_sched::model::TaskRecord;

/// Raises the settings with a controller's patch.
fn patch(w: &mut Store, text: serde_json::Value) {
    w.work_settings_set(
        NOW,
        &op(&ctl(), "work.settings", "", json!({"patch": text})),
    )
    .expect("settings patch");
}

/// An inline run of `/bin/true` as `who`, with `weight`, in the root directory.
fn inline_run(who: &Identity, weight: f64) -> SignedOp {
    op(
        who,
        "work.run",
        "",
        json!({"add": {"command": ["true"], "cwd": "/", "weight": weight}}),
    )
}

#[test]
fn a_run_role_of_supervisor_keeps_a_worker_from_running_or_accepting_a_command() {
    let mut w = world();
    let (alice, bob) = (alice(), bob());
    patch(&mut w, json!({"run_role": "supervisor"}));
    let argv = vec!["true".to_string()];
    let digest = claims::digest(&argv, "/", &[]);
    queue_command(&mut w, &alice, "c", &argv);
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("bob claims");
    let accept = op(&bob, "work.accept", "c", json!({"digest": digest}));
    let err = w.work_accept(NOW, &accept).expect_err("worker accepts");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let run = op(&bob, "work.run", "c", json!({"task": "c"}));
    let err = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect_err("worker runs");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let err = w
        .work_request(NOW, &inline_run(&bob, 1.0), 4.0, "/logs")
        .expect_err("worker runs inline");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    assert!(
        w.work_task("job-1").is_err(),
        "a refused inline run adds no job"
    );
}

#[test]
fn a_supervisor_may_run_and_accept_once_the_run_role_is_supervisor() {
    let mut w = world();
    let (alice, sup) = (alice(), sup());
    patch(&mut w, json!({"run_role": "supervisor"}));
    let argv = vec!["true".to_string()];
    let digest = claims::digest(&argv, "/", &[]);
    queue_command(&mut w, &alice, "c", &argv);
    w.work_claim(NOW, &op(&sup, "work.claim", "c", json!({})))
        .expect("supervisor claims");
    w.work_accept(
        NOW,
        &op(&sup, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("supervisor accepts");
    let run = op(&sup, "work.run", "c", json!({"task": "c"}));
    let requested = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect("supervisor runs");
    assert_eq!(requested.requested_ms, Some(NOW));
}

#[test]
fn switching_self_accept_off_after_an_author_accepted_stops_the_author_running_it() {
    let mut w = world();
    let alice = alice();
    patch(&mut w, json!({"self_accept": "true"}));
    let argv = vec!["true".to_string()];
    let digest = claims::digest(&argv, "/", &[]);
    queue_command(&mut w, &alice, "c", &argv);
    w.work_claim(NOW, &op(&alice, "work.claim", "c", json!({})))
        .expect("alice claims her task");
    w.work_accept(
        NOW,
        &op(&alice, "work.accept", "c", json!({"digest": digest})),
    )
    .expect("alice accepts her own command while allowed");
    patch(&mut w, json!({"self_accept": "false"}));
    let run = op(&alice, "work.run", "c", json!({"task": "c"}));
    let err = w
        .work_request(NOW, &run, 4.0, "/logs")
        .expect_err("author after the flip");
    assert!(
        is_refused(
            &err,
            "a command must be accepted by an identity other than its author"
        ),
        "{err}"
    );
    assert_eq!(w.work_task("c").expect("task").requested_ms, None);
}

#[test]
fn a_waiting_cap_of_two_refuses_a_third_waiting_job_until_one_starts() {
    let mut w = world();
    let (alice, bob) = (alice(), bob());
    patch(&mut w, json!({"max_waiting": "2"}));
    for _ in 0..2 {
        w.work_request(NOW, &inline_run(&alice, 1.0), 4.0, "/logs")
            .expect("a job within the cap");
    }
    let err = w
        .work_request(NOW, &inline_run(&alice, 1.0), 4.0, "/logs")
        .expect_err("third waiting job");
    let by = agora_crypto::pseudonym(&alice.public());
    assert!(
        is_refused(&err, &format!("{by} already has 2 jobs waiting to start")),
        "{err}"
    );
    assert!(w.work_task("job-3").is_err(), "a refused run adds no job");
    let other = w
        .work_request(NOW, &inline_run(&bob, 1.0), 4.0, "/logs")
        .expect("another identity has its own cap");
    assert_eq!(other.requested_ms, Some(NOW));
    w.conn
        .execute("UPDATE work_tasks SET started=?1 WHERE id='job-1'", [NOW])
        .expect("job-1 starts");
    let third = w
        .work_request(NOW, &inline_run(&alice, 1.0), 4.0, "/logs")
        .expect("a started job no longer counts");
    // Ids are global: bob's job took job-3 in between, so alice's job is job-4.
    assert_eq!(third.id, "job-4");
}

#[test]
fn a_weight_below_a_tenth_is_refused_at_add_and_at_an_inline_run() {
    let mut w = world();
    let alice = alice();
    let light = op(
        &alice,
        "work.add",
        "",
        json!({"id": "w1", "scope": "proj", "weight": 0.05}),
    );
    let err = w.work_add(NOW, &light).expect_err("0.05 at add");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "weight must be at least 0.1"),
        "{err}"
    );
    let err = w
        .work_request(NOW, &inline_run(&alice, 0.05), 4.0, "/logs")
        .expect_err("0.05 inline");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "weight must be at least 0.1"),
        "{err}"
    );
    let floor = op(
        &alice,
        "work.add",
        "",
        json!({"id": "w2", "scope": "proj", "weight": 0.1}),
    );
    let task = w.work_add(NOW, &floor).expect("0.1 is allowed");
    assert_eq!(task.weight, 0.1);
}

/// Queues `id` with `files` as `who` (the root directory is the working directory).
fn queue_files(
    w: &mut Store,
    who: &Identity,
    id: &str,
    files: &[&str],
) -> Result<TaskRecord, StoreError> {
    let add = op(
        who,
        "work.add",
        "",
        json!({"id": id, "scope": "proj", "cwd": "/", "files": files}),
    );
    w.work_add(NOW, &add)
}

#[test]
fn a_worker_cannot_claim_the_root_or_a_top_level_directory_but_a_two_level_path_is_fine() {
    let mut w = world();
    let alice = alice();
    // The refusal names the canonical path: "/tmp/../a" is "/a".
    for (id, raw, canonical) in [
        ("r1", "/", "/"),
        ("r2", "/tmp", "/tmp"),
        ("r3", "/tmp/../a", "/a"),
    ] {
        let err = queue_files(&mut w, &alice, id, &[raw]).expect_err("too broad");
        assert!(
            is_refused(&err, &format!("{canonical} is too broad to claim")),
            "{err}"
        );
    }
    let deep = queue_files(&mut w, &alice, "ok", &["/tmp/x", "/tmp/../tmp/y/z"])
        .expect("two components or more");
    assert_eq!(
        deep.files,
        vec!["/tmp/x".to_string(), "/tmp/y/z".to_string()]
    );
}

#[test]
fn a_supervisor_may_claim_the_root_and_a_top_level_directory() {
    let mut w = world();
    let sup = sup();
    let root = queue_files(&mut w, &sup, "s1", &["/"]).expect("root by a supervisor");
    assert_eq!(root.files, vec!["/".to_string()]);
    let top = queue_files(&mut w, &sup, "s2", &["/tmp"]).expect("top level by a supervisor");
    assert_eq!(top.files, vec!["/tmp".to_string()]);
}

/// Queues the command `argv` in the root directory as `who`, with the id `id`.
fn queue_command(w: &mut Store, who: &Identity, id: &str, argv: &[String]) {
    let add = op(
        who,
        "work.add",
        "",
        json!({"id": id, "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add command");
}
