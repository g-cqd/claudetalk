//! Claim expiry through the reconciler (R7): the snapshot lists the claims, `ExpireClaim` returns one to the queue, and
//! a claim without a command, a requested task, a claim that moved on, a fresh claim and a worker's signature all leave
//! the queue alone.

use super::*;
use agora_core::{Role, TransitionError};
use agora_sched::reconcile::ClaimSnap;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// D5 digest of argv `["touch","/tmp/x"]` in `/` with no files (the same command the work tests use).
const TOUCH_DIGEST: &str = "059ca237bc078272521caa7a46631c6869a704487b8fed20619a2d58b43e7323";
const LOG_DIR: &str = "/var/agora/logs";
/// The default claim TTL: 1800 seconds.
const DEFAULT_TTL_MS: i64 = 1800 * SEC;

static NONCES: AtomicU64 = AtomicU64::new(0);

/// A nonce that no call in this process produces twice.
fn fresh() -> String {
    format!("claim-{}", NONCES.fetch_add(1, Ordering::Relaxed))
}

/// A work operation signed by `who` at `NOW` with a fresh nonce.
fn work_op(who: &Identity, kind: &str, subject: &str, arg: Value) -> SignedOp {
    SignedOp::new(kind, subject, &arg.to_string(), NOW, &fresh()).sign(who)
}

/// The world plus a project scope and two workers: `alice` queues tasks, `bob` claims them.
fn queue_world() -> (Store, Identity, Identity, Identity) {
    let (mut s, sys) = world();
    let alice = person(5);
    let bob = person(6);
    enroll(&mut s, &alice, "worker", false);
    enroll(&mut s, &bob, "worker", false);
    s.work_scope_set(
        NOW,
        &work_op(&alice, "work.scope", "proj", json!({"id": "proj"})),
    )
    .expect("scope");
    (s, sys, alice, bob)
}

/// Queues task `id` in the project scope as `who`, with the touch command when `command` is set.
fn queue(s: &mut Store, who: &Identity, id: &str, command: bool) {
    let mut arg = json!({"id": id, "title": id, "scope": "proj"});
    if command {
        arg["command"] = json!(["touch", "/tmp/x"]);
        arg["cwd"] = json!("/");
    }
    s.work_add(NOW, &work_op(who, "work.add", "", arg))
        .expect("queue");
}

fn claim(s: &mut Store, who: &Identity, id: &str) {
    s.work_claim(NOW, &work_op(who, "work.claim", id, json!({})))
        .expect("claim");
}

/// The expiry action for `task`, claimed by `agent` at `claimed_ms`.
fn expire(task: &str, agent: &Identity, claimed_ms: i64) -> ReconcileAction {
    ReconcileAction::ExpireClaim {
        key: format!("claim:{task}:{claimed_ms}"),
        task: task.into(),
        agent: pk_hex(agent),
        claimed_ms,
    }
}

/// Applies `action`, signed by the system, at time `at` (the operation's timestamp and the store's clock).
fn apply_at(
    s: &mut Store,
    sys: &Identity,
    action: &ReconcileAction,
    nonce: &str,
    at: i64,
) -> Result<bool, StoreError> {
    let arg = serde_json::to_string(action).expect("action json");
    let op = SignedOp::new(action.op_kind(), action.key(), &arg, at, nonce).sign(sys);
    s.apply_reconcile(at, &op)
}

/// State, agent and claim time of work task `id`.
fn row(s: &Store, id: &str) -> (String, Option<String>, Option<i64>) {
    s.conn
        .query_row(
            "SELECT state, agent, claimed FROM work_tasks WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("task row")
}

#[test]
fn a_claimed_task_is_listed_in_the_snapshot() {
    let (mut s, _sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let input = s.reconcile_input(NOW, None, false).expect("input");
    assert_eq!(
        input.claims,
        vec![ClaimSnap {
            task: "t1".into(),
            agent: pk_hex(&bob),
            claimed_ms: NOW,
            title: "t1".into(),
        }]
    );
}

#[test]
fn a_claim_on_a_task_without_a_command_is_not_listed_in_the_snapshot() {
    let (mut s, _sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", false);
    claim(&mut s, &bob, "t1");
    assert!(
        s.reconcile_input(NOW, None, false)
            .expect("input")
            .claims
            .is_empty()
    );
}

#[test]
fn expiry_returns_the_task_to_the_queue_and_writes_one_reclaim_event() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    // Exactly one TTL after the claim: the boundary counts as expired.
    assert!(matches!(
        apply_at(
            &mut s,
            &sys,
            &expire("t1", &bob, NOW),
            &fresh(),
            NOW + DEFAULT_TTL_MS
        ),
        Ok(true)
    ));
    assert_eq!(row(&s, "t1"), ("queued".to_string(), None, None));
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed' AND subject = 't1'"
        ),
        1
    );
    let data: String = s
        .conn
        .query_row(
            "SELECT data FROM events WHERE kind = 'work.reclaimed'",
            [],
            |r| r.get(0),
        )
        .expect("event");
    let data: Value = serde_json::from_str(&data).expect("event json");
    assert_eq!(data["note"], "claim expired");
    assert_eq!(data["scope"], "proj");
    assert_eq!(data["detail"], "claim expired");
    assert!(
        s.reconcile_input(NOW, None, false)
            .expect("input")
            .claims
            .is_empty()
    );
}

