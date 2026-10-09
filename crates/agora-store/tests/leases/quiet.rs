//! Quiet windows: controller-only, one open at a time, and closable by the holder or a controller.

use super::*;

#[test]
fn only_a_controller_may_open_a_quiet_window() {
    let mut w = world();
    let end = NOW + 60 * MIN;
    forbidden(open_quiet(&mut w.s, &w.alice, end, NOW), Role::Worker);
    forbidden(open_quiet(&mut w.s, &w.sup, end, NOW), Role::Supervisor);
    assert!(listed(&w.s, NOW, Some(LeaseKind::Quiet), None, false).is_empty());
}

#[test]
fn a_controller_opens_a_quiet_window_that_has_no_expiry() {
    let mut w = world();
    let ctl = controller(&mut w);
    let end = NOW + 60 * MIN;
    let quiet = open_quiet(&mut w.s, &ctl, end, NOW).expect("controller opens");
    assert_eq!(
        quiet,
        LeaseRecord {
            id: quiet.id.clone(),
            kind: LeaseKind::Quiet,
            resource: "machine".into(),
            amount: 0.0,
            holder: pk_hex(&ctl),
            holder_name: pseudonym(&ctl.public()),
            card: None,
            task: None,
            created_ms: NOW,
            expires_ms: None,
            heartbeat_ms: NOW,
            expected_end_ms: Some(end),
            released_ms: None,
            note: "tests".into(),
        }
    );
}

#[test]
fn only_one_quiet_window_is_open_at_a_time() {
    let mut w = world();
    let ctl = controller(&mut w);
    let end = NOW + 60 * MIN;
    let first = open_quiet(&mut w.s, &ctl, end, NOW).expect("first window opens");
    invalid(
        open_quiet(&mut w.s, &ctl, end, NOW),
        "a quiet window is already open",
    );
    close_quiet(&mut w.s, &ctl, &first.id, NOW + SEC).expect("holder closes it");
    open_quiet(&mut w.s, &ctl, end, NOW + 2 * SEC)
        .expect("a new window may open once the first is closed");
}

#[test]
fn a_quiet_window_must_end_after_now() {
    let mut w = world();
    let ctl = controller(&mut w);
    invalid(
        open_quiet(&mut w.s, &ctl, NOW, NOW),
        "quiet window must end after now",
    );
}

#[test]
fn a_quiet_window_stays_open_past_its_expected_end_until_it_is_closed() {
    let mut w = world();
    let ctl = controller(&mut w);
    let quiet = open_quiet(&mut w.s, &ctl, NOW + 60 * MIN, NOW).expect("controller opens");
    let much_later = NOW + 5 * 60 * MIN;
    assert_eq!(
        listed(&w.s, much_later, Some(LeaseKind::Quiet), None, true),
        vec![quiet.id]
    );
}

#[test]
fn the_holder_closes_its_own_quiet_window() {
    let mut w = world();
    let ctl = controller(&mut w);
    let quiet = open_quiet(&mut w.s, &ctl, NOW + 60 * MIN, NOW).expect("controller opens");
    close_quiet(&mut w.s, &ctl, &quiet.id, NOW + SEC).expect("holder closes");
    assert_eq!(
        lease_at(&w.s, NOW + SEC, &quiet.id).released_ms,
        Some(NOW + SEC)
    );
}

#[test]
fn another_controller_may_close_a_quiet_window() {
    let mut w = world();
    let ctl = controller(&mut w);
    let quiet = open_quiet(&mut w.s, &ctl, NOW + 60 * MIN, NOW).expect("controller opens");
    close_quiet(&mut w.s, &w.owner, &quiet.id, NOW + SEC)
        .expect("the owner, a controller+, closes");
    assert_eq!(
        lease_at(&w.s, NOW + SEC, &quiet.id).released_ms,
        Some(NOW + SEC)
    );
}

#[test]
fn a_supervisor_or_worker_may_not_close_someone_elses_quiet_window() {
    let mut w = world();
    let ctl = controller(&mut w);
    let quiet = open_quiet(&mut w.s, &ctl, NOW + 60 * MIN, NOW).expect("controller opens");
    forbidden(
        close_quiet(&mut w.s, &w.sup, &quiet.id, NOW + SEC),
        Role::Supervisor,
    );
    forbidden(
        close_quiet(&mut w.s, &w.alice, &quiet.id, NOW + SEC),
        Role::Worker,
    );
    assert_eq!(lease_at(&w.s, NOW + SEC, &quiet.id).released_ms, None);
}

// ---- expiry through the system identity --------------------------------------------------------------------
