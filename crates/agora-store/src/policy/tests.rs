//! Unit tests for team and lane policies, notices and post_notice.

use super::*;
use crate::identity::hex;
use agora_core::TransitionError;
use agora_crypto::Identity;
use std::sync::atomic::{AtomicU64, Ordering};

const NOW: i64 = 1_700_000_000_000;

/// Signed at `now`. The nonce is the kind and a process-wide counter, so it is short (well under
/// the 128-byte cap) and unique for every op a test signs.
fn signed(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = format!("{kind}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    SignedOp::new(kind, subject, arg, now, &nonce).sign(id)
}

/// The owner, who grants roles, and one key per role under test.
struct Cast {
    owner: Identity,
    supervisor: Identity,
    controller: Identity,
    worker: Identity,
}

fn cast_store() -> (Store, Cast) {
    let mut s = Store::open_in_memory().unwrap();
    let c = Cast {
        owner: Identity::from_seed([1; 32]),
        supervisor: Identity::from_seed([2; 32]),
        controller: Identity::from_seed([3; 32]),
        worker: Identity::from_seed([4; 32]),
    };
    s.bootstrap_owner(NOW, &c.owner.public()).unwrap();
    for (id, role) in [(&c.supervisor, "supervisor"), (&c.controller, "controller")] {
        s.register_identity(NOW, &id.public(), role).unwrap();
        let op = signed(&c.owner, "role.grant", &hex(&id.public()), role, NOW);
        s.grant_role(NOW, &op).unwrap();
    }
    s.register_identity(NOW, &c.worker.public(), "worker")
        .unwrap();
    (s, c)
}

/// A 64-character lowercase hex key derived from `n`.
fn key(n: u64) -> String {
    format!("{n:064x}")
}

fn stale(secs: f64) -> TeamPolicy {
    TeamPolicy {
        stale_after_s: Some(secs),
        ..TeamPolicy::default()
    }
}

/// Submits a `policy.set` with an explicit subject and arg.
fn submit(
    s: &mut Store,
    by: &Identity,
    subject: &str,
    arg: &str,
    now: i64,
) -> Result<PolicyRecord, StoreError> {
    s.policy_set(now, &signed(by, "policy.set", subject, arg, now))
}

/// Sets `team`/`lane` with a well-formed arg.
fn set(
    s: &mut Store,
    by: &Identity,
    team: &str,
    lane: &str,
    policy: &TeamPolicy,
    now: i64,
) -> Result<PolicyRecord, StoreError> {
    let arg = serde_json::to_string(&PolicyArgs {
        team: team.into(),
        lane: lane.into(),
        policy: policy.clone(),
    })
    .unwrap();
    submit(s, by, &format!("{team}/{lane}"), &arg, now)
}

fn is_forbidden(err: &StoreError, role: Role) -> bool {
    matches!(err, StoreError::Transition(TransitionError::Forbidden(r)) if *r == role)
}

fn post_one(s: &mut Store, target: &str, text: &str) -> i64 {
    let tx = s.conn.transaction().unwrap();
    let seq = post_notice(&tx, NOW, target, "kind", "subject", text).unwrap();
    tx.commit().unwrap();
    seq
}

#[test]
fn worker_is_refused_and_nothing_is_written() {
    let (mut s, c) = cast_store();
    let err = set(&mut s, &c.worker, "t", "l", &stale(60.0), NOW).unwrap_err();
    assert!(is_forbidden(&err, Role::Worker), "{err:?}");
    assert!(s.policies().unwrap().is_empty());
}

#[test]
fn supervisor_sets_a_policy_and_gets_the_record_back() {
    let (mut s, c) = cast_store();
    let rec = set(&mut s, &c.supervisor, "t", "l", &stale(60.0), NOW).unwrap();
    assert_eq!(rec.team, "t");
    assert_eq!(rec.lane, "l");
    assert_eq!(rec.policy, stale(60.0));
    assert_eq!(rec.updated_ms, NOW);
    assert_eq!(rec.by, hex(&c.supervisor.public()));
    assert_eq!(s.policies().unwrap(), vec![rec]);
}

#[test]
fn escalation_chain_needs_a_controller() {
    let (mut s, c) = cast_store();
    let chain = TeamPolicy {
        escalation_chain: Some(vec![key(7)]),
        ..TeamPolicy::default()
    };
    let err = set(&mut s, &c.supervisor, "t", "l", &chain, NOW).unwrap_err();
    assert!(is_forbidden(&err, Role::Supervisor), "{err:?}");
    assert!(s.policies().unwrap().is_empty());
    set(&mut s, &c.controller, "t", "l", &chain, NOW + 1).unwrap();
    assert_eq!(
        s.policies().unwrap()[0].policy.escalation_chain,
        Some(vec![key(7)])
    );
}

#[test]
fn supervisor_cannot_clear_a_stored_chain() {
    let (mut s, c) = cast_store();
    let chain = TeamPolicy {
        escalation_chain: Some(vec![key(7)]),
        ..TeamPolicy::default()
    };
    set(&mut s, &c.controller, "t", "l", &chain, NOW).unwrap();
    let err = set(&mut s, &c.supervisor, "t", "l", &stale(60.0), NOW + 1).unwrap_err();
    assert!(is_forbidden(&err, Role::Supervisor), "{err:?}");
    assert_eq!(
        s.policies().unwrap()[0].policy.escalation_chain,
        Some(vec![key(7)])
    );
    // Where no chain is stored, a supervisor may set a policy without one.
    set(&mut s, &c.supervisor, "t", "other", &stale(60.0), NOW + 2).unwrap();
}

#[test]
fn subject_parts_must_be_wildcards_or_valid_names() {
    let (mut s, c) = cast_store();
    let arg = r#"{"team":"t","lane":"l","policy":{}}"#;
    let missing_slash = submit(&mut s, &c.supervisor, "tl", arg, NOW).unwrap_err();
    assert!(
        matches!(missing_slash, StoreError::Invalid(_)),
        "{missing_slash:?}"
    );
    for subject in ["t/bad name", "t/l/extra", "/l", "t/"] {
        let err = submit(&mut s, &c.supervisor, subject, arg, NOW).unwrap_err();
        assert!(matches!(err, StoreError::Refused(_)), "{subject}: {err:?}");
    }
    assert!(s.policies().unwrap().is_empty());
    let wildcard = r#"{"team":"*","lane":"*","policy":{}}"#;
    submit(&mut s, &c.supervisor, "*/*", wildcard, NOW).unwrap();
}

#[test]
fn arg_team_and_lane_must_equal_the_subject() {
    let (mut s, c) = cast_store();
    let arg = r#"{"team":"u","lane":"l","policy":{}}"#;
    let err = submit(&mut s, &c.supervisor, "t/l", arg, NOW).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Invalid("policy arg team and lane must match the subject")
        ),
        "{err:?}"
    );
    let malformed = submit(&mut s, &c.supervisor, "t/l", "{", NOW + 1).unwrap_err();
    assert!(matches!(malformed, StoreError::Invalid(_)), "{malformed:?}");
    assert!(s.policies().unwrap().is_empty());
}

