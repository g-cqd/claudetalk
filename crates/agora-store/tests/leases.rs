//! Public-API tests for pools, path leases, pool leases and quiet windows: capacity accounting, path conflicts
//! (prefix overlap, claimed task files, expiry instants), renewal and release rights, the controller-only quiet
//! window, and expiry applied by the system identity through `reconcile.expire`.
//!
//! Every op is signed at the instant it is applied, so expiry is exercised by moving `now` forward, never by
//! sleeping or reading the clock.

mod common;

use agora_core::{Role, TransitionError};
use agora_crypto::{Identity, pseudonym};
use agora_sched::model::{LeaseKind, LeaseRecord, PoolRecord};
use agora_sched::reconcile::ReconcileAction;
use agora_store::*;
use common::*;
use std::fmt::Debug;

const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
/// Expiry of a lease taken at `NOW` with the default TTL of 900 s, as the refusal messages render it.
const DEFAULT_EXPIRY: &str = "2023-11-14T22:28:20Z";

/// A controller, granted by the owner through a real `role.grant` op.
fn controller(w: &mut World) -> Identity {
    let ctl = Identity::from_seed([5; 32]);
    w.s.register_identity(NOW, &ctl.public(), "ctl")
        .expect("register controller");
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&ctl), "controller"),
    )
    .expect("grant controller");
    ctl
}

/// The daemon's system identity, registered as it is at bind.
fn system(w: &mut World) -> Identity {
    let sys = Identity::from_seed([9; 32]);
    w.s.register_system_identity(NOW, &sys.public())
        .expect("register system identity");
    sys
}

fn define_pool(s: &mut Store, ctl: &Identity, name: &str, capacity: f64, unit: &str) -> PoolRecord {
    let arg = serde_json::json!({"name": name, "capacity": capacity, "unit": unit}).to_string();
    s.pool_define(NOW, &op(ctl, "pool.define", name, &arg))
        .expect("pool defined")
}

fn acquire_pool(
    s: &mut Store,
    who: &Identity,
    pool: &str,
    amount: f64,
    ttl_s: Option<f64>,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({
        "kind": "pool", "resource": pool, "amount": amount, "ttl_s": ttl_s
    })
    .to_string();
    s.lease_acquire(now, &op_at(who, "lease.acquire", "", &arg, now))
}

fn acquire_path(
    s: &mut Store,
    who: &Identity,
    path: &str,
    ttl_s: Option<f64>,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({"kind": "path", "resource": path, "ttl_s": ttl_s}).to_string();
    s.lease_acquire(now, &op_at(who, "lease.acquire", "", &arg, now))
}

fn renew(
    s: &mut Store,
    who: &Identity,
    lease: &str,
    ttl_s: Option<f64>,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({ "ttl_s": ttl_s }).to_string();
    s.lease_renew(now, &op_at(who, "lease.renew", lease, &arg, now))
}

fn release(s: &mut Store, who: &Identity, lease: &str, now: i64) -> Result<(), StoreError> {
    s.lease_release(now, &op_at(who, "lease.release", lease, "", now))
}

fn open_quiet(
    s: &mut Store,
    who: &Identity,
    expected_end: i64,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({"expected_end_ms": expected_end, "note": "tests"}).to_string();
    s.quiet_open(now, &op_at(who, "quiet.open", "", &arg, now))
}

fn close_quiet(s: &mut Store, who: &Identity, lease: &str, now: i64) -> Result<(), StoreError> {
    s.quiet_close(now, &op_at(who, "quiet.close", lease, "", now))
}

fn queue_task(s: &mut Store, author: &Identity, id: &str, files: &[&str]) {
    let arg = serde_json::json!({"id": id, "title": "edit", "files": files}).to_string();
    s.work_add(NOW, &op(author, "work.add", "", &arg))
        .expect("task queued");
}

fn claim_task(s: &mut Store, who: &Identity, id: &str) {
    s.work_claim(NOW, &op(who, "work.claim", id, ""))
        .expect("task claimed");
}

/// A `reconcile.expire` action signed by `signer`, applied at `now`.
fn expire(s: &mut Store, signer: &Identity, lease: &str, now: i64) -> Result<bool, StoreError> {
    let action = ReconcileAction::ExpireLease {
        key: format!("expire:{lease}"),
        lease: lease.to_string(),
    };
    let arg = serde_json::to_string(&action).expect("action serialises");
    s.apply_reconcile(now, &op_at(signer, "reconcile.expire", "", &arg, now))
}

/// The lease with `id`, read through the public listing (no filters).
fn lease_at(s: &Store, now: i64, id: &str) -> LeaseRecord {
    s.leases(now, None, None, false)
        .expect("leases")
        .into_iter()
        .find(|lease| lease.id == id)
        .expect("lease is listed")
}

/// Ids of the leases a filtered listing returns, in listing order.
fn listed(
    s: &Store,
    now: i64,
    kind: Option<LeaseKind>,
    holder: Option<&str>,
    active_only: bool,
) -> Vec<String> {
    s.leases(now, kind, holder, active_only)
        .expect("leases")
        .into_iter()
        .map(|lease| lease.id)
        .collect()
}

