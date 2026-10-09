//! Pool definitions and pool leases: who may define a pool, how it is redefined, and how its capacity is shared.

use super::*;

#[test]
fn pool_define_is_controller_only() {
    let mut f = fixture();
    let arg = r#"{"name":"gpu","capacity":2}"#;
    let worker =
        f.s.pool_define(NOW, &op(&f.alice, "pool.define", "gpu", arg, NOW));
    assert!(matches!(
        worker,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    let sup =
        f.s.pool_define(NOW, &op(&f.sup, "pool.define", "gpu", arg, NOW));
    assert!(matches!(
        sup,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Supervisor
        )))
    ));
    let pool =
        f.s.pool_define(NOW, &op(&f.ctl, "pool.define", "gpu", arg, NOW))
            .expect("controller defines");
    assert_eq!(pool.name, "gpu");
    assert_eq!(pool.capacity, 2.0);
    assert_eq!(pool.used, 0.0);
    assert_eq!(pool.created_by, key(&f.ctl));
    let defined: i64 =
        f.s.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='pool.defined'",
                [],
                |r| r.get(0),
            )
            .expect("count");
    assert_eq!(defined, 1);
}

#[test]
fn pool_define_rejects_bad_name_capacity_and_subject() {
    let mut f = fixture();
    let bad_name = f.s.pool_define(
        NOW,
        &op(
            &f.ctl,
            "pool.define",
            "bad name",
            r#"{"name":"bad name","capacity":1}"#,
            NOW,
        ),
    );
    assert!(matches!(bad_name, Err(StoreError::Refused(_))));
    for capacity in ["0", "-2"] {
        let arg = format!(r#"{{"name":"gpu","capacity":{capacity}}}"#);
        let res =
            f.s.pool_define(NOW, &op(&f.ctl, "pool.define", "gpu", &arg, NOW));
        assert!(
            matches!(res, Err(StoreError::Invalid(_))),
            "capacity {capacity}"
        );
    }
    let mismatched = f.s.pool_define(
        NOW,
        &op(
            &f.ctl,
            "pool.define",
            "other",
            r#"{"name":"gpu","capacity":1}"#,
            NOW,
        ),
    );
    assert!(matches!(mismatched, Err(StoreError::Invalid(_))));
}

#[test]
fn pool_redefine_updates_capacity_unit_and_note_but_keeps_creator() {
    let mut f = fixture();
    define_pool(&mut f.s, &f.ctl, "gpu", 2.0, "slots");
    let arg = r#"{"name":"gpu","capacity":5,"unit":"gb","note":"rack 3"}"#;
    let redefined =
        f.s.pool_define(
            NOW + 1_000,
            &op(&f.owner, "pool.define", "gpu", arg, NOW + 1_000),
        )
        .expect("owner redefines");
    assert_eq!(redefined.capacity, 5.0);
    assert_eq!(redefined.unit, "gb");
    assert_eq!(redefined.note, "rack 3");
    assert_eq!(redefined.created_ms, NOW);
    assert_eq!(redefined.created_by, key(&f.ctl));
}

#[test]
fn pool_capacity_is_shared_across_holders() {
    let mut f = fixture();
    define_pool(&mut f.s, &f.ctl, "gpu", 3.0, "slots");
    let first = acquire_pool(&mut f.s, &f.alice, "gpu", 2.0, NOW).expect("alice takes 2");
    let full = acquire_pool(&mut f.s, &f.bob, "gpu", 2.0, NOW);
    match full {
        Err(StoreError::Refused(message)) => {
            assert_eq!(message, "pool gpu has 1 of 3 slots free")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    acquire_pool(&mut f.s, &f.bob, "gpu", 1.0, NOW).expect("bob takes the last 1");
    let used = f.s.pools(NOW).expect("pools")[0].used;
    assert_eq!(used, 3.0);
    f.s.lease_release(NOW, &op(&f.alice, "lease.release", &first.id, "", NOW))
        .expect("alice releases");
    assert_eq!(f.s.pools(NOW).expect("pools")[0].used, 1.0);
    acquire_pool(&mut f.s, &f.bob, "gpu", 2.0, NOW).expect("freed capacity is reusable");
}

#[test]
fn expired_pool_lease_frees_its_capacity() {
    let mut f = fixture();
    define_pool(&mut f.s, &f.ctl, "gpu", 1.0, "");
    let arg = r#"{"kind":"pool","resource":"gpu","amount":1,"ttl_s":10}"#;
    f.s.lease_acquire(NOW, &op(&f.alice, "lease.acquire", "", arg, NOW))
        .expect("alice takes the pool with a 10 s ttl");
    let expired_at = NOW + 10_000;
    assert_eq!(f.s.pools(expired_at).expect("pools")[0].used, 0.0);
    acquire_pool(&mut f.s, &f.bob, "gpu", 1.0, expired_at).expect("bob takes it after expiry");
}

#[test]
fn pool_lease_needs_a_defined_pool_and_a_positive_amount() {
    let mut f = fixture();
    let missing = acquire_pool(&mut f.s, &f.alice, "nope", 1.0, NOW);
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
    define_pool(&mut f.s, &f.ctl, "gpu", 2.0, "");
    let zero = acquire_pool(&mut f.s, &f.alice, "gpu", 0.0, NOW);
    assert!(matches!(zero, Err(StoreError::Invalid(_))));
}