#[test]
fn invalid_policy_is_refused_with_the_validation_message() {
    let (mut s, c) = cast_store();
    let zero_wip = TeamPolicy {
        wip_limit: Some(0),
        ..TeamPolicy::default()
    };
    let err = set(&mut s, &c.supervisor, "t", "l", &zero_wip, NOW).unwrap_err();
    assert!(
        matches!(&err, StoreError::Refused(m) if m == "wip_limit must be at least 1"),
        "{err:?}"
    );
    let bad_key = TeamPolicy {
        escalation_chain: Some(vec!["ABC".into()]),
        ..TeamPolicy::default()
    };
    let err = set(&mut s, &c.controller, "t", "l", &bad_key, NOW + 1).unwrap_err();
    assert!(
        matches!(&err, StoreError::Refused(m) if m.contains("lowercase hex")),
        "{err:?}"
    );
    assert!(s.policies().unwrap().is_empty());
}

#[test]
fn a_second_set_replaces_the_whole_row() {
    let (mut s, c) = cast_store();
    let first = TeamPolicy {
        stale_after_s: Some(60.0),
        wip_limit: Some(2),
        ..TeamPolicy::default()
    };
    set(&mut s, &c.supervisor, "t", "l", &first, NOW).unwrap();
    let second = stale(90.0);
    set(&mut s, &c.controller, "t", "l", &second, NOW + 5).unwrap();
    let rows = s.policies().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].policy, second,
        "wip_limit is unset by the second set"
    );
    assert_eq!(rows[0].updated_ms, NOW + 5);
    assert_eq!(rows[0].by, hex(&c.controller.public()));
}

#[test]
fn policies_are_listed_by_team_then_lane() {
    let (mut s, c) = cast_store();
    for (team, lane) in [("b", "a"), ("a", "z"), ("a", "b")] {
        set(&mut s, &c.supervisor, team, lane, &stale(60.0), NOW).unwrap();
    }
    let order: Vec<(String, String)> = s
        .policies()
        .unwrap()
        .into_iter()
        .map(|p| (p.team, p.lane))
        .collect();
    let expected =
        [("a", "b"), ("a", "z"), ("b", "a")].map(|(t, l)| (t.to_string(), l.to_string()));
    assert_eq!(order, expected);
}

