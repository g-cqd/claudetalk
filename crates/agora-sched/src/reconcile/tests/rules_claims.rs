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

/// The task ids of the expiry actions in `actions`, in order; any other action fails the test.
fn expired_tasks(actions: &[ReconcileAction]) -> Vec<String> {
    actions
        .iter()
        .map(|a| match a {
            ReconcileAction::ExpireClaim { task, .. } => task.clone(),
            other => panic!("unexpected action {other:?}"),
        })
        .collect()
}

#[test]
fn r7_claim_not_yet_due_emits_nothing() {
    let i = with_ttl(60.0, vec![claim("t1", "aa", NOW - 59_000)]);
    assert_eq!(plan(&i), Vec::new());
}

#[test]
fn r7_claim_due_at_exactly_its_deadline() {
    let i = with_ttl(60.0, vec![claim("t1", "aa", NOW - 60_000)]);
    assert_eq!(plan(&i).len(), 1);
}

#[test]
fn r7_due_claim_emits_one_expire_that_carries_its_notice() {
    let claimed = NOW - 120_000;
    let i = with_ttl(60.0, vec![claim("t1", "aa", claimed)]);
    assert_eq!(
        plan(&i),
        vec![ReconcileAction::ExpireClaim {
            key: format!("claim:t1:{claimed}"),
            task: "t1".to_owned(),
            agent: key("aa"),
            claimed_ms: claimed,
            notice_key: format!("claim-notice:t1:{claimed}"),
            text: "claim on task t1 (Title t1) expired after 60s without a run request; it is back in the queue"
                .to_owned(),
        }]
    );
}

#[test]
fn r7_due_claims_are_sorted_oldest_claim_first_then_by_task_id() {
    let claimed = NOW - 120_000;
    let i = with_ttl(
        60.0,
        vec![
            claim("t1", "aa", claimed),
            claim("t3", "cc", claimed),
            claim("t2", "bb", claimed - 60_000),
        ],
    );
    assert_eq!(expired_tasks(&plan(&i)), vec!["t2", "t1", "t3"]);
}

#[test]
fn r7_twenty_due_claims_emit_exactly_the_oldest_sixteen() {
    let claims = (0..20_i64)
        .map(|n| claim(&format!("t{n:02}"), "aa", NOW - 120_000 - n * 1_000))
        .collect();
    let actions = plan(&with_ttl(60.0, claims));
    assert_eq!(actions.len(), MAX_CLAIM_EXPIRIES_PER_PASS);
    assert_eq!(
        expired_tasks(&actions),
        (4..20)
            .rev()
            .map(|n| format!("t{n:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn r7_sixteen_due_claims_all_expire() {
    let claims = (0..16_i64)
        .map(|n| claim(&format!("t{n:02}"), "aa", NOW - 120_000 - n * 1_000))
        .collect();
    let actions = plan(&with_ttl(60.0, claims));
    assert_eq!(actions.len(), MAX_CLAIM_EXPIRIES_PER_PASS);
    assert_eq!(
        expired_tasks(&actions),
        (0..16)
            .rev()
            .map(|n| format!("t{n:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn r7_seventeen_due_claims_leave_out_the_newest() {
    let claims = (0..17_i64)
        .map(|n| claim(&format!("t{n:02}"), "aa", NOW - 120_000 - n * 1_000))
        .collect();
    let tasks = expired_tasks(&plan(&with_ttl(60.0, claims)));
    assert_eq!(tasks.len(), MAX_CLAIM_EXPIRIES_PER_PASS);
    // t00 has the largest claimed_ms of the seventeen, so it is the one left for a later pass.
    assert!(!tasks.iter().any(|t| t == "t00"));
    assert_eq!(
        tasks,
        (1..17)
            .rev()
            .map(|n| format!("t{n:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn r7_marked_claims_do_not_use_up_the_cap() {
    let claims: Vec<ClaimSnap> = (0..17_i64)
        .map(|n| claim(&format!("t{n:02}"), "aa", NOW - 120_000 - n * 1_000))
        .collect();
    // The oldest claim (t16) is already marked, so it is skipped and the sixteen after it still fit the cap.
    let marked = format!("claim:t16:{}", NOW - 120_000 - 16 * 1_000);
    let i = ReconcileInput {
        marks: vec![mark(&marked)],
        ..with_ttl(60.0, claims)
    };
    let tasks = expired_tasks(&plan(&i));
    assert_eq!(tasks.len(), MAX_CLAIM_EXPIRIES_PER_PASS);
    assert_eq!(tasks.first().map(String::as_str), Some("t15"));
}

#[test]
fn r7_marked_claim_key_is_skipped() {
    let claimed = NOW - 120_000;
    let i = ReconcileInput {
        marks: vec![mark(&format!("claim:t1:{claimed}"))],
        ..with_ttl(60.0, vec![claim("t1", "aa", claimed)])
    };
    assert_eq!(plan(&i), Vec::new());
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
    assert_eq!(actions.len(), 2);
}