fn forbidden<T: Debug>(result: Result<T, StoreError>, role: Role) {
    match result {
        Err(StoreError::Transition(TransitionError::Forbidden(got))) => assert_eq!(got, role),
        other => panic!("expected Forbidden({role:?}), got {other:?}"),
    }
}

fn refused<T: Debug>(result: Result<T, StoreError>, message: &str) {
    match result {
        Err(StoreError::Refused(got)) => assert_eq!(got, message),
        other => panic!("expected refusal {message:?}, got {other:?}"),
    }
}

fn invalid<T: Debug>(result: Result<T, StoreError>, message: &str) {
    match result {
        Err(StoreError::Invalid(got)) => assert_eq!(got, message),
        other => panic!("expected invalid {message:?}, got {other:?}"),
    }
}

/// The refusal for a path that `holder` holds through `what` (`lease <id>` or `task <id>`) until `until`.
fn held_message(path: &str, holder: &Identity, what: &str, until: &str) -> String {
    format!(
        "path {path} is held by {} ({what}) until {until}",
        pseudonym(&holder.public())
    )
}

// ---- pools -------------------------------------------------------------------------------------------------

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

#[test]
fn a_path_lease_records_its_holder_and_expires_after_the_default_ttl() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    assert_eq!(
        lease,
        LeaseRecord {
            id: lease.id.clone(),
            kind: LeaseKind::Path,
            resource: "/proj/w".into(),
            amount: 0.0,
            holder: pk_hex(&w.alice),
            holder_name: pseudonym(&w.alice.public()),
            card: None,
            task: None,
            created_ms: NOW,
            expires_ms: Some(NOW + 900 * SEC),
            heartbeat_ms: NOW,
            expected_end_ms: None,
            released_ms: None,
            note: String::new(),
        }
    );
}

#[test]
fn overlapping_paths_are_refused_in_both_directions_and_the_message_names_the_holder() {
    let mut w = world();
    let held = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w/x/y", None, NOW),
        &held_message(
            "/proj/w/x/y",
            &w.alice,
            &format!("lease {}", held.id),
            DEFAULT_EXPIRY,
        ),
    );
    let deeper = acquire_path(&mut w.s, &w.bob, "/proj/p/q", None, NOW).expect("bob takes /p/q");
    refused(
        acquire_path(&mut w.s, &w.carol, "/proj/p", None, NOW),
        &held_message(
            "/proj/p",
            &w.bob,
            &format!("lease {}", deeper.id),
            DEFAULT_EXPIRY,
        ),
    );
}

#[test]
fn sibling_paths_and_a_shared_string_prefix_do_not_conflict() {
    let mut w = world();
    acquire_path(&mut w.s, &w.alice, "/proj/w/a", None, NOW).expect("alice takes /w/a");
    acquire_path(&mut w.s, &w.bob, "/proj/w/b", None, NOW).expect("a sibling is free");
    acquire_path(&mut w.s, &w.carol, "/proj/w/ab", None, NOW)
        .expect("a shared string prefix is not a path overlap");
}

#[test]
fn the_holder_may_take_an_overlapping_path_again() {
    let mut w = world();
    acquire_path(&mut w.s, &w.alice, "/proj/w/x", None, NOW).expect("alice takes /w/x");
    acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW)
        .expect("alice may widen her own lease to its parent");
    let mine = pk_hex(&w.alice);
    assert_eq!(
        listed(&w.s, NOW, Some(LeaseKind::Path), Some(mine.as_str()), true).len(),
        2
    );
}

#[test]
fn a_path_lease_holds_until_its_expiry_instant_and_not_after() {
    let mut w = world();
    let held =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let just_before = NOW + 10 * SEC - 1;
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w", None, just_before),
        &held_message(
            "/proj/w",
            &w.alice,
            &format!("lease {}", held.id),
            "2023-11-14T22:13:30Z",
        ),
    );
    acquire_path(&mut w.s, &w.bob, "/proj/w", None, NOW + 10 * SEC)
        .expect("the path is free at the expiry instant");
}

#[test]
fn a_claimed_task_holds_its_files_against_other_identities() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    claim_task(&mut w.s, &w.alice, "t1");
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w", None, NOW),
        &held_message("/proj/w", &w.alice, "task t1", "-"),
    );
}

#[test]
fn the_claimant_may_lease_the_files_of_its_own_claimed_task() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    claim_task(&mut w.s, &w.alice, "t1");
    acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW)
        .expect("the claimant is not blocked by its own task");
}

#[test]
fn a_queued_task_holds_no_files() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    acquire_path(&mut w.s, &w.bob, "/proj/w/x.rs", None, NOW)
        .expect("only a claimed or running task holds files");
}

#[test]
fn a_relative_path_and_the_quiet_kind_are_refused_by_lease_acquire() {
    let mut w = world();
    let relative = serde_json::json!({"kind": "path", "resource": "w"}).to_string();
    invalid(
        w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &relative)),
        "path must be absolute",
    );
    let quiet = serde_json::json!({"kind": "quiet", "resource": "machine"}).to_string();
    invalid(
        w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &quiet)),
        "use quiet.open",
    );
}

// ---- renewal and release -----------------------------------------------------------------------------------

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
