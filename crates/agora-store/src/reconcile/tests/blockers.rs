//! Tests for the blocker card and the admission pause, applied through `apply_reconcile`.

use super::*;

#[test]
fn blocker_card_is_created_refreshed_and_closed_and_the_next_one_is_numbered_on() {
    let (mut s, sys) = world();
    let open = |key: &str, title: &str, action: &str| ReconcileAction::Blocker {
        key: key.into(),
        open: true,
        title: title.into(),
        owner_action: action.into(),
    };
    assert!(
        apply(
            &mut s,
            &sys,
            &open("blocker:1", "Machine floor breached: disk", "free disk"),
            "k1"
        )
        .expect("create")
    );
    let (state, lane, team, needs, action, owner): (String, String, String, i64, String, Option<String>) = s
        .conn
        .query_row(
            "SELECT state, lane, team, needs_owner, owner_action, owner FROM cards WHERE id = 'blocker-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .expect("blocker card");
    assert_eq!(
        (
            state.as_str(),
            lane.as_str(),
            team.as_str(),
            needs,
            action.as_str(),
            owner
        ),
        ("blocked", "ops", "", 1, "free disk", None)
    );
    assert_eq!(
        state_value(&s.conn, "blocker_card").expect("state"),
        Some("blocker-1".into())
    );

    assert!(
        apply(
            &mut s,
            &sys,
            &open("blocker:2", "Machine floor breached: memory", "free memory"),
            "k2"
        )
        .expect("refresh")
    );
    assert_eq!(
        count(&s, "SELECT COUNT(*) FROM cards WHERE id LIKE 'blocker-%'"),
        1
    );
    let (title, action): (String, String) = s
        .conn
        .query_row(
            "SELECT title, owner_action FROM cards WHERE id = 'blocker-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("refreshed");
    assert_eq!(
        (title.as_str(), action.as_str()),
        ("Machine floor breached: memory", "free memory")
    );

    let close = ReconcileAction::Blocker {
        key: "unblock:blocker-1".into(),
        open: false,
        title: String::new(),
        owner_action: String::new(),
    };
    assert!(apply(&mut s, &sys, &close, "k3").expect("close"));
    let (state, needs): (String, i64) = s
        .conn
        .query_row(
            "SELECT state, needs_owner FROM cards WHERE id = 'blocker-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("closed card");
    assert_eq!((state.as_str(), needs), ("cancelled", 0));
    let evidence: String = s
        .conn
        .query_row(
            "SELECT ref FROM evidence WHERE card = 'blocker-1'",
            [],
            |r| r.get(0),
        )
        .expect("evidence");
    assert_eq!(evidence, "resolved at 2023-11-14T22:13:20Z");
    let moved: String = s
        .conn
        .query_row(
            "SELECT data FROM events WHERE kind = 'card.moved' AND subject = 'blocker-1'",
            [],
            |r| r.get(0),
        )
        .expect("card.moved");
    assert_eq!(moved, r#"{"from":"blocked","to":"cancelled"}"#);
    assert_eq!(state_value(&s.conn, "blocker_card").expect("state"), None);

    assert!(
        apply(
            &mut s,
            &sys,
            &open("blocker:3", "Machine floor breached: disk", "free disk"),
            "k4"
        )
        .expect("reopen")
    );
    assert_eq!(
        state_value(&s.conn, "blocker_card").expect("state"),
        Some("blocker-2".into())
    );
}

#[test]
fn admission_pause_is_recorded_and_resume_clears_it() {
    let (mut s, sys) = world();
    assert_eq!(s.admission_paused().expect("read"), None);
    let pause = ReconcileAction::Admission {
        key: "pause:1".into(),
        paused: Some("disk free 1.0 GiB below the floor of 5.0 GiB".into()),
    };
    assert!(apply(&mut s, &sys, &pause, "p1").expect("pause"));
    assert_eq!(
        s.admission_paused().expect("read"),
        Some("disk free 1.0 GiB below the floor of 5.0 GiB".into())
    );
    let resume = ReconcileAction::Admission {
        key: "resume:1".into(),
        paused: None,
    };
    assert!(apply(&mut s, &sys, &resume, "p2").expect("resume"));
    assert_eq!(s.admission_paused().expect("read"), None);
}