#[test]
fn set_appends_a_policy_set_event_for_the_subject() {
    let (mut s, c) = cast_store();
    set(&mut s, &c.supervisor, "t", "l", &stale(60.0), NOW).unwrap();
    let count: i64 = s
        .conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='policy.set' AND subject='t/l'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn effective_policy_layers_the_stored_rows() {
    let (mut s, c) = cast_store();
    let global = TeamPolicy {
        stale_after_s: Some(100.0),
        reclaim_after_s: Some(100.0),
        ..TeamPolicy::default()
    };
    let team = TeamPolicy {
        escalate_after_s: Some(200.0),
        ..TeamPolicy::default()
    };
    let lane = TeamPolicy {
        stale_after_s: Some(300.0),
        ..TeamPolicy::default()
    };
    let team_lane = TeamPolicy {
        reclaim: Some(false),
        ..TeamPolicy::default()
    };
    set(&mut s, &c.supervisor, "*", "*", &global, NOW).unwrap();
    set(&mut s, &c.supervisor, "t", "*", &team, NOW + 1).unwrap();
    set(&mut s, &c.supervisor, "*", "l", &lane, NOW + 2).unwrap();
    set(&mut s, &c.supervisor, "t", "l", &team_lane, NOW + 3).unwrap();

    let tl = s.effective_policy("t", "l", true).unwrap();
    assert_eq!(tl.stale_after_s, 300.0, "lane beats global");
    assert_eq!(tl.escalate_after_s, 200.0, "team layer applies");
    assert_eq!(
        tl.reclaim_after_s, 100.0,
        "global applies when nothing overrides"
    );
    assert!(!tl.reclaim, "team+lane layer applies last");
    assert!(tl.require_review, "the default passes through");

    let t_other = s.effective_policy("t", "other", false).unwrap();
    assert_eq!(t_other.stale_after_s, 100.0, "no lane row for other");
    assert_eq!(t_other.escalate_after_s, 200.0);
    assert!(t_other.reclaim);
    assert!(!t_other.require_review);

    let u_l = s.effective_policy("u", "l", true).unwrap();
    assert_eq!(u_l.stale_after_s, 300.0, "lane row applies to any team");
    assert_eq!(u_l.escalate_after_s, 900.0, "no team row: default");
}

#[test]
fn effective_policy_with_a_wildcard_part_reads_each_layer_once() {
    let (mut s, c) = cast_store();
    let global = stale(100.0);
    let team = TeamPolicy {
        stale_after_s: Some(200.0),
        wip_limit: Some(4),
        ..TeamPolicy::default()
    };
    set(&mut s, &c.supervisor, "*", "*", &global, NOW).unwrap();
    set(&mut s, &c.supervisor, "t", "*", &team, NOW + 1).unwrap();
    let got = s.effective_policy("t", "*", true).unwrap();
    assert_eq!(got, effective(&[&global, &team], true));
    assert_eq!(got.stale_after_s, 200.0);
}

#[test]
fn effective_policy_without_rows_is_the_defaults() {
    let (s, _c) = cast_store();
    assert_eq!(
        s.effective_policy("t", "l", false).unwrap(),
        effective(&[], false)
    );
}

#[test]
fn corrupt_stored_policy_is_an_error_not_a_default() {
    let (s, _c) = cast_store();
    s.conn
        .execute(
            "INSERT INTO policies(team, lane, json, updated, by) VALUES ('t','l','{bad',1,'x')",
            [],
        )
        .unwrap();
    assert!(matches!(s.policies(), Err(StoreError::Invalid(_))));
    assert!(matches!(
        s.effective_policy("t", "l", true),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn notices_page_by_seq_for_one_target_only() {
    let mut s = Store::open_in_memory().unwrap();
    let a1 = post_one(&mut s, "aa", "one");
    let b1 = post_one(&mut s, "bb", "other target");
    let a2 = post_one(&mut s, "aa", "two");
    let a3 = post_one(&mut s, "aa", "three");
    assert!(a1 < b1 && b1 < a2 && a2 < a3);

    let first = s.notices("aa", 0, 2).unwrap();
    let texts: Vec<&str> = first.iter().map(|n| n.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    assert_eq!(first[0].seq, a1);
    assert_eq!(first[0].target, "aa");
    assert_eq!(first[0].kind, "kind");
    assert_eq!(first[0].subject, "subject");
    assert_eq!(first[0].ts_ms, NOW);

    let rest = s.notices("aa", a2, 10).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].seq, a3);
    assert!(s.notices("aa", a3, 10).unwrap().is_empty());
    assert!(s.notices("cc", 0, 10).unwrap().is_empty());
}

#[test]
fn notices_limit_is_capped_at_500() {
    let mut s = Store::open_in_memory().unwrap();
    for i in 0..501 {
        post_one(&mut s, "aa", &format!("n{i}"));
    }
    assert_eq!(s.notices("aa", 0, u32::MAX).unwrap().len(), 500);
    assert_eq!(s.notices("aa", 0, 3).unwrap().len(), 3);
}

#[test]
fn post_notice_keeps_text_up_to_the_cap_intact() {
    let mut s = Store::open_in_memory().unwrap();
    let exact = "y".repeat(NOTICE_TEXT_MAX_CHARS);
    let seq = post_one(&mut s, "aa", &exact);
    let got = s.notices("aa", seq - 1, 1).unwrap();
    assert_eq!(got[0].text, exact);
}

#[test]
fn post_notice_cuts_long_text_at_2000_characters() {
    let mut s = Store::open_in_memory().unwrap();
    post_one(&mut s, "aa", &"x".repeat(2500));
    let multibyte = "é".repeat(2001);
    post_one(&mut s, "aa", &multibyte);
    let got = s.notices("aa", 0, 10).unwrap();
    assert_eq!(got[0].text.chars().count(), NOTICE_TEXT_MAX_CHARS);
    assert_eq!(got[1].text, "é".repeat(NOTICE_TEXT_MAX_CHARS));
}
