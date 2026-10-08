//! Unit tests of the agent operations on the work queue, on an in-memory store.

use crate::identity::hex;
use crate::{SignedOp, Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::claims;
use agora_sched::model::TaskState;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

const NOW: i64 = 1_700_000_000_000;
static NONCES: AtomicU64 = AtomicU64::new(0);

/// A signed operation whose argument is `arg` as JSON.
fn op(id: &Identity, kind: &str, subject: &str, arg: Value) -> SignedOp {
    op_raw(id, kind, subject, &arg.to_string())
}

/// A signed operation whose argument is the raw string `arg` (for `role.grant`).
fn op_raw(id: &Identity, kind: &str, subject: &str, arg: &str) -> SignedOp {
    let nonce = format!("w{}", NONCES.fetch_add(1, Ordering::Relaxed));
    SignedOp::new(kind, subject, arg, NOW, &nonce).sign(id)
}

fn pk(id: &Identity) -> String {
    hex(&id.public())
}

fn ident(seed: u8) -> Identity {
    Identity::from_seed([seed; 32])
}

fn owner() -> Identity {
    ident(1)
}

fn sup() -> Identity {
    ident(2)
}

fn alice() -> Identity {
    ident(3)
}

fn bob() -> Identity {
    ident(4)
}

fn ctl() -> Identity {
    ident(5)
}

fn eve() -> Identity {
    ident(6)
}

fn olive() -> Identity {
    ident(8)
}

/// Owner bootstrapped; sup is a supervisor, ctl a controller, olive an observer; alice and bob are workers; eve is
/// never registered. Scope `proj` exists.
fn world() -> Store {
    let mut s = Store::open_in_memory().expect("store");
    let owner = owner();
    s.bootstrap_owner(NOW, &owner.public()).expect("owner");
    for (id, label) in [
        (sup(), "sup"),
        (ctl(), "ctl"),
        (alice(), "alice"),
        (bob(), "bob"),
        (olive(), "olive"),
    ] {
        s.register_identity(NOW, &id.public(), label)
            .expect("register");
    }
    for (id, role) in [
        (sup(), "supervisor"),
        (ctl(), "controller"),
        (olive(), "observer"),
    ] {
        s.grant_role(NOW, &op_raw(&owner, "role.grant", &pk(&id), role))
            .expect("grant");
    }
    let scope = op(
        &alice(),
        "work.scope",
        "proj",
        json!({"id": "proj", "kind": "project"}),
    );
    s.work_scope_set(NOW, &scope).expect("scope");
    s
}

fn is_refused(err: &StoreError, message: &str) -> bool {
    matches!(err, StoreError::Refused(m) if m == message)
}

fn is_forbidden(err: &StoreError, role: Role) -> bool {
    matches!(err, StoreError::Transition(TransitionError::Forbidden(r)) if *r == role)
}

/// Queues `id` in scope `proj` as `who`.
fn queue(w: &mut Store, who: &Identity, id: &str) {
    let add = op(
        who,
        "work.add",
        "",
        json!({"id": id, "title": id, "scope": "proj"}),
    );
    w.work_add(NOW, &add).expect("add");
}

#[test]
fn add_claim_and_finish_a_plain_task() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    queue(&mut w, &alice, "t1");
    let claim = op(&bob, "work.claim", "t1", json!({}));
    let claimed = w.work_claim(NOW, &claim).expect("claim");
    assert_eq!(claimed.state, TaskState::Claimed);
    assert_eq!(claimed.agent, Some(pk(&bob)));
    let finish = op(
        &bob,
        "work.finish",
        "t1",
        json!({"state": "done", "note": "ok"}),
    );
    let done = w.work_finish(NOW, &finish).expect("finish");
    assert_eq!(done.state, TaskState::Done);
    assert_eq!(done.note, "ok");
    let again = w
        .work_claim(NOW, &op(&bob, "work.claim", "t1", json!({})))
        .expect_err("finished task");
    assert!(is_refused(&again, "task t1 is done"), "{again}");
}

