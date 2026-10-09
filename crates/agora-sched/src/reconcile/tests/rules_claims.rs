//! Planner tests for rule R7: claim expiry for work tasks held in `claimed` without a run request.

use super::*;

/// A claim on task `task` by `key(agent)`, made at `claimed_ms`.
fn claim(task: &str, agent: &str, claimed_ms: i64) -> ClaimSnap {
    ClaimSnap {
        task: task.to_owned(),
        agent: key(agent),
        claimed_ms,
        title: format!("Title {task}"),
    }
}

/// An input whose claim ttl is `ttl_s` seconds.
fn with_ttl(ttl_s: f64, claims: Vec<ClaimSnap>) -> ReconcileInput {
    ReconcileInput {
        claims,
        settings: WorkSettings {
            claim_ttl_s: ttl_s,
            ..WorkSettings::default()
        },
        ..input()
    }
}

#[test]
fn r7_claim_not_yet_due_emits_nothing() {
    let i = with_ttl(60.0, vec![claim("t1", "aa", NOW - 59_000)]);
    assert_eq!(plan(&i), Vec::new());
}

#[test]
fn r7_claim_due_at_exactly_its_deadline() {
    let i = with_ttl(60.0, vec![claim("t1", "aa", NOW - 60_000)]);
    assert_eq!(plan(&i).len(), 2);
}

#[test]
fn r7_due_claim_emits_expire_then_notice_with_exact_keys_and_text() {
    let claimed = NOW - 120_000;
    let i = with_ttl(60.0, vec![claim("t1", "aa", claimed)]);
    assert_eq!(
        plan(&i),
        vec![
            ReconcileAction::ExpireClaim {
                key: format!("claim:t1:{claimed}"),
                task: "t1".to_owned(),
                agent: key("aa"),
                claimed_ms: claimed,
            },
            notice(
                &format!("claim-notice:t1:{claimed}"),
                &key("aa"),
                "claim_expired",
                "t1",
                "claim on task t1 (Title t1) expired after 60s without a run request; it is back in the queue",
            ),
        ]
    );
}

#[test]
fn r7_two_due_claims_are_sorted_by_task_id() {
    let claimed = NOW - 120_000;
    let i = with_ttl(
        60.0,
        vec![claim("t2", "bb", claimed), claim("t1", "aa", claimed)],
    );
    let texts: Vec<String> = plan(&i)
        .iter()
        .map(|a| match a {
            ReconcileAction::ExpireClaim { task, .. } => format!("expire {task}"),
            ReconcileAction::Notice { subject, .. } => format!("notice {subject}"),
            other => panic!("unexpected action {other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec!["expire t1", "notice t1", "expire t2", "notice t2"]
    );
}

#[test]
fn r7_marked_claim_key_is_skipped_and_the_notice_keeps_its_own_key() {
    let claimed = NOW - 120_000;
    let i = ReconcileInput {
        marks: vec![mark(&format!("claim:t1:{claimed}"))],
        ..with_ttl(60.0, vec![claim("t1", "aa", claimed)])
    };
    assert_eq!(
        plan(&i),
        vec![notice(
            &format!("claim-notice:t1:{claimed}"),
            &key("aa"),
            "claim_expired",
            "t1",
            "claim on task t1 (Title t1) expired after 60s without a run request; it is back in the queue",
        )]
    );
}

#[test]
fn r7_non_finite_or_non_positive_ttl_turns_the_rule_off() {
    for ttl in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let i = with_ttl(ttl, vec![claim("t1", "aa", NOW - 1_000)]);
        assert_eq!(plan(&i), Vec::new(), "ttl {ttl}");
    }
}

#[test]
fn r7_lease_expiry_comes_before_claim_expiry() {
    let claimed = NOW - 120_000;
    let mut l = lease("l1", LeaseKind::Path);
    l.expires_ms = Some(NOW - 1);
    let i = ReconcileInput {
        leases: vec![l],
        ..with_ttl(60.0, vec![claim("t1", "aa", claimed)])
    };
    let actions = plan(&i);
    assert_eq!(actions[0], expire("l1", NOW - 1));
    assert!(matches!(actions[1], ReconcileAction::ExpireClaim { .. }));
    assert!(matches!(actions[2], ReconcileAction::Notice { .. }));
    assert_eq!(actions.len(), 3);
}
