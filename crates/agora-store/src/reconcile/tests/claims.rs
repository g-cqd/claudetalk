//! Claim expiry through the reconciler (R7): the snapshot lists the claims, `ExpireClaim` returns one to the queue, and
//! a claim without a command, a requested task, a claim that moved on, a fresh claim and a worker's signature all leave
//! the queue alone.

use super::*;
use crate::reconcile::ReconcileOutcome;
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
pub(super) fn fresh() -> String {
    format!("claim-{}", NONCES.fetch_add(1, Ordering::Relaxed))
}

/// A work operation signed by `who` at `NOW` with a fresh nonce.
fn work_op(who: &Identity, kind: &str, subject: &str, arg: Value) -> SignedOp {
    SignedOp::new(kind, subject, &arg.to_string(), NOW, &fresh()).sign(who)
}

/// The world plus a project scope and two workers: `alice` queues tasks, `bob` claims them.
pub(super) fn queue_world() -> (Store, Identity, Identity, Identity) {
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
pub(super) fn queue(s: &mut Store, who: &Identity, id: &str, command: bool) {
    let mut arg = json!({"id": id, "title": id, "scope": "proj"});
    if command {
        arg["command"] = json!(["touch", "/tmp/x"]);
        arg["cwd"] = json!("/");
    }
    s.work_add(NOW, &work_op(who, "work.add", "", arg))
        .expect("queue");
}

pub(super) fn claim(s: &mut Store, who: &Identity, id: &str) {
    s.work_claim(NOW, &work_op(who, "work.claim", id, json!({})))
        .expect("claim");
}

/// The expiry action for `task`, claimed by `agent` at `claimed_ms`, with its notice.
pub(super) fn expire(task: &str, agent: &Identity, claimed_ms: i64) -> ReconcileAction {
    ReconcileAction::ExpireClaim {
        key: format!("claim:{task}:{claimed_ms}"),
        task: task.into(),
        agent: pk_hex(agent),
        claimed_ms,
        notice_key: format!("claim-notice:{task}:{claimed_ms}"),
        text: format!("claim on task {task} expired"),
    }
}

/// Applies `action`, signed by the system, at time `at` (the operation's timestamp and the store's clock), and
/// reports what it did.
pub(super) fn outcome_at(
    s: &mut Store,
    sys: &Identity,
    action: &ReconcileAction,
    nonce: &str,
    at: i64,
) -> Result<ReconcileOutcome, StoreError> {
    let arg = serde_json::to_string(action).expect("action json");
    let op = SignedOp::new(action.op_kind(), action.key(), &arg, at, nonce).sign(sys);
    s.apply_reconcile_outcome(at, &op)
}

fn apply_at(
    s: &mut Store,
    sys: &Identity,
    action: &ReconcileAction,
    nonce: &str,
    at: i64,
) -> Result<bool, StoreError> {
    outcome_at(s, sys, action, nonce, at).map(|o| o.applied)
}

/// The `claim_expired` notices as (target, subject, text), oldest first.
pub(super) fn claim_notices(s: &Store) -> Vec<(String, String, String)> {
    let mut st = s
        .conn
        .prepare(
            "SELECT target, subject, text FROM notices WHERE kind = 'claim_expired' ORDER BY seq",
        )
        .expect("prepare");
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query");
    rows.collect::<Result<_, _>>().expect("rows")
}

/// State, agent and claim time of work task `id`.
pub(super) fn row(s: &Store, id: &str) -> (String, Option<String>, Option<i64>) {
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

/// Writes a raw `work_settings` row with `claim_ttl_s` set to `ttl_s` (JSON text), as an older build or a hand edit
/// would, so `validate` never sees it.
fn raw_claim_ttl(s: &Store, ttl_s: &str) {
    s.conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, ?1)",
            [format!("{{\"claim_ttl_s\": {ttl_s}}}")],
        )
        .expect("settings row");
}

#[test]
fn a_stored_claim_ttl_below_the_minimum_reads_as_the_minimum() {
    let (s, _sys) = world();
    raw_claim_ttl(&s, "1.0");
    assert_eq!(s.work_settings().expect("read").claim_ttl_s, 60.0);
    // The read does not write back: the row keeps the stored value.
    let stored: String = s
        .conn
        .query_row("SELECT json FROM work_settings WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("row");
    let stored: Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(stored["claim_ttl_s"], 1.0);
}

#[test]
fn a_stored_claim_ttl_of_one_hour_reads_unchanged() {
    let (s, _sys) = world();
    raw_claim_ttl(&s, "3600.0");
    assert_eq!(s.work_settings().expect("read").claim_ttl_s, 3600.0);
}

#[test]
fn a_stored_claim_ttl_below_the_minimum_does_not_expire_a_claim_before_the_minimum() {
    let (mut s, sys, alice, bob) = queue_world();
    raw_claim_ttl(&s, "1.0");
    queue(&mut s, &alice, "young", true);
    queue(&mut s, &alice, "old", true);
    claim(&mut s, &bob, "young");
    claim(&mut s, &bob, "old");
    let young = row(&s, "young");
    // Checked 30 s after the claims: under the 60 s minimum, so the claim stands.
    assert!(
        apply_at(
            &mut s,
            &sys,
            &expire("young", &bob, NOW),
            &fresh(),
            NOW + 30 * SEC
        )
        .is_ok()
    );
    assert_eq!(row(&s, "young"), young);
    // Checked 61 s after the claims: past the minimum, so the claim expires.
    assert!(matches!(
        apply_at(
            &mut s,
            &sys,
            &expire("old", &bob, NOW),
            &fresh(),
            NOW + 61 * SEC
        ),
        Ok(true)
    ));
    assert_eq!(row(&s, "old"), ("queued".to_string(), None, None));
    assert_eq!(row(&s, "young"), young);
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
    let outcome = outcome_at(
        &mut s,
        &sys,
        &expire("t1", &bob, NOW - 1),
        &fresh(),
        NOW + DEFAULT_TTL_MS,
    );
    assert!(!outcome.expect("apply").notice_posted);
    assert_eq!(row(&s, "t1"), before);
    assert!(claim_notices(&s).is_empty());
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

#[test]
fn a_released_claim_posts_one_notice_to_its_claimer_under_its_own_key() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let outcome = outcome_at(
        &mut s,
        &sys,
        &expire("t1", &bob, NOW),
        &fresh(),
        NOW + DEFAULT_TTL_MS,
    );
    assert!(outcome.expect("apply").notice_posted);
    assert_eq!(
        claim_notices(&s),
        vec![(pk_hex(&bob), "t1".into(), "claim on task t1 expired".into())]
    );
    assert_eq!(
        count(
            &s,
            &format!("SELECT COUNT(*) FROM reconcile_marks WHERE key = 'claim-notice:t1:{NOW}'")
        ),
        1
    );
}

#[test]
fn a_repeated_release_posts_no_second_notice() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let action = expire("t1", &bob, NOW);
    let first = outcome_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS);
    assert!(first.expect("first").notice_posted);
    let again = outcome_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS);
    assert_eq!(again.expect("again"), ReconcileOutcome::default());
    assert_eq!(claim_notices(&s).len(), 1);
}
