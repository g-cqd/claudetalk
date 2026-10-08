//! Timer deletion rights and the listing and due-time queries (D11).

use super::*;

// ---- deletion and listing --------------------------------------------------------------------------------------

#[test]
fn the_owner_deletes_their_own_timer() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, None),
        plain_template(),
    )
    .expect("create");
    delete(&mut f.s, &f.alice, NOW, "nightly").expect("owner delete");
    assert!(f.s.timers(None).expect("timers").is_empty());
}

#[test]
fn another_worker_cannot_delete_a_timer_and_it_is_kept() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, None),
        plain_template(),
    )
    .expect("create");
    let err = delete(&mut f.s, &f.bob, NOW, "nightly");
    assert_eq!(forbidden(err), Role::Worker);
    assert_eq!(f.s.timers(None).expect("timers").len(), 1);
}

#[test]
fn a_supervisor_deletes_another_workers_timer() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, None),
        plain_template(),
    )
    .expect("create");
    delete(&mut f.s, &f.sup, NOW, "nightly").expect("supervisor delete");
    assert!(f.s.timers(None).expect("timers").is_empty());
}

#[test]
fn timers_lists_only_the_owners_timers_oldest_first() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "a1",
        every(60, None),
        plain_template(),
    )
    .expect("a1");
    create(
        &mut f.s,
        &f.bob,
        NOW,
        "b1",
        every(60, None),
        plain_template(),
    )
    .expect("b1");
    create(
        &mut f.s,
        &f.alice,
        NOW + 1_000,
        "a2",
        every(60, None),
        plain_template(),
    )
    .expect("a2");
    let alice = pk_hex(&f.alice);
    assert_eq!(
        ids(f.s.timers(Some(alice.as_str())).expect("alice's timers")),
        ["a1", "a2"]
    );
    assert_eq!(
        ids(f.s.timers(None).expect("all timers")),
        ["a1", "b1", "a2"]
    );
}

#[test]
fn next_timer_due_is_the_earliest_next_fire_of_an_enabled_timer() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "once",
        once_at(NOW + 2_000),
        plain_template(),
    )
    .expect("create once");
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "steady",
        every(60, Some(NOW + 5_000)),
        plain_template(),
    )
    .expect("create steady");
    assert_eq!(f.s.next_timer_due().expect("due"), Some(NOW + 2_000));
    // Firing the one-shot disables it, so the steady timer's fire becomes the next due.
    fire(&mut f.s, &f.sys, NOW + 2_000, "once", NOW + 2_000).expect("fire once");
    assert_eq!(f.s.next_timer_due().expect("due"), Some(NOW + 5_000));
}

#[test]
fn due_timers_lists_only_timers_due_by_now_in_fire_order() {
    let mut f = fixture();
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "steady",
        every(60, Some(NOW + 2_000)),
        plain_template(),
    )
    .expect("create steady");
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "once",
        once_at(NOW + 5_000),
        plain_template(),
    )
    .expect("create once");
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "later",
        once_at(NOW + 9_000),
        plain_template(),
    )
    .expect("create later");
    assert!(f.s.due_timers(NOW + 1_999).expect("due").is_empty());
    assert_eq!(
        ids(f.s.due_timers(NOW + 5_000).expect("due")),
        ["steady", "once"]
    );
}
