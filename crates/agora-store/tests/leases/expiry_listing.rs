//! Expiry applied by the system identity, and listing leases by kind, holder and activity.

use super::*;

#[test]
fn a_system_signed_expire_releases_the_lease_at_the_applied_instant() {
    let mut w = world();
    let sys = system(&mut w);
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let applied_at = NOW + 20 * SEC;
    assert!(expire(&mut w.s, &sys, &lease.id, applied_at).expect("system expires"));
    assert_eq!(
        lease_at(&w.s, applied_at, &lease.id).released_ms,
        Some(applied_at)
    );
}

#[test]
fn a_repeated_expire_is_a_no_op_and_keeps_the_first_release_time() {
    let mut w = world();
    let sys = system(&mut w);
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let first = NOW + 20 * SEC;
    assert!(expire(&mut w.s, &sys, &lease.id, first).expect("first apply"));
    assert!(!expire(&mut w.s, &sys, &lease.id, NOW + 30 * SEC).expect("second apply"));
    assert_eq!(
        lease_at(&w.s, NOW + 30 * SEC, &lease.id).released_ms,
        Some(first)
    );
}

#[test]
fn only_the_system_identity_may_sign_an_expire() {
    let mut w = world();
    let _sys = system(&mut w);
    let lease =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let late = NOW + 20 * SEC;
    forbidden(expire(&mut w.s, &w.alice, &lease.id, late), Role::Worker);
    assert_eq!(lease_at(&w.s, late, &lease.id).released_ms, None);
}

// ---- listings ----------------------------------------------------------------------------------------------

#[test]
fn leases_filter_by_kind() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 2.0, "slot");
    let path = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("path lease");
    let pool = acquire_pool(&mut w.s, &w.alice, "gpu", 1.0, None, NOW).expect("pool lease");
    let quiet = open_quiet(&mut w.s, &ctl, NOW + 60 * MIN, NOW).expect("quiet window");
    assert_eq!(
        listed(&w.s, NOW, Some(LeaseKind::Path), None, false),
        vec![path.id]
    );
    assert_eq!(
        listed(&w.s, NOW, Some(LeaseKind::Pool), None, false),
        vec![pool.id]
    );
    assert_eq!(
        listed(&w.s, NOW, Some(LeaseKind::Quiet), None, false),
        vec![quiet.id]
    );
}

#[test]
fn leases_filter_by_holder() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 2.0, "slot");
    let alice_path = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice path");
    let alice_pool = acquire_pool(&mut w.s, &w.alice, "gpu", 1.0, None, NOW).expect("alice pool");
    let bob_path = acquire_path(&mut w.s, &w.bob, "/proj/x", None, NOW).expect("bob path");
    let alice = pk_hex(&w.alice);
    let bob = pk_hex(&w.bob);
    assert_eq!(
        listed(&w.s, NOW, None, Some(alice.as_str()), false),
        vec![alice_path.id, alice_pool.id]
    );
    assert_eq!(
        listed(&w.s, NOW, None, Some(bob.as_str()), false),
        vec![bob_path.id]
    );
}

#[test]
fn active_only_drops_released_and_expired_leases_and_keeps_creation_order() {
    let mut w = world();
    let short = acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("short lease");
    let gone = acquire_path(&mut w.s, &w.bob, "/proj/x", None, NOW).expect("released lease");
    release(&mut w.s, &w.bob, &gone.id, NOW).expect("bob releases");
    let long = acquire_path(&mut w.s, &w.carol, "/proj/y", None, NOW).expect("long lease");
    let later = NOW + 20 * SEC;
    assert_eq!(listed(&w.s, later, None, None, true), vec![long.id.clone()]);
    assert_eq!(
        listed(&w.s, later, None, None, false),
        vec![short.id, gone.id, long.id]
    );
}
