//! The seven-day cap on lease TTLs and the cap of 64 active path and pool leases per holder.

use super::*;

const DAY: i64 = 86_400_000;

/// Takes `n` path leases `/cap/0`, `/cap/1`, ... for `who`, all with the same TTL.
fn fill_paths(s: &mut Store, who: &Identity, n: usize, ttl_s: Option<f64>, now: i64) {
    for i in 0..n {
        acquire_path(s, who, &format!("/cap/{i}"), ttl_s, now).expect("under the cap");
    }
}

fn assert_seven_day_cap<T: std::fmt::Debug>(result: Result<T, StoreError>) {
    match result {
        Err(StoreError::Invalid(message)) => assert_eq!(message, "a lease lasts at most 7 days"),
        other => panic!("expected the seven-day cap, got {other:?}"),
    }
}

fn assert_lease_cap<T: std::fmt::Debug>(result: Result<T, StoreError>, who: &Identity) {
    let expected = format!(
        "{} already holds 64 active leases",
        pseudonym(&who.public())
    );
    match result {
        Err(StoreError::Refused(message)) => assert_eq!(message, expected),
        other => panic!("expected the lease cap, got {other:?}"),
    }
}

#[test]
fn a_ttl_of_exactly_seven_days_is_accepted() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/week", Some(604_800.0), NOW)
        .expect("seven days is the cap");
    assert_eq!(lease.expires_ms, Some(NOW + 7 * DAY));
}

#[test]
fn a_ttl_above_seven_days_is_refused_on_acquire_and_writes_nothing() {
    let mut f = fixture();
    assert_seven_day_cap(acquire_path(
        &mut f.s,
        &f.alice,
        "/proj/long",
        Some(604_800.5),
        NOW,
    ));
    assert_seven_day_cap(acquire_path(
        &mut f.s,
        &f.alice,
        "/proj/huge",
        Some(1.0e300),
        NOW,
    ));
    assert!(
        f.s.leases(NOW, None, None, false)
            .expect("leases")
            .is_empty()
    );
}

#[test]
fn a_renewal_may_not_extend_past_seven_days() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/r", Some(60.0), NOW).expect("alice");
    let too_long = r#"{"ttl_s":604800.5}"#;
    assert_seven_day_cap(f.s.lease_renew(
        NOW + 1_000,
        &op(&f.alice, "lease.renew", &lease.id, too_long, NOW + 1_000),
    ));
    let unchanged = f.s.leases(NOW + 1_000, None, None, true).expect("leases");
    assert_eq!(unchanged[0].expires_ms, Some(NOW + MIN));
    let exact = r#"{"ttl_s":604800}"#;
    let renewed =
        f.s.lease_renew(
            NOW + 1_000,
            &op(&f.alice, "lease.renew", &lease.id, exact, NOW + 1_000),
        )
        .expect("seven days renews");
    assert_eq!(renewed.expires_ms, Some(NOW + 1_000 + 7 * DAY));
}

#[test]
fn a_settings_default_above_seven_days_applies_to_acquire_and_renew_without_a_ttl() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/d", Some(60.0), NOW).expect("alice");
    f.s.conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, '{\"lease_ttl_s\": 700000.0}')",
            [],
        )
        .expect("settings row");
    assert_seven_day_cap(acquire_path(&mut f.s, &f.alice, "/proj/e", None, NOW));
    assert_seven_day_cap(f.s.lease_renew(
        NOW + 1_000,
        &op(&f.alice, "lease.renew", &lease.id, "", NOW + 1_000),
    ));
}

#[test]
fn a_holder_may_hold_64_active_path_leases_and_the_65th_is_refused_per_holder() {
    let mut f = fixture();
    fill_paths(&mut f.s, &f.alice, 64, None, NOW);
    let alice_key = key(&f.alice);
    assert_eq!(
        f.s.leases(NOW, None, Some(alice_key.as_str()), true)
            .expect("leases")
            .len(),
        64
    );
    let refused = acquire_path(&mut f.s, &f.alice, "/cap/64", None, NOW);
    assert_lease_cap(refused, &f.alice);
    assert_eq!(
        f.s.leases(NOW, None, Some(alice_key.as_str()), true)
            .expect("leases")
            .len(),
        64,
        "a refused acquire writes nothing"
    );
    acquire_path(&mut f.s, &f.bob, "/cap/64", None, NOW).expect("the cap is per holder");
}

#[test]
fn pool_leases_count_toward_the_64_active_leases() {
    let mut f = fixture();
    define_pool(&mut f.s, &f.ctl, "gpu", 100.0, "");
    acquire_pool(&mut f.s, &f.alice, "gpu", 1.0, NOW).expect("one pool lease");
    fill_paths(&mut f.s, &f.alice, 63, None, NOW);
    assert_lease_cap(
        acquire_path(&mut f.s, &f.alice, "/cap/63", None, NOW),
        &f.alice,
    );
    assert_lease_cap(acquire_pool(&mut f.s, &f.alice, "gpu", 1.0, NOW), &f.alice);
}

#[test]
fn quiet_windows_do_not_count_toward_the_64_active_leases() {
    let mut f = fixture();
    open_quiet(&mut f.s, &f.ctl, NOW + MIN, NOW).expect("quiet window");
    fill_paths(&mut f.s, &f.ctl, 64, None, NOW);
    assert_lease_cap(acquire_path(&mut f.s, &f.ctl, "/cap/64", None, NOW), &f.ctl);
}

#[test]
fn releasing_one_lease_frees_a_slot_under_the_cap() {
    let mut f = fixture();
    fill_paths(&mut f.s, &f.alice, 64, None, NOW);
    let alice_key = key(&f.alice);
    let first =
        f.s.leases(NOW, None, Some(alice_key.as_str()), true)
            .expect("leases")[0]
            .id
            .clone();
    f.s.lease_release(
        NOW + 1_000,
        &op(&f.alice, "lease.release", &first, "", NOW + 1_000),
    )
    .expect("release");
    acquire_path(&mut f.s, &f.alice, "/cap/64", None, NOW + 1_000).expect("one slot is free again");
}

#[test]
fn expired_leases_free_their_slots_at_the_expiry_instant() {
    let mut f = fixture();
    fill_paths(&mut f.s, &f.alice, 64, Some(60.0), NOW);
    assert_lease_cap(
        acquire_path(&mut f.s, &f.alice, "/cap/64", None, NOW + MIN - 1),
        &f.alice,
    );
    acquire_path(&mut f.s, &f.alice, "/cap/64", None, NOW + MIN)
        .expect("every lease has expired at its expiry instant");
}
