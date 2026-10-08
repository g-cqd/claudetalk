//! Public-API tests of persisted timers (D11) and of team/lane policies (D4, section 3.2): timer creation rules and
//! quota, deletion rights, due-time queries, firing (coalescing, one-shot timers, no-op cases, the system identity),
//! policy rights, validation, upsert, layering, and the notices an escalation leaves behind.
//!
//! Every operation is signed at the instant it is applied, and a fire is applied at an explicit instant, so no test
//! sleeps or reads a clock.

mod common;
// Integration-test crate roots resolve child modules from tests/, so the parts are named explicitly.
#[path = "timers_policy/create.rs"]
mod create;
#[path = "timers_policy/fire.rs"]
mod fire;
#[path = "timers_policy/policy.rs"]
mod policy;
#[path = "timers_policy/query.rs"]
mod query;

use agora_core::{Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::model::{TaskRecord, TimerRecord};
use agora_store::*;
use common::*;
use serde_json::{Value, json};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const TITLE: &str = "nightly build";

/// The store with an owner, a supervisor, workers alice and bob, an observer olive, a controller and the registered
/// system identity.
struct Fixture {
    s: Store,
    owner: Identity,
    sup: Identity,
    alice: Identity,
    bob: Identity,
    olive: Identity,
    ctl: Identity,
    sys: Identity,
}

fn fixture() -> Fixture {
    let World {
        mut s,
        owner,
        sup,
        alice,
        bob,
        olive,
        ..
    } = world();
    let ctl = Identity::from_seed([11; 32]);
    s.register_identity(NOW, &ctl.public(), "ctl")
        .expect("register controller");
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&ctl), "controller"))
        .expect("grant controller");
    let sys = Identity::from_seed([12; 32]);
    s.register_system_identity(NOW, &sys.public())
        .expect("register system identity");
    Fixture {
        s,
        owner,
        sup,
        alice,
        bob,
        olive,
        ctl,
        sys,
    }
}

// ---- outcome helpers -------------------------------------------------------------------------------------------

/// The text of a refusal; any other outcome fails the test.
fn refusal<T: std::fmt::Debug>(result: Result<T, StoreError>) -> String {
    match result {
        Err(StoreError::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The fixed text of an invalid-input error; any other outcome fails the test.
fn invalid<T: std::fmt::Debug>(result: Result<T, StoreError>) -> &'static str {
    match result {
        Err(StoreError::Invalid(message)) => message,
        other => panic!("expected an invalid-input error, got {other:?}"),
    }
}

/// The role named by a forbidden error; any other outcome fails the test.
fn forbidden<T: std::fmt::Debug>(result: Result<T, StoreError>) -> Role {
    match result {
        Err(StoreError::Transition(TransitionError::Forbidden(role))) => role,
        other => panic!("expected a forbidden error, got {other:?}"),
    }
}

/// The ids of a timer list, in the order given.
fn ids(timers: Vec<TimerRecord>) -> Vec<String> {
    timers.into_iter().map(|t| t.id).collect()
}

// ---- schedule and operation builders ---------------------------------------------------------------------------

/// An `every` schedule; `start_ms` pins the grid's anchor, and `None` anchors it at creation plus the period.
fn every(every_s: u64, start_ms: Option<i64>) -> Value {
    json!({"kind": "every", "every_s": every_s, "start_ms": start_ms})
}

/// A one-shot `at` schedule.
fn once_at(at_ms: i64) -> Value {
    json!({"kind": "at", "at_ms": at_ms})
}

/// A template with the title every template needs and nothing else.
fn plain_template() -> Value {
    json!({"title": TITLE})
}

/// Creates a timer as `who` at `at`.
fn create(
    s: &mut Store,
    who: &Identity,
    at: i64,
    id: &str,
    spec: Value,
    template: Value,
) -> Result<TimerRecord, StoreError> {
    let arg = json!({"id": id, "spec": spec, "template": template}).to_string();
    s.timer_create(at, &op_at(who, "timer.create", "", &arg, at))
}

/// Deletes timer `id` as `who` at `at`.
fn delete(s: &mut Store, who: &Identity, at: i64, id: &str) -> Result<(), StoreError> {
    s.timer_delete(at, &op_at(who, "timer.delete", id, "{}", at))
}

/// The system identity fires timer `id` at `at`; `fire_ms` is the scheduled instant it is firing for.
fn fire(
    s: &mut Store,
    sys: &Identity,
    at: i64,
    id: &str,
    fire_ms: i64,
) -> Result<Option<TaskRecord>, StoreError> {
    let arg = json!({"fire_ms": fire_ms}).to_string();
    s.timer_fire(at, &op_at(sys, "timer.fire", id, &arg, at))
}

/// The stored timer `id` owned by `owner`.
fn timer_of(s: &Store, owner: &Identity, id: &str) -> TimerRecord {
    let owner = pk_hex(owner);
    s.timers(Some(owner.as_str()))
        .expect("timers")
        .into_iter()
        .find(|t| t.id == id)
        .expect("timer exists")
}

/// Sets the per-identity timer quota through a controller's `work.settings`.
fn set_quota(f: &mut Fixture, quota: &str) {
    let arg = json!({"patch": {"timer_quota": quota}}).to_string();
    f.s.work_settings_set(NOW, &op(&f.ctl, "work.settings", "", &arg))
        .expect("settings");
}