#[test]
fn a_command_less_claim_is_not_expired() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", false);
    claim(&mut s, &bob, "t1");
    let before = row(&s, "t1");
    // Long past the TTL, but a task without a command is closed by its claimant, never requeued by expiry.
    assert!(
        apply_at(
            &mut s,
            &sys,
            &expire("t1", &bob, NOW),
            &fresh(),
            NOW + 10 * DEFAULT_TTL_MS
        )
        .is_ok()
    );
    assert_eq!(row(&s, "t1"), before);
    assert_eq!(
        row(&s, "t1"),
        ("claimed".to_string(), Some(pk_hex(&bob)), Some(NOW))
    );
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed'"
        ),
        0
    );
}

#[test]
fn a_claim_younger_than_the_ttl_is_not_expired_even_by_a_signed_action() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let before = row(&s, "t1");
    // The system signed the expiry, but the claim is one millisecond short of its TTL.
    assert!(
        apply_at(
            &mut s,
            &sys,
            &expire("t1", &bob, NOW),
            &fresh(),
            NOW + DEFAULT_TTL_MS - 1
        )
        .is_ok()
    );
    assert_eq!(row(&s, "t1"), before);
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed'"
        ),
        0
    );
}

#[test]
fn the_ttl_comes_from_the_work_settings() {
    let (mut s, sys, alice, bob) = queue_world();
    s.conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, '{\"claim_ttl_s\": 60.0}')",
            [],
        )
        .expect("settings row");
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    assert!(matches!(
        apply_at(
            &mut s,
            &sys,
            &expire("t1", &bob, NOW),
            &fresh(),
            NOW + 60 * SEC
        ),
        Ok(true)
    ));
    assert_eq!(row(&s, "t1"), ("queued".to_string(), None, None));
}

#[test]
fn a_repeated_expiry_is_a_no_op() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let action = expire("t1", &bob, NOW);
    assert!(matches!(
        apply_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS),
        Ok(true)
    ));
    let after_first = row(&s, "t1");
    assert!(matches!(
        apply_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS),
        Ok(false)
    ));
    assert_eq!(row(&s, "t1"), after_first);
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed'"
        ),
        1
    );
}

#[test]
fn a_claim_made_at_another_time_is_left_alone() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let before = row(&s, "t1");
    // Old enough by the clock, so only the claim time differs.
    assert!(
        apply_at(
            &mut s,
            &sys,
            &expire("t1", &bob, NOW - 1),
            &fresh(),
            NOW + DEFAULT_TTL_MS
        )
        .is_ok()
    );
    assert_eq!(row(&s, "t1"), before);
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed'"
        ),
        0
    );
}

#[test]
fn a_requested_task_is_neither_listed_nor_reverted() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    s.work_accept(
        NOW,
        &work_op(&bob, "work.accept", "t1", json!({"digest": TOUCH_DIGEST})),
    )
    .expect("accept");
    s.work_request(
        NOW,
        &work_op(&bob, "work.run", "", json!({"task": "t1"})),
        4.0,
        LOG_DIR,
    )
    .expect("run request");
    let before = row(&s, "t1");
    assert!(
        s.reconcile_input(NOW, None, false)
            .expect("input")
            .claims
            .is_empty()
    );
    assert!(apply(&mut s, &sys, &expire("t1", &bob, NOW), &fresh()).is_ok());
    assert_eq!(row(&s, "t1"), before);
    assert_eq!(
        count(
            &s,
            "SELECT COUNT(*) FROM events WHERE kind = 'work.reclaimed'"
        ),
        0
    );
}

#[test]
fn a_worker_cannot_sign_a_claim_expiry() {
    let (mut s, _sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", false);
    claim(&mut s, &bob, "t1");
    let before = row(&s, "t1");
    let got = apply(&mut s, &bob, &expire("t1", &bob, NOW), &fresh());
    assert!(
        matches!(
            got,
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Worker
            )))
        ),
        "{got:?}"
    );
    assert_eq!(row(&s, "t1"), before);
}
