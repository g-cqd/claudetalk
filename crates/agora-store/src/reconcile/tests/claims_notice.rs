//! Claim expiry notices (R7) for old-shape actions, and the work settings read when the stored row is corrupt.
//! Old-shape actions come from the pre-0.2.0 planner: they carry no notice key and no text.

use super::claims::{claim, claim_notices, fresh, outcome_at, queue, queue_world, row};
use super::*;
use crate::reconcile::ReconcileOutcome;

/// The default claim TTL: 1800 seconds.
const DEFAULT_TTL_MS: i64 = 1800 * SEC;

/// An expiry for `task` as the pre-0.2.0 planner wrote it: the notice key and the text are empty.
fn old_shape_expiry(task: &str, agent: &Identity, claimed_ms: i64) -> ReconcileAction {
    ReconcileAction::ExpireClaim {
        key: format!("claim:{task}:{claimed_ms}"),
        task: task.into(),
        agent: pk_hex(agent),
        claimed_ms,
        notice_key: String::new(),
        text: String::new(),
    }
}

#[test]
fn an_old_shape_expiry_releases_the_claim_and_posts_no_notice() {
    let (mut s, sys, alice, bob) = queue_world();
    queue(&mut s, &alice, "t1", true);
    claim(&mut s, &bob, "t1");
    let action = old_shape_expiry("t1", &bob, NOW);
    let first = outcome_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS);
    let first = first.expect("first application");
    assert!(first.applied);
    assert!(!first.notice_posted);
    assert_eq!(row(&s, "t1"), ("queued".to_string(), None, None));
    assert!(claim_notices(&s).is_empty());
    assert_eq!(count(&s, "SELECT COUNT(*) FROM notices"), 0);
    // The key is marked by the release, so a second application is a quiet no-op.
    let second = outcome_at(&mut s, &sys, &action, &fresh(), NOW + DEFAULT_TTL_MS);
    assert_eq!(
        second.expect("second application"),
        ReconcileOutcome::default()
    );
    assert_eq!(count(&s, "SELECT COUNT(*) FROM notices"), 0);
}

#[test]
fn a_corrupt_settings_row_is_reported_as_invalid() {
    let (s, _sys) = world();
    s.conn
        .execute("INSERT INTO work_settings(id, json) VALUES (1, '{')", [])
        .expect("settings row");
    let got = s.work_settings().map(|_| ());
    match got {
        Err(StoreError::Invalid(msg)) => assert_eq!(msg, "corrupt work settings in database"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}
