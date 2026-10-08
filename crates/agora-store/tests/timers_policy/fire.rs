//! Firing a timer: enqueued tasks, the grid after a late fire, coalescing, one-shot timers, the no-op cases and the system identity (D11).

use super::*;
use agora_sched::model::TaskState;

// ---- firing ----------------------------------------------------------------------------------------------------

#[test]
fn a_fire_enqueues_a_queued_task_authored_by_the_owner_with_the_timer_origin() {
    let mut f = fixture();
    let due = NOW + MINUTE_MS;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, Some(due)),
        plain_template(),
    )
    .expect("create");
    let task = fire(&mut f.s, &f.sys, due, "nightly", due)
        .expect("fire")
        .expect("a task is enqueued");
    assert_eq!(task.id, "nightly-1");
    assert_eq!(task.author, pk_hex(&f.alice));
    assert_eq!(task.origin.as_deref(), Some("timer:nightly"));
    assert_eq!(task.state, TaskState::Queued);
    assert_eq!(task.title, TITLE);
    assert_eq!(task.queued_ms, due);
}

#[test]
fn a_late_fire_schedules_the_next_grid_point_after_now() {
    let mut f = fixture();
    let anchor = NOW + MINUTE_MS;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, Some(anchor)),
        plain_template(),
    )
    .expect("create");
    // The fire is for the anchor's grid point, but it runs three minutes and seventeen seconds late.
    let now = anchor + 3 * MINUTE_MS + 17_000;
    fire(&mut f.s, &f.sys, now, "nightly", anchor)
        .expect("fire")
        .expect("a task");
    let timer = timer_of(&f.s, &f.alice, "nightly");
    assert_eq!(timer.next_fire_ms, Some(anchor + 4 * MINUTE_MS));
    assert_eq!(timer.last_fire_ms, Some(anchor));
    assert_eq!(timer.fired, 1);
}

#[test]
fn missed_every_fires_during_downtime_coalesce_into_one_task() {
    let mut f = fixture();
    // Five-minute timer; the daemon was down for three hours, which is 36 missed fires.
    let period_s: u64 = 300;
    let period_ms = 300_000;
    let anchor = NOW + period_ms;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "sweep",
        every(period_s, Some(anchor)),
        plain_template(),
    )
    .expect("create");
    let back_up = NOW + 3 * HOUR_MS;
    let task = fire(&mut f.s, &f.sys, back_up, "sweep", anchor)
        .expect("fire")
        .expect("one task for all the missed fires");
    assert_eq!(task.id, "sweep-1");
    // Nothing replays: the next fire is the first grid point after now (anchor + 36 periods is still before now,
    // so the 37th), and a second fire at the same instant enqueues nothing.
    assert_eq!(
        fire(&mut f.s, &f.sys, back_up, "sweep", anchor).expect("second fire"),
        None
    );
    let timer = timer_of(&f.s, &f.alice, "sweep");
    assert_eq!(timer.next_fire_ms, Some(NOW + 37 * period_ms));
    assert_eq!(timer.fired, 1);
    assert!(matches!(
        f.s.work_task("sweep-2"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn an_at_timer_is_disabled_by_its_fire() {
    let mut f = fixture();
    let due = NOW + 1_000;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "once",
        once_at(due),
        plain_template(),
    )
    .expect("create");
    fire(&mut f.s, &f.sys, due, "once", due)
        .expect("fire")
        .expect("a task");
    let timer = timer_of(&f.s, &f.alice, "once");
    assert!(!timer.enabled);
    assert_eq!(timer.next_fire_ms, None);
    assert_eq!(timer.fired, 1);
}

#[test]
fn a_fire_before_the_timer_is_due_enqueues_nothing() {
    let mut f = fixture();
    let due = NOW + MINUTE_MS;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "once",
        once_at(due),
        plain_template(),
    )
    .expect("create");
    assert_eq!(
        fire(&mut f.s, &f.sys, NOW + 1_000, "once", due).expect("early fire"),
        None
    );
    assert!(matches!(
        f.s.work_task("once-1"),
        Err(StoreError::NotFound(_))
    ));
    let timer = timer_of(&f.s, &f.alice, "once");
    assert!(timer.enabled);
    assert_eq!(timer.next_fire_ms, Some(due));
    assert_eq!(timer.fired, 0);
}

#[test]
fn a_disabled_timer_fires_nothing() {
    let mut f = fixture();
    let due = NOW + 1_000;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "once",
        once_at(due),
        plain_template(),
    )
    .expect("create");
    fire(&mut f.s, &f.sys, due, "once", due)
        .expect("first fire")
        .expect("a task");
    assert_eq!(
        fire(&mut f.s, &f.sys, due + HOUR_MS, "once", due).expect("second fire"),
        None
    );
    assert!(matches!(
        f.s.work_task("once-2"),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(timer_of(&f.s, &f.alice, "once").fired, 1);
}

#[test]
fn a_timer_whose_owner_was_revoked_is_disabled_without_a_task() {
    let mut f = fixture();
    let due = NOW + MINUTE_MS;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, Some(due)),
        plain_template(),
    )
    .expect("create");
    f.s.revoke_identity(NOW, &op(&f.owner, "identity.revoke", &pk_hex(&f.alice), ""))
        .expect("revoke alice");
    assert_eq!(
        fire(&mut f.s, &f.sys, due, "nightly", due).expect("fire"),
        None
    );
    let timer = timer_of(&f.s, &f.alice, "nightly");
    assert!(!timer.enabled);
    assert_eq!(timer.next_fire_ms, None);
    assert!(matches!(
        f.s.work_task("nightly-1"),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn a_timer_fire_signed_by_a_worker_is_forbidden() {
    let mut f = fixture();
    let due = NOW + MINUTE_MS;
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, Some(due)),
        plain_template(),
    )
    .expect("create");
    let arg = json!({"fire_ms": due}).to_string();
    let forged = op_at(&f.alice, "timer.fire", "nightly", &arg, due);
    assert_eq!(forbidden(f.s.timer_fire(due, &forged)), Role::Worker);
    assert!(matches!(
        f.s.work_task("nightly-1"),
        Err(StoreError::NotFound(_))
    ));
    assert!(timer_of(&f.s, &f.alice, "nightly").enabled);
}
