//! Public-API tests for pools, path leases, pool leases and quiet windows: capacity accounting, path conflicts
//! (prefix overlap, claimed task files, expiry instants), renewal and release rights, the controller-only quiet
//! window, and expiry applied by the system identity through `reconcile.expire`.
//!
//! Every op is signed at the instant it is applied, so expiry is exercised by moving `now` forward, never by
//! sleeping or reading the clock.

#[path = "../common/mod.rs"]
mod common;
mod expiry_listing;
mod holders;
mod paths;
mod pools;
mod quiet;

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