#[test]
fn add_refuses_duplicates_unknown_scopes_bad_kinds_and_malformed_args() {
    let mut w = world();
    let alice = alice();
    queue(&mut w, &alice, "t1");
    let dup = op(&alice, "work.add", "", json!({"id": "t1", "scope": "proj"}));
    let err = w.work_add(NOW, &dup).expect_err("duplicate");
    assert!(is_refused(&err, "task t1 exists already"), "{err}");
    let missing = op(&alice, "work.add", "", json!({"id": "t2", "scope": "nope"}));
    let err = w.work_add(NOW, &missing).expect_err("unknown scope");
    assert!(
        is_refused(
            &err,
            "unknown scope nope; create it with `agora work scope set nope --kind project`"
        ),
        "{err}"
    );
    let kind = op(
        &alice,
        "work.add",
        "",
        json!({"id": "t3", "kind": "bad kind"}),
    );
    let err = w.work_add(NOW, &kind).expect_err("bad kind");
    assert!(matches!(err, StoreError::Refused(_)), "{err}");
    let garbage = op(&alice, "work.add", "", json!("not an object"));
    let err = w.work_add(NOW, &garbage).expect_err("malformed");
    assert!(
        matches!(err, StoreError::Invalid("malformed arguments")),
        "{err}"
    );
}

#[test]
fn observer_is_refused_for_every_mutation() {
    let mut w = world();
    let olive = olive();
    let add = op(&olive, "work.add", "", json!({"id": "t1", "scope": "proj"}));
    let err = w.work_add(NOW, &add).expect_err("observer add");
    assert!(is_forbidden(&err, Role::Observer), "{err}");
    let scope = op(&olive, "work.scope", "x", json!({"id": "x"}));
    let err = w.work_scope_set(NOW, &scope).expect_err("observer scope");
    assert!(is_forbidden(&err, Role::Observer), "{err}");
}

#[test]
fn a_worker_cannot_cancel_or_update_another_identitys_task_but_a_supervisor_can() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let sup = sup();
    queue(&mut w, &alice, "t1");
    let cancel = op(&bob, "work.cancel", "t1", json!({"note": "mine"}));
    let err = w.work_cancel(NOW, &cancel).expect_err("bob cancels");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let update = op(&bob, "work.update", "t1", json!({"title": "stolen"}));
    let err = w.work_update(NOW, &update).expect_err("bob updates");
    assert!(is_forbidden(&err, Role::Worker), "{err}");
    let edit = op(
        &sup,
        "work.update",
        "t1",
        json!({"title": "reviewed", "size": 3.0}),
    );
    let edited = w.work_update(NOW, &edit).expect("supervisor updates");
    assert_eq!(edited.title, "reviewed");
    let cancel = op(&sup, "work.cancel", "t1", json!({"note": "superseded"}));
    let outcome = w.work_cancel(NOW, &cancel).expect("supervisor cancels");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
    assert!(outcome.stop.is_none());
}

#[test]
fn next_claims_the_first_ready_task_and_reports_why_it_cannot() {
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
    let next = |w: &mut Store, scope: &str| {
        let args = json!({"scope": scope, "policy": "fifo"});
        let who = ident(3);
        w.work_next(NOW, &op(&who, "work.next", "", args))
            .expect("next")
    };
    let first = next(&mut w, "proj");
    assert_eq!(first.claimed.map(|t| t.id), Some("a".to_string()));
    assert_eq!(first.reason, "");
    let blocked = next(&mut w, "proj");
    assert!(blocked.claimed.is_none());
    assert_eq!(
        blocked.reason,
        "of 1 queued task(s) in scope proj, 1 wait for their prerequisites and 0 change files that claimed or \
         running tasks hold"
    );
    let unscoped = next(&mut w, "");
    assert_eq!(
        unscoped.reason,
        "of 1 queued task(s), 1 wait for their prerequisites and 0 change files that claimed or running tasks hold"
    );
}

#[test]
fn next_reports_nothing_queued_in_the_scope() {
    let mut w = world();
    let alice = alice();
    let args = json!({"scope": "proj", "policy": "fifo"});
    let outcome = w
        .work_next(NOW, &op(&alice, "work.next", "", args))
        .expect("next");
    assert!(outcome.claimed.is_none());
    assert_eq!(outcome.reason, "nothing is queued in scope proj");
}

