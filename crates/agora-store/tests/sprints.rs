//! Public-API tests for sprints: the planned, active and closed lifecycle, the
//! one-active-sprint rule, card membership, malformed create args, supervisor-only
//! management, and nonce handling for failed and replayed operations.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_store::*;

const WINDOW: &str = r#"{"name":"Sprint 1","goal":"ship","starts":0,"ends":100}"#;

/// Creates a planned sprint as the supervisor.
fn plan_sprint(w: &mut World, id: &str) {
    w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", id, WINDOW))
        .unwrap();
}

fn start(w: &mut World, id: &str) {
    w.s.start_sprint(NOW, &op(&w.sup, "sprint.start", id, ""))
        .unwrap();
}

fn close(w: &mut World, id: &str) {
    w.s.close_sprint(NOW, &op(&w.sup, "sprint.close", id, ""))
        .unwrap();
}

fn add(w: &mut World, sprint: &str, card: &str) {
    w.s.add_to_sprint(NOW, &op(&w.sup, "sprint.add", sprint, card))
        .unwrap();
}

#[test]
fn sprint_summary_counts_cards_by_their_state() {
    let mut w = world();
    make_card(&mut w, "c1");
    make_card(&mut w, "c2");
    make_card(&mut w, "c3");
    plan_sprint(&mut w, "s1");
    add(&mut w, "s1", "c1");
    add(&mut w, "s1", "c2");
    add(&mut w, "s1", "c3");
    start(&mut w, "s1");
    // In flight: claimed by alice.
    w.s.transition(NOW, &op(&w.sup, "card.move", "c1", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c1", "claimed"), false)
        .unwrap();
    // Blocked: the supervisor may take it straight out of backlog.
    w.s.transition(NOW, &op(&w.sup, "card.move", "c2", "blocked"), false)
        .unwrap();
    // Done: full path with evidence, no review gate.
    w.s.transition(NOW, &op(&w.sup, "card.move", "c3", "ready"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c3", "claimed"), false)
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c3", "doing"), false)
        .unwrap();
    w.s.add_evidence(NOW, &op(&w.alice, "card.evidence", "c3", "run.log"))
        .unwrap();
    w.s.transition(NOW, &op(&w.alice, "card.move", "c3", "done"), false)
        .unwrap();
    assert_eq!(
        w.s.sprint_summary("s1").unwrap(),
        SprintSummary {
            total: 3,
            done: 1,
            in_flight: 1,
            blocked: 1
        }
    );
}

#[test]
fn only_one_sprint_is_active_at_a_time() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    plan_sprint(&mut w, "s2");
    start(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.start_sprint(NOW, &op(&w.sup, "sprint.start", "s2", ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("another sprint is already active")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);

    close(&mut w, "s1");
    start(&mut w, "s2");
}

#[test]
fn a_sprint_starts_once_and_never_restarts_after_closing() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    start(&mut w, "s1");
    let err =
        w.s.start_sprint(NOW, &op(&w.sup, "sprint.start", "s1", ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint is not planned")),
        "got {err:?}"
    );

    close(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.start_sprint(NOW, &op(&w.sup, "sprint.start", "s1", ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint is not planned")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn only_an_active_sprint_can_be_closed() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.close_sprint(NOW, &op(&w.sup, "sprint.close", "s1", ""))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint is not active")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn closed_sprint_refuses_new_cards() {
    let mut w = world();
    make_card(&mut w, "c1");
    make_card(&mut w, "c2");
    plan_sprint(&mut w, "s1");
    add(&mut w, "s1", "c1");
    start(&mut w, "s1");
    close(&mut w, "s1");

    let before = w.s.event_count().unwrap();
    let err =
        w.s.add_to_sprint(NOW, &op(&w.sup, "sprint.add", "s1", "c2"))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint is closed")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
    assert_eq!(
        w.s.sprint_summary("s1").unwrap(),
        SprintSummary {
            total: 1,
            ..Default::default()
        }
    );
}

#[test]
fn create_rejects_a_window_that_does_not_end_after_it_starts() {
    let mut w = world();
    let before = w.s.event_count().unwrap();
    for arg in [
        r#"{"name":"S","starts":5,"ends":5}"#,
        r#"{"name":"S","starts":5,"ends":4}"#,
    ] {
        let err =
            w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", "s1", arg))
                .unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid("sprint must end after it starts")),
            "{arg}: got {err:?}"
        );
    }
    assert_eq!(w.s.event_count().unwrap(), before);

    // One unit later is the smallest valid window.
    w.s.create_sprint(
        NOW,
        &op(
            &w.sup,
            "sprint.create",
            "s1",
            r#"{"name":"S","starts":5,"ends":6}"#,
        ),
    )
    .unwrap();
}

#[test]
fn create_rejects_malformed_arguments_without_an_event() {
    let mut w = world();
    let before = w.s.event_count().unwrap();
    for (arg, msg, why) in [
        ("not json", "sprint arg must be JSON", "malformed JSON"),
        (
            r#"{"goal":"ship","starts":0,"ends":100}"#,
            "sprint name must be a non-empty string",
            "missing name",
        ),
        (
            r#"{"name":"","starts":0,"ends":100}"#,
            "sprint name must be a non-empty string",
            "empty name",
        ),
        (
            r#"{"name":"S","starts":"0","ends":100}"#,
            "sprint starts must be an integer",
            "non-integer starts",
        ),
        (
            r#"{"name":"S","starts":0,"ends":1.5}"#,
            "sprint ends must be an integer",
            "non-integer ends",
        ),
    ] {
        let err =
            w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", "s1", arg))
                .unwrap_err();
        assert!(
            matches!(err, StoreError::Invalid(m) if m == msg),
            "{why}: got {err:?}"
        );
        assert_eq!(
            w.s.event_count().unwrap(),
            before,
            "{why} appended an event"
        );
    }

    // Nothing was stored, so the id is still free for a valid create.
    w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", "s1", WINDOW))
        .unwrap();
}

#[test]
fn duplicate_sprint_id_is_rejected() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", "s1", WINDOW))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint already exists")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn create_rejects_an_empty_sprint_id() {
    let mut w = world();
    let before = w.s.event_count().unwrap();
    let err =
        w.s.create_sprint(NOW, &op(&w.sup, "sprint.create", "", WINDOW))
            .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("sprint id must not be empty")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn only_a_supervisor_manages_sprints() {
    let mut w = world();
    make_card(&mut w, "c1");
    plan_sprint(&mut w, "s1");
    let before = w.s.event_count().unwrap();

    let err =
        w.s.create_sprint(NOW, &op(&w.alice, "sprint.create", "s2", WINDOW))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    let err =
        w.s.add_to_sprint(NOW, &op(&w.alice, "sprint.add", "s1", "c1"))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    let err =
        w.s.start_sprint(NOW, &op(&w.alice, "sprint.start", "s1", ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.event_count().unwrap(), before);

    start(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.close_sprint(NOW, &op(&w.alice, "sprint.close", "s1", ""))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn add_rejects_an_unknown_card() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.add_to_sprint(NOW, &op(&w.sup, "sprint.add", "s1", "missing"))
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn add_rejects_an_unknown_sprint() {
    let mut w = world();
    make_card(&mut w, "c1");
    let before = w.s.event_count().unwrap();
    let err =
        w.s.add_to_sprint(NOW, &op(&w.sup, "sprint.add", "missing", "c1"))
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn start_unknown_sprint_is_not_found() {
    let mut w = world();
    let before = w.s.event_count().unwrap();
    let err =
        w.s.start_sprint(NOW, &op(&w.sup, "sprint.start", "ghost", ""))
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(w.s.event_count().unwrap(), before);
}

#[test]
fn sprint_summary_of_an_unknown_sprint_is_not_found() {
    let w = world();
    let err = w.s.sprint_summary("ghost").unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref id) if id == "ghost"));
}

#[test]
fn adding_the_same_card_twice_counts_it_once() {
    let mut w = world();
    make_card(&mut w, "c1");
    plan_sprint(&mut w, "s1");
    add(&mut w, "s1", "c1");
    let before = w.s.event_count().unwrap();
    w.s.add_to_sprint(NOW, &op(&w.sup, "sprint.add", "s1", "c1"))
        .unwrap();
    assert_eq!(
        w.s.event_count().unwrap(),
        before,
        "re-adding a member appended an event"
    );
    assert_eq!(
        w.s.sprint_summary("s1").unwrap(),
        SprintSummary {
            total: 1,
            ..Default::default()
        }
    );
}

#[test]
fn failed_start_does_not_burn_its_nonce() {
    let mut w = world();
    plan_sprint(&mut w, "s1");
    plan_sprint(&mut w, "s2");
    start(&mut w, "s1");

    let retry = op(&w.sup, "sprint.start", "s2", "");
    let err = w.s.start_sprint(NOW, &retry).unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("another sprint is already active")),
        "got {err:?}"
    );

    close(&mut w, "s1");
    w.s.start_sprint(NOW, &retry).unwrap();
}

#[test]
fn replayed_create_is_rejected() {
    let mut w = world();
    let create = op(&w.sup, "sprint.create", "s1", WINDOW);
    w.s.create_sprint(NOW, &create).unwrap();
    let before = w.s.event_count().unwrap();
    let err = w.s.create_sprint(NOW, &create).unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid("replayed operation")),
        "got {err:?}"
    );
    assert_eq!(w.s.event_count().unwrap(), before);
}
