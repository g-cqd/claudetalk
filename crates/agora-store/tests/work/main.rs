//! Public-API tests of the work queue: scopes, adding tasks, claims, `next`, acceptance of commands, runs, finish,
//! cancel, update, reclaim, settings, the event log, the plan and the holders. Rules: SPEC section 3.2 and D3 to D9,
//! ref-queue-cli sections 1 to 4.

#[path = "../common/mod.rs"]
mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::model::{
    LeaseKind, LeaseRecord, NextOutcome, ScopeKind, ScopeRecord, StopTarget, TaskRecord, TaskState,
    WorkEvent,
};
use agora_sched::order::OrderPolicy;
use agora_sched::predict::Level;
use agora_sched::render::Mode;
use agora_store::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PROJ: &str = "proj";
const LOG_DIR: &str = "/var/agora/logs";
/// D5 digest of argv `["touch","/tmp/x"]` in `/` with no files, computed outside the crate with
/// `printf '%s' '["agora-work/v1",["touch","/tmp/x"],"/",[]]' | shasum -a 256`.
const TOUCH_DIGEST: &str = "059ca237bc078272521caa7a46631c6869a704487b8fed20619a2d58b43e7323";
/// The shared tail of the NAME refusal (claims::validate_name).
const NAME_RULE: &str =
    "use up to 128 letters, digits, _ . : @ or -, starting with a letter, a digit or _";

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

/// The role named by a forbidden-transition error; any other outcome fails the test.
fn forbidden<T: std::fmt::Debug>(result: Result<T, StoreError>) -> Role {
    match result {
        Err(StoreError::Transition(TransitionError::Forbidden(role))) => role,
        other => panic!("expected a forbidden error, got {other:?}"),
    }
}

/// The subject of a not-found error; any other outcome fails the test.
fn not_found<T: std::fmt::Debug>(result: Result<T, StoreError>) -> String {
    match result {
        Err(StoreError::NotFound(what)) => what,
        other => panic!("expected not found, got {other:?}"),
    }
}

fn kinds(events: &[WorkEvent]) -> Vec<(String, Option<String>)> {
    events
        .iter()
        .map(|event| (event.kind.clone(), event.task.clone()))
        .collect()
}

fn claimant_name(s: &Store, id: &str) -> String {
    s.work_task(id)
        .expect("task")
        .agent_name
        .expect("a claimed task has a claimant name")
}

fn author_name(s: &Store, id: &str) -> String {
    s.work_task(id).expect("task").author_name
}

// ---- builders --------------------------------------------------------------------------------------------------

/// Add arguments for `id` in the project scope, with the `extra` fields replacing the defaults.
fn add_args(id: &str, extra: Value) -> String {
    let mut args = json!({"id": id, "title": id, "scope": PROJ});
    if let (Some(map), Value::Object(more)) = (args.as_object_mut(), extra) {
        map.extend(more);
    }
    args.to_string()
}

/// Queues `id` as `who` at `when`; the op is signed at the same time.
fn queue_at(s: &mut Store, who: &Identity, id: &str, when: i64, extra: Value) -> TaskRecord {
    let signed = op_at(who, "work.add", "", &add_args(id, extra), when);
    s.work_add(when, &signed).expect("queue task")
}

fn queue(s: &mut Store, who: &Identity, id: &str, extra: Value) -> TaskRecord {
    queue_at(s, who, id, NOW, extra)
}

/// A queued task whose command is `touch /tmp/x` in `/`.
fn command_task(s: &mut Store, who: &Identity, id: &str) -> TaskRecord {
    queue(
        s,
        who,
        id,
        json!({"command": ["touch", "/tmp/x"], "cwd": "/"}),
    )
}

fn set_scope(s: &mut Store, who: &Identity, args: Value) -> Result<ScopeRecord, StoreError> {
    let id = args["id"].as_str().expect("scope id").to_string();
    s.work_scope_set(NOW, &op(who, "work.scope", &id, &args.to_string()))
}

fn scope(s: &mut Store, who: &Identity, id: &str, parent: Option<&str>) -> ScopeRecord {
    let mut args = json!({"id": id});
    if let Some(parent) = parent {
        args["parent"] = json!(parent);
    }
    set_scope(s, who, args).expect("set scope")
}

/// The project scope every queue test runs in.
fn project(w: &mut World) {
    scope(&mut w.s, &w.alice, PROJ, None);
}

fn claim(s: &mut Store, who: &Identity, id: &str) -> TaskRecord {
    s.work_claim(NOW, &op(who, "work.claim", id, "{}"))
        .expect("claim")
}

fn next_task(s: &mut Store, who: &Identity, scope: Option<&str>, when: i64) -> NextOutcome {
    let arg = json!({ "scope": scope }).to_string();
    s.work_next(when, &op_at(who, "work.next", "", &arg, when))
        .expect("next")
}

fn accept(
    s: &mut Store,
    who: &Identity,
    id: &str,
    digest: &str,
) -> Result<(TaskRecord, bool), StoreError> {
    let arg = json!({ "digest": digest }).to_string();
    s.work_accept(NOW, &op(who, "work.accept", id, &arg))
}

fn run(s: &mut Store, who: &Identity, id: &str) -> Result<TaskRecord, StoreError> {
    let arg = json!({ "task": id }).to_string();
    s.work_request(NOW, &op(who, "work.run", "", &arg), 4.0, LOG_DIR)
}

fn finish(
    s: &mut Store,
    who: &Identity,
    id: &str,
    state: &str,
    note: &str,
) -> Result<TaskRecord, StoreError> {
    let arg = json!({ "state": state, "note": note }).to_string();
    s.work_finish(NOW, &op(who, "work.finish", id, &arg))
}

fn path_lease(s: &mut Store, who: &Identity, path: &str, ttl_s: Option<f64>) -> LeaseRecord {
    let arg = json!({ "kind": "path", "resource": path, "ttl_s": ttl_s }).to_string();
    s.lease_acquire(NOW, &op(who, "lease.acquire", "", &arg))
        .expect("path lease")
}

/// A controller, registered and granted by the owner. The World has none.
fn controller(w: &mut World) -> Identity {
    let ctl = Identity::from_seed([9; 32]);
    w.s.register_identity(NOW, &ctl.public(), "ctl")
        .expect("register controller");
    w.s.grant_role(
        NOW,
        &op(&w.owner, "role.grant", &pk_hex(&ctl), "controller"),
    )
    .expect("grant controller");
    ctl
}

/// The system identity the daemon signs with, registered as the store's one system identity.
fn system_identity(s: &mut Store) -> Identity {
    let sys = Identity::from_seed([10; 32]);
    s.register_system_identity(NOW, &sys.public())
        .expect("register system identity");
    sys
}

/// The daemon's `job.start` for a requested task: the command runs as process 4242 under supervisor 4200.
fn start_job(s: &mut Store, sys: &Identity, id: &str) -> TaskRecord {
    let arg =
        json!({"pid": 4242, "supervisor_pid": 4200, "supervisor_start_s": 1700, "qos": "utility"});
    s.job_start(NOW, &op(sys, "job.start", id, &arg.to_string()))
        .expect("job start")
}

mod accept_run;
mod add;
mod claims;
mod finish_cancel;
mod scopes;
mod settings;
mod update_reclaim;
mod views;