#[test]
fn claim_refuses_an_unfinished_prerequisite_with_its_state() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    queue(&mut w, &alice, "a");
    let b = op(
        &alice,
        "work.add",
        "",
        json!({"id": "b", "scope": "proj", "after": ["a"]}),
    );
    w.work_add(NOW, &b).expect("add b");
    let err = w
        .work_claim(NOW, &op(&bob, "work.claim", "b", json!({})))
        .expect_err("b waits");
    assert!(is_refused(&err, "task b waits for a (queued)"), "{err}");
}

#[test]
fn held_files_block_a_claim_of_a_conflicting_task() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let first = op(
        &alice,
        "work.add",
        "",
        json!({"id": "a", "scope": "proj", "files": ["/w/src"], "cwd": "/"}),
    );
    w.work_add(NOW, &first).expect("add a");
    let second = op(
        &alice,
        "work.add",
        "",
        json!({"id": "b", "scope": "proj", "files": ["/w/src/lib.rs"], "cwd": "/"}),
    );
    w.work_add(NOW, &second).expect("add b");
    w.work_claim(NOW, &op(&bob, "work.claim", "a", json!({})))
        .expect("bob claims a");
    let err = w
        .work_claim(NOW, &op(&alice, "work.claim", "b", json!({})))
        .expect_err("b conflicts with a");
    let text = err.to_string();
    assert!(
        text.starts_with("task b changes files that task a holds ("),
        "{text}"
    );
    let args = json!({"scope": "proj", "policy": "fifo"});
    let outcome = w
        .work_next(NOW, &op(&alice, "work.next", "", args))
        .expect("next");
    assert!(outcome.claimed.is_none());
    assert_eq!(
        outcome.reason,
        "of 1 queued task(s) in scope proj, 0 wait for their prerequisites and 1 change files that claimed or running \
         tasks hold"
    );
}

#[test]
fn self_accept_is_refused_by_default_and_allowed_after_the_controller_sets_it() {
    let mut w = world();
    let alice = alice();
    let argv = vec!["echo".to_string(), "hi".to_string()];
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add c");
    let good = claims::digest(&argv, "/", &[]);
    w.work_claim(NOW, &op(&alice, "work.claim", "c", json!({})))
        .expect("alice claims her own task");
    let own = op(&alice, "work.accept", "c", json!({"digest": good}));
    let err = w.work_accept(NOW, &own).expect_err("self accept");
    assert!(
        matches!(err, StoreError::Invalid(m) if m == "a command must be accepted by an identity other than its author"),
        "{err}"
    );
    let ctl = ctl();
    let patch = json!({"patch": {"self_accept": "true"}});
    w.work_settings_set(NOW, &op(&ctl, "work.settings", "", patch))
        .expect("controller allows self accept");
    let (task, first) = w.work_accept(NOW, &own).expect("self accept allowed");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk(&alice)));
}

#[test]
fn accept_by_another_identity_needs_the_exact_digest_and_a_repeat_is_not_first() {
    let mut w = world();
    let alice = alice();
    let bob = bob();
    let argv = vec!["make".to_string(), "test".to_string()];
    let add = op(
        &alice,
        "work.add",
        "",
        json!({"id": "c", "scope": "proj", "command": argv, "cwd": "/"}),
    );
    w.work_add(NOW, &add).expect("add c");
    w.work_claim(NOW, &op(&bob, "work.claim", "c", json!({})))
        .expect("bob claims");
    let wrong = op(&bob, "work.accept", "c", json!({"digest": "0".repeat(64)}));
    let err = w.work_accept(NOW, &wrong).expect_err("wrong digest");
    assert!(
        err.to_string()
            .starts_with("the digest does not match the command of task c"),
        "{err}"
    );
    let good = claims::digest(&argv, "/", &[]);
    let upper = op(
        &bob,
        "work.accept",
        "c",
        json!({"digest": good.to_uppercase()}),
    );
    let (_, first) = w.work_accept(NOW, &upper).expect("accept with the digest");
    assert!(first);
    let repeat = op(&bob, "work.accept", "c", json!({"digest": good}));
    let (_, again) = w.work_accept(NOW, &repeat).expect("repeat");
    assert!(!again);
}

#[cfg(test)]
mod guards;
#[cfg(test)]
mod run;
