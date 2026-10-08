//! The breadth floor on path leases (S-8): a path must have at least two components after canonicalisation, unless the
//! holder is Supervisor or above.

use super::*;

fn assert_too_broad<T: std::fmt::Debug>(result: Result<T, StoreError>, path: &str) {
    match result {
        Err(StoreError::Refused(message)) => {
            assert_eq!(message, format!("{path} is too broad to claim"));
        }
        other => panic!("expected {path} to be too broad, got {other:?}"),
    }
}

#[test]
fn a_worker_cannot_lease_the_root_or_a_top_level_path() {
    let mut f = fixture();
    assert_too_broad(acquire_path(&mut f.s, &f.alice, "/", None, NOW), "/");
    assert_too_broad(acquire_path(&mut f.s, &f.alice, "/x", None, NOW), "/x");
    assert!(
        f.s.leases(NOW, None, None, false)
            .expect("leases")
            .is_empty(),
        "a refused lease writes nothing"
    );
}

#[test]
fn breadth_is_judged_after_canonicalisation() {
    let mut f = fixture();
    // "/x/.." and "/x/../y" normalise to the root and a top-level path.
    assert_too_broad(acquire_path(&mut f.s, &f.alice, "/x/..", None, NOW), "/");
    assert_too_broad(acquire_path(&mut f.s, &f.alice, "/x/../y", None, NOW), "/y");
}

#[test]
fn a_supervisor_may_lease_the_root_and_a_top_level_path() {
    let mut f = fixture();
    acquire_path(&mut f.s, &f.sup, "/", None, NOW).expect("root for a supervisor");
    acquire_path(&mut f.s, &f.sup, "/x", None, NOW).expect("top level for a supervisor");
}

#[test]
fn a_worker_may_lease_two_components() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/x/../x/y", None, NOW).expect("two components");
    assert_eq!(lease.resource, "/x/y");
}
