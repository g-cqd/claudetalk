//! Pool tests: defining and redefining a pool, and sharing its capacity between holders.

use super::*;

#[test]
fn pool_define_is_refused_for_supervisors_and_workers_and_defines_nothing() {
    let mut w = world();
    let arg = r#"{"name":"gpu","capacity":2}"#;
    forbidden(
        w.s.pool_define(NOW, &op(&w.alice, "pool.define", "gpu", arg)),
        Role::Worker,
    );
    forbidden(
        w.s.pool_define(NOW, &op(&w.sup, "pool.define", "gpu", arg)),
        Role::Supervisor,
    );
    assert!(w.s.pools(NOW).expect("pools").is_empty());
}

#[test]
fn controller_defines_a_pool_that_starts_with_nothing_used() {
    let mut w = world();
    let ctl = controller(&mut w);
    let pool = define_pool(&mut w.s, &ctl, "gpu", 2.0, "slot");
    assert_eq!(
        pool,
        PoolRecord {
            name: "gpu".into(),
            capacity: 2.0,
            unit: "slot".into(),
            used: 0.0,
            note: String::new(),
            created_ms: NOW,
            created_by: pk_hex(&ctl),
        }
    );
    assert_eq!(w.s.pools(NOW).expect("pools"), vec![pool]);
}

#[test]
fn redefining_a_pool_changes_its_capacity_and_keeps_the_holds_already_taken() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 2.0, "slot");
    acquire_pool(&mut w.s, &w.alice, "gpu", 2.0, None, NOW).expect("alice takes both");
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    acquire_pool(&mut w.s, &w.bob, "gpu", 1.0, None, NOW)
        .expect("the grown pool has room for one more");
    let pools = w.s.pools(NOW).expect("pools");
    assert_eq!(
        pools
            .iter()
            .map(|p| (p.capacity, p.used))
            .collect::<Vec<_>>(),
        vec![(3.0, 3.0)]
    );
}

#[test]
fn a_pool_lease_past_the_free_capacity_is_refused_with_the_free_amount_and_changes_nothing() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    acquire_pool(&mut w.s, &w.alice, "gpu", 2.0, None, NOW).expect("alice takes two of three");
    refused(
        acquire_pool(&mut w.s, &w.bob, "gpu", 2.0, None, NOW),
        "pool gpu has 1 of 3 slot free",
    );
    let pools = w.s.pools(NOW).expect("pools");
    assert_eq!(pools[0].used, 2.0);
}

#[test]
fn a_pool_lease_may_take_exactly_the_free_remainder() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    acquire_pool(&mut w.s, &w.alice, "gpu", 2.0, None, NOW).expect("alice takes two");
    acquire_pool(&mut w.s, &w.bob, "gpu", 1.0, None, NOW).expect("bob takes the last one");
    refused(
        acquire_pool(&mut w.s, &w.carol, "gpu", 1.0, None, NOW),
        "pool gpu has 0 of 3 slot free",
    );
}

#[test]
fn releasing_a_pool_lease_frees_its_capacity_for_others() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    let held = acquire_pool(&mut w.s, &w.alice, "gpu", 2.0, None, NOW).expect("alice takes two");
    release(&mut w.s, &w.alice, &held.id, NOW + SEC).expect("alice releases");
    acquire_pool(&mut w.s, &w.bob, "gpu", 2.0, None, NOW + SEC)
        .expect("the released capacity is free");
    let pools = w.s.pools(NOW + SEC).expect("pools");
    assert_eq!(pools[0].used, 2.0);
}

#[test]
fn an_expired_pool_lease_frees_its_capacity_at_its_expiry_instant() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    acquire_pool(&mut w.s, &w.alice, "gpu", 2.0, Some(10.0), NOW).expect("alice takes two");
    let at_expiry = NOW + 10 * SEC;
    acquire_pool(&mut w.s, &w.carol, "gpu", 3.0, None, at_expiry)
        .expect("the expired hold no longer counts");
    let pools = w.s.pools(at_expiry).expect("pools");
    assert_eq!(pools[0].used, 3.0);
}

#[test]
fn a_pool_lease_names_a_defined_pool_and_a_positive_amount() {
    let mut w = world();
    let ctl = controller(&mut w);
    define_pool(&mut w.s, &ctl, "gpu", 3.0, "slot");
    match acquire_pool(&mut w.s, &w.alice, "nope", 1.0, None, NOW) {
        Err(StoreError::NotFound(what)) => assert_eq!(what, "pool nope"),
        other => panic!("expected NotFound for an undefined pool, got {other:?}"),
    }
    invalid(
        acquire_pool(&mut w.s, &w.alice, "gpu", 0.0, None, NOW),
        "lease amount must be a finite number above zero",
    );
}

// ---- path leases: acquisition and conflicts ----------------------------------------------------------------
