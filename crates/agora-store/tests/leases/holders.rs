//! Renewal and release of leases: by the holder, by a supervisor, and refused for anyone else.

use super::*;

#[test]
fn renewing_by_the_holder_extends_the_expiry_and_moves_the_heartbeat() {
    let mut w = world();
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let later = NOW + 5 * SEC;
    let renewed = renew(&mut w.s, &w.alice, &lease.id, Some(60.0), later).expect("holder renews");
    assert_eq!(renewed.expires_ms, Some(NOW + 65 * SEC));
    assert_eq!(renewed.heartbeat_ms, later);
    assert_eq!(renewed.released_ms, None);
}

#[test]
fn only_the_holder_may_renew_and_a_refused_renewal_keeps_the_expiry() {
    let mut w = world();
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let later = NOW + 5 * SEC;
    forbidden(
        renew(&mut w.s, &w.bob, &lease.id, Some(60.0), later),
        Role::Worker,
    );
    forbidden(
        renew(&mut w.s, &w.sup, &lease.id, Some(60.0), later),
        Role::Supervisor,
    );
    assert_eq!(
        lease_at(&w.s, later, &lease.id).expires_ms,
        Some(NOW + 10 * SEC)
    );
}

#[test]
fn renewing_an_expired_lease_is_refused() {
    let mut w = world();
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    invalid(
        renew(&mut w.s, &w.alice, &lease.id, None, NOW + 10 * SEC),
        "lease is not active",
    );
}

#[test]
fn releasing_by_the_holder_ends_the_lease_and_frees_the_path() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    let later = NOW + SEC;
    release(&mut w.s, &w.alice, &lease.id, later).expect("holder releases");
    assert_eq!(lease_at(&w.s, later, &lease.id).released_ms, Some(later));
    acquire_path(&mut w.s, &w.bob, "/proj/w", None, later).expect("the path is free after release");
}

#[test]
fn a_supervisor_may_release_another_identitys_lease() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    let later = NOW + SEC;
    release(&mut w.s, &w.sup, &lease.id, later).expect("supervisor releases");
    assert_eq!(lease_at(&w.s, later, &lease.id).released_ms, Some(later));
}

#[test]
fn releasing_another_workers_lease_is_forbidden_and_keeps_it_active() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    let later = NOW + SEC;
    forbidden(release(&mut w.s, &w.bob, &lease.id, later), Role::Worker);
    assert_eq!(lease_at(&w.s, later, &lease.id).released_ms, None);
}

#[test]
fn releasing_a_lease_twice_is_refused() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    release(&mut w.s, &w.alice, &lease.id, NOW + SEC).expect("first release");
    invalid(
        release(&mut w.s, &w.alice, &lease.id, NOW + 2 * SEC),
        "lease is already released",
    );
}

// ---- quiet windows -----------------------------------------------------------------------------------------
