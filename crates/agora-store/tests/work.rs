//! Public-API tests of the work queue: scopes, adding tasks, claims, `next`, acceptance of commands, runs, finish,
//! cancel, update, reclaim, settings, the event log, the plan and the holders. Rules: SPEC section 3.2 and D3 to D9,
//! ref-queue-cli sections 1 to 4.

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

// ---- scopes ----------------------------------------------------------------------------------------------------

#[test]
fn a_new_scope_is_a_project_at_the_top_and_an_agent_scope_under_a_parent() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let top = scope(s, alice, PROJ, None);
    assert_eq!((top.kind, top.parent), (ScopeKind::Project, None));
    let child = scope(s, alice, "proj-a", Some(PROJ));
    assert_eq!(child.kind, ScopeKind::Agent);
    assert_eq!(child.parent.as_deref(), Some(PROJ));
}

#[test]
fn listing_a_root_gives_its_subtree_in_creation_order_with_open_task_counts() {
    let mut w = world();
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    scope(s, alice, "other", None);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"scope": "proj-a"}));
    claim(s, bob, "t2");
    let listed = s.work_scopes(Some(PROJ)).expect("scopes");
    let rows: Vec<(&str, BTreeMap<String, u64>)> = listed
        .iter()
        .map(|row| (row.id.as_str(), row.counts.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (PROJ, BTreeMap::from([("queued".to_string(), 1)])),
            ("proj-a", BTreeMap::from([("claimed".to_string(), 1)])),
        ]
    );
}

#[test]
fn a_scope_cannot_be_put_under_itself_or_under_its_own_descendant() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    let under_child = refusal(set_scope(s, alice, json!({"id": PROJ, "parent": "proj-a"})));
    assert_eq!(
        under_child,
        "scope proj cannot be under proj-a, which is proj or under it"
    );
    let under_self = refusal(set_scope(s, alice, json!({"id": PROJ, "parent": PROJ})));
    assert_eq!(
        under_self,
        "scope proj cannot be under proj, which is proj or under it"
    );
}

#[test]
fn a_scope_with_an_unknown_parent_is_refused_and_not_created() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let msg = refusal(set_scope(s, alice, json!({"id": "x", "parent": "nope"})));
    assert_eq!(msg, "unknown parent scope nope");
    assert!(s.work_scopes(Some("x")).expect("scopes").is_empty());
}

#[test]
fn an_empty_parent_makes_a_scope_a_root_and_keeps_its_kind() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    scope(s, alice, PROJ, None);
    scope(s, alice, "proj-a", Some(PROJ));
    let cleared = set_scope(s, alice, json!({"id": "proj-a", "parent": ""})).expect("clear");
    assert_eq!((cleared.parent, cleared.kind), (None, ScopeKind::Agent));
}

// ---- adding tasks ----------------------------------------------------------------------------------------------

#[test]
fn a_task_added_with_only_a_scope_starts_queued_with_every_default() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("add");
    assert_eq!((t.id.as_str(), t.title.as_str()), ("task-1", ""));
    assert_eq!((t.kind.as_str(), t.state), ("other", TaskState::Queued));
    assert_eq!(t.scope.as_deref(), Some(PROJ));
    assert_eq!(t.author, pk_hex(alice));
    assert_eq!((t.size, t.value, t.weight), (1.0, 1.0, 1.0));
    assert_eq!((t.agent, t.accepted_by, t.claimed_ms), (None, None, None));
    assert_eq!((t.requested_ms, t.command, t.cwd), (None, None, None));
    assert_eq!((t.digest, t.note.as_str()), (None, ""));
    assert!(t.after.is_empty() && t.files.is_empty() && !t.needs_acceptance);
}

#[test]
fn queued_tasks_without_an_id_are_numbered_task_n_in_arrival_order() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let first = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("first");
    let second = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("second");
    assert_eq!(
        (first.id.as_str(), second.id.as_str()),
        ("task-1", "task-2")
    );
}

#[test]
fn an_explicit_task_id_is_kept_as_given() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = queue(s, alice, "deploy-1", json!({}));
    assert_eq!((t.id.as_str(), t.state), ("deploy-1", TaskState::Queued));
}

#[test]
fn an_automatic_id_skips_an_id_already_taken() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "task-2", json!({}));
    let auto = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("auto");
    assert_eq!(auto.id, "task-3");
}

#[test]
fn a_duplicate_task_id_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let again = s.work_add(NOW, &op(alice, "work.add", "", &add_args("t1", json!({}))));
    assert_eq!(refusal(again), "task t1 exists already");
}

#[test]
fn a_task_after_an_unknown_task_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t9", json!({"after": ["ghost"]}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(msg, "task t9 is after unknown task ghost");
}

#[test]
fn an_unknown_scope_is_refused_with_the_create_hint() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"scope": "nope"}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(
        msg,
        "unknown scope nope; create it with `agora work scope set nope --kind project`"
    );
}

#[test]
fn invalid_task_ids_are_refused_by_the_name_rule() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let long = "a".repeat(129);
    for id in ["bad id", "", "-lead", long.as_str()] {
        let add = add_args(id, json!({}));
        let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
        assert_eq!(msg, format!("task '{id}' is not valid: {NAME_RULE}"));
    }
}

#[test]
fn an_invalid_kind_is_refused_by_the_kind_rule() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"kind": "bad kind"}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(
        msg,
        "kind 'bad kind' is not valid: use up to 64 letters, digits, _ or -"
    );
}

#[test]
fn a_zero_size_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"size": 0}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &add));
    assert_eq!(invalid(err), "size must be a number above zero");
}

#[test]
fn a_relative_file_path_without_a_working_directory_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"files": ["src/a.rs"]}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &add));
    assert_eq!(
        invalid(err),
        "a relative file path needs a working directory"
    );
}

#[test]
fn file_paths_are_stored_canonical_and_deduplicated() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = queue(
        s,
        alice,
        "t1",
        json!({"files": ["/proj/src/../docs//a.md", "/proj/docs/a.md/"]}),
    );
    assert_eq!(t.files, vec!["/proj/docs/a.md".to_string()]);
}

#[test]
fn a_command_needs_an_existing_absolute_working_directory() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let no_cwd = add_args("t1", json!({"command": ["true"]}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &no_cwd));
    assert_eq!(invalid(err), "a command needs a working directory");
    let missing = add_args(
        "t1",
        json!({"command": ["true"], "cwd": "/nonexistent-agora-m5"}),
    );
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &missing)));
    assert_eq!(msg, "/nonexistent-agora-m5 is not a directory");
}

#[test]
fn malformed_add_arguments_are_refused() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let err = s.work_add(NOW, &op(alice, "work.add", "", "not json"));
    assert_eq!(invalid(err), "malformed arguments");
}

// ---- claims ----------------------------------------------------------------------------------------------------

#[test]
fn a_claim_waits_while_a_prerequisite_is_not_done_and_names_it() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"after": ["t1"]}));
    let waiting = refusal(s.work_claim(NOW, &op(bob, "work.claim", "t2", "{}")));
    assert_eq!(waiting, "task t2 waits for t1 (queued)");
    finish(s, alice, "t1", "done", "").expect("close t1");
    let t2 = claim(s, bob, "t2");
    assert_eq!(t2.agent, Some(pk_hex(bob)));
}

#[test]
fn a_claim_is_refused_when_a_claimed_task_holds_its_files_and_names_the_holder() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src"]}));
    claim(s, bob, "t0");
    queue(s, alice, "t1", json!({"files": ["/proj/src/lib.rs"]}));
    let holder = claimant_name(s, "t0");
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(
        msg,
        format!("task t1 changes files that task t0 holds (claimed by {holder})")
    );
}

#[test]
fn a_claim_is_refused_when_a_path_lease_of_another_identity_covers_its_files() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    let lease = path_lease(s, bob, "/proj/docs", None);
    queue(s, alice, "t1", json!({"files": ["/proj/docs/readme.md"]}));
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(
        msg,
        format!("task t1 changes files that lease:{} holds", lease.id)
    );
}

#[test]
fn a_claimant_does_not_conflict_with_its_own_path_lease() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    path_lease(s, bob, "/proj/docs", None);
    queue(s, alice, "t1", json!({"files": ["/proj/docs/readme.md"]}));
    let t1 = claim(s, bob, "t1");
    assert_eq!(
        (t1.state, t1.agent),
        (TaskState::Claimed, Some(pk_hex(bob)))
    );
}

#[test]
fn a_task_already_claimed_is_refused_and_names_its_claimant() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(s.work_claim(NOW, &op(carol, "work.claim", "t1", "{}")));
    assert_eq!(msg, format!("task t1 is claimed by {holder}"));
}

#[test]
fn claiming_an_unknown_task_is_not_found() {
    let mut w = world();
    let bob = &w.bob;
    let err = w.s.work_claim(NOW, &op(bob, "work.claim", "ghost", "{}"));
    assert_eq!(not_found(err), "task ghost");
}

// ---- next ------------------------------------------------------------------------------------------------------

#[test]
fn next_claims_queued_tasks_in_arrival_order() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    queue_at(s, alice, "t3", NOW + 2, json!({}));
    let mut order = Vec::new();
    for step in 0..3 {
        let claimed = next_task(s, bob, Some(PROJ), NOW + 3 + step);
        order.push(claimed.claimed.expect("a claim").id);
    }
    assert_eq!(order, vec!["t1", "t2", "t3"]);
}

#[test]
fn next_puts_a_validity_task_before_an_earlier_ordinary_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({"kind": "feature"}));
    queue_at(s, alice, "t2", NOW + 1, json!({"kind": "security"}));
    let claimed = next_task(s, bob, Some(PROJ), NOW + 2);
    assert_eq!(claimed.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_skips_a_task_waiting_for_an_unfinished_prerequisite() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t0", NOW, json!({}));
    queue_at(s, alice, "t1", NOW + 1, json!({"after": ["t0"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({}));
    let first = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(first.claimed.map(|t| t.id), Some("t0".to_string()));
    let second = next_task(s, bob, Some(PROJ), NOW + 4);
    assert_eq!(second.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_skips_a_task_whose_files_a_claimed_task_holds() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src/main.rs"]}));
    claim(s, carol, "t0");
    queue_at(s, alice, "t1", NOW + 1, json!({"files": ["/proj/src"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({}));
    let claimed = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(claimed.claimed.map(|t| t.id), Some("t2".to_string()));
}

#[test]
fn next_in_a_scope_with_nothing_queued_names_the_scope() {
    let mut w = world();
    project(&mut w);
    scope(&mut w.s, &w.alice, "other", None);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"scope": "other"}));
    let outcome = next_task(s, bob, Some(PROJ), NOW + 1);
    assert_eq!(outcome.claimed, None);
    assert_eq!(outcome.reason, "nothing is queued in scope proj");
}

#[test]
fn next_without_a_scope_and_an_empty_queue_says_nothing_is_queued() {
    let mut w = world();
    let bob = &w.bob;
    let outcome =
        w.s.work_next(NOW, &op(bob, "work.next", "", "{}"))
            .expect("next");
    assert_eq!(outcome.claimed, None);
    assert_eq!(outcome.reason, "nothing is queued");
}

#[test]
fn next_reports_how_many_queued_tasks_wait_or_are_held_when_nothing_is_claimable() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t0", json!({"files": ["/proj/src/main.rs"]}));
    claim(s, carol, "t0");
    queue_at(s, alice, "t1", NOW + 1, json!({"files": ["/proj/src"]}));
    queue_at(s, alice, "t2", NOW + 2, json!({"after": ["t0"]}));
    let outcome = next_task(s, bob, Some(PROJ), NOW + 3);
    assert_eq!(outcome.claimed, None);
    assert_eq!(
        outcome.reason,
        "of 2 queued task(s) in scope proj, 1 wait for their prerequisites and 1 change files that claimed or \
         running tasks hold"
    );
}

#[test]
fn next_refuses_an_unknown_scope() {
    let mut w = world();
    let bob = &w.bob;
    let msg = refusal(w.s.work_next(NOW, &op(bob, "work.next", "", r#"{"scope":"nope"}"#)));
    assert_eq!(
        msg,
        "unknown scope nope; create it with `agora work scope set nope --kind project`"
    );
}

// ---- acceptance (D5) -------------------------------------------------------------------------------------------

#[test]
fn accept_is_refused_for_a_task_without_a_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let msg = refusal(accept(s, bob, "t1", TOUCH_DIGEST));
    assert_eq!(msg, "task t1 has no command to accept");
}

#[test]
fn accept_is_refused_before_the_task_is_claimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    let msg = refusal(accept(s, bob, "t1", TOUCH_DIGEST));
    assert_eq!(
        msg,
        "task t1 is queued; accept a command after you claim its task"
    );
}

#[test]
fn accept_is_refused_for_a_task_claimed_by_someone_else() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(accept(s, carol, "t1", TOUCH_DIGEST));
    assert_eq!(
        msg,
        format!("task t1 is claimed by {holder}; only that agent accepts its command")
    );
}

#[test]
fn accept_refuses_a_digest_that_does_not_match_the_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let wrong = "0".repeat(64);
    let msg = refusal(accept(s, bob, "t1", &wrong));
    assert_eq!(
        msg,
        "the digest does not match the command of task t1 in its directory; read it with `agora work show t1`, \
         and accept it with the digest that show prints"
    );
}

#[test]
fn accepting_the_d5_digest_records_the_claimant_as_accepter() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let (task, first) = accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(bob)));
}

#[test]
fn accept_takes_the_digest_in_upper_case() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let (task, first) = accept(s, bob, "t1", &TOUCH_DIGEST.to_uppercase()).expect("accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(bob)));
}

#[test]
fn accepting_again_is_not_a_first_acceptance() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("first accept");
    let (_, first) = accept(s, bob, "t1", TOUCH_DIGEST).expect("second accept");
    assert!(!first);
}

#[test]
fn an_author_may_not_accept_their_own_command_by_default() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    claim(s, alice, "t1");
    let err = accept(s, alice, "t1", TOUCH_DIGEST);
    assert_eq!(
        invalid(err),
        "a command must be accepted by an identity other than its author"
    );
}

#[test]
fn an_author_may_accept_their_own_command_once_the_controller_allows_self_accept() {
    let mut w = world();
    project(&mut w);
    let ctl = controller(&mut w);
    let settings = json!({"patch": {"self_accept": "true"}}).to_string();
    w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &settings))
        .expect("settings");
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    claim(s, alice, "t1");
    let (task, first) = accept(s, alice, "t1", TOUCH_DIGEST).expect("self accept");
    assert!(first);
    assert_eq!(task.accepted_by, Some(pk_hex(alice)));
}

// ---- run requests ----------------------------------------------------------------------------------------------

#[test]
fn an_inline_run_creates_a_claimed_accepted_job_with_a_prediction_and_log_path() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let arg = json!({"add": {"command": ["touch", "/tmp/x"], "cwd": "/"}}).to_string();
    let job = s
        .work_request(NOW, &op(alice, "work.run", "", &arg), 4.0, LOG_DIR)
        .expect("inline run");
    assert_eq!(job.id, "job-1");
    assert_eq!(job.state, TaskState::Claimed);
    assert_eq!(job.agent, Some(pk_hex(alice)));
    assert_eq!(job.accepted_by, Some(pk_hex(alice)));
    assert_eq!(job.requested_ms, Some(NOW));
    assert_eq!(job.log.as_deref(), Some("/var/agora/logs/job-1.log"));
    assert_eq!(job.prediction.map(|p| p.level), Some(Level::None));
}

#[test]
fn a_queued_command_cannot_run_until_its_claimant_accepts_it() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    let msg = refusal(run(s, bob, "t1"));
    assert_eq!(
        msg,
        format!(
            "task t1 runs a command that {} queued, and you have not accepted it: read it with `agora work show t1`, \
             then accept it with the `agora work accept t1 --digest ...` line that show prints",
            author_name(s, "t1")
        )
    );
}

#[test]
fn an_accepted_claimed_task_is_requested_with_its_log_path_and_waits_for_capacity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    let requested = run(s, bob, "t1").expect("run");
    assert_eq!(requested.requested_ms, Some(NOW));
    assert_eq!(requested.log.as_deref(), Some("/var/agora/logs/t1.log"));
    let waiting: Vec<String> = s
        .work_waiting()
        .expect("waiting")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(waiting, vec!["t1"]);
}

#[test]
fn a_run_names_exactly_one_of_a_task_or_a_command() {
    let mut w = world();
    let alice = &w.alice;
    let neither =
        w.s.work_request(NOW, &op(alice, "work.run", "", "{}"), 4.0, LOG_DIR);
    assert_eq!(
        invalid(neither),
        "name a task you claimed, or give a command to run"
    );
    let both = json!({"task": "t1", "add": {"command": ["true"], "cwd": "/"}}).to_string();
    let err =
        w.s.work_request(NOW, &op(alice, "work.run", "", &both), 4.0, LOG_DIR);
    assert_eq!(invalid(err), "give a task ID or a command, not both");
}

#[test]
fn a_queued_task_cannot_be_run_before_it_is_claimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    let msg = refusal(run(s, bob, "t1"));
    assert_eq!(msg, "claim task t1 first: `agora work claim t1`");
}

// ---- finish ----------------------------------------------------------------------------------------------------

#[test]
fn the_claimant_closes_a_task_without_a_command_as_done_or_failed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({}));
    claim(s, bob, "t1");
    claim(s, bob, "t2");
    for (id, state) in [("t1", TaskState::Done), ("t2", TaskState::Failed)] {
        let closed = finish(s, bob, id, state.as_str(), "ok").expect("finish");
        assert_eq!((closed.state, closed.note.as_str()), (state, "ok"));
        assert_eq!(closed.ended_ms, Some(NOW));
    }
}

#[test]
fn the_author_closes_its_own_queued_task_without_a_command() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let closed = finish(s, alice, "t1", "done", "").expect("finish");
    assert_eq!(closed.state, TaskState::Done);
}

#[test]
fn a_task_with_a_command_cannot_be_closed_by_hand() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    command_task(s, alice, "t1");
    let msg = refusal(finish(s, alice, "t1", "done", ""));
    assert_eq!(
        msg,
        "task t1 has a command: run it with `agora work run t1`, or cancel it"
    );
}

#[test]
fn a_queued_task_is_closed_only_by_its_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let msg = refusal(finish(s, bob, "t1", "done", ""));
    assert_eq!(
        msg,
        format!(
            "task t1 was queued by {name}; until an agent claims it, only {name} closes it",
            name = author_name(s, "t1")
        )
    );
}

#[test]
fn a_claimed_task_is_closed_only_by_its_claimant() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let holder = claimant_name(s, "t1");
    let msg = refusal(finish(s, carol, "t1", "done", ""));
    assert_eq!(
        msg,
        format!("task t1 is claimed by {holder}; only that agent closes it")
    );
}

#[test]
fn a_final_task_cannot_be_closed_again() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    finish(s, alice, "t1", "done", "").expect("first close");
    let msg = refusal(finish(s, alice, "t1", "failed", ""));
    assert_eq!(msg, "task t1 is done already");
}

// ---- cancel ----------------------------------------------------------------------------------------------------

#[test]
fn the_author_cancels_its_queued_task_as_cancelled() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let outcome = s
        .work_cancel(
            NOW,
            &op(alice, "work.cancel", "t1", r#"{"note":"dropped"}"#),
        )
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
    assert_eq!(outcome.task.note, "dropped");
    assert_eq!(outcome.stop, None);
}

#[test]
fn the_claimant_cancels_its_claimed_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let outcome = s
        .work_cancel(NOW, &op(bob, "work.cancel", "t1", "{}"))
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
}

#[test]
fn a_worker_cannot_cancel_a_task_claimed_by_someone_else() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, carol) = (&mut w.s, &w.alice, &w.bob, &w.carol);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let err = s.work_cancel(NOW, &op(carol, "work.cancel", "t1", "{}"));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_supervisor_cancels_any_queued_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup) = (&mut w.s, &w.alice, &w.sup);
    queue(s, alice, "t1", json!({}));
    let outcome = s
        .work_cancel(NOW, &op(sup, "work.cancel", "t1", "{}"))
        .expect("cancel");
    assert_eq!(outcome.task.state, TaskState::Cancelled);
}

#[test]
fn cancelling_a_running_task_requests_a_stop_and_keeps_its_state() {
    let mut w = world();
    project(&mut w);
    let sys = system_identity(&mut w.s);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    run(s, bob, "t1").expect("run");
    start_job(s, &sys, "t1");
    let outcome = s
        .work_cancel(NOW, &op(bob, "work.cancel", "t1", r#"{"note":"stop"}"#))
        .expect("cancel running");
    assert_eq!(outcome.task.state, TaskState::Running);
    assert!(outcome.task.cancel_requested);
    assert_eq!(
        outcome.stop,
        Some(StopTarget {
            supervisor_pid: 4200,
            supervisor_start_s: Some(1700),
            job_dir: String::new(),
        })
    );
    assert!(s.work_task("t1").expect("task").cancel_requested);
}

#[test]
fn a_final_task_cannot_be_cancelled() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    s.work_cancel(NOW, &op(alice, "work.cancel", "t1", "{}"))
        .expect("first cancel");
    let msg = refusal(s.work_cancel(NOW, &op(alice, "work.cancel", "t1", "{}")));
    assert_eq!(msg, "task t1 is cancelled already");
}

// ---- update and assign -----------------------------------------------------------------------------------------

#[test]
fn a_supervisor_edits_a_task_it_did_not_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup) = (&mut w.s, &w.alice, &w.sup);
    queue(s, alice, "t1", json!({"title": "old"}));
    let arg = json!({"title": "new", "size": 3.0}).to_string();
    let edited = s
        .work_update(NOW, &op(sup, "work.update", "t1", &arg))
        .expect("update");
    assert_eq!((edited.title.as_str(), edited.size), ("new", 3.0));
}

#[test]
fn a_worker_cannot_edit_a_queued_task_it_did_not_author() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let err = s.work_update(NOW, &op(bob, "work.update", "t1", r#"{"title":"mine"}"#));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_supervisor_assigns_a_queued_task_by_claiming_it_for_the_identity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(bob)}).to_string();
    let assigned = s
        .work_update(NOW, &op(sup, "work.update", "t1", &arg))
        .expect("assign");
    assert_eq!(assigned.state, TaskState::Claimed);
    assert_eq!(assigned.agent, Some(pk_hex(bob)));
    assert_eq!(assigned.claimed_ms, Some(NOW));
}

#[test]
fn a_worker_cannot_assign_a_task_to_an_identity() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(bob)}).to_string();
    let err = s.work_update(NOW, &op(alice, "work.update", "t1", &arg));
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn assigning_to_an_unregistered_identity_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice, sup, eve) = (&mut w.s, &w.alice, &w.sup, &w.eve);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"agent": pk_hex(eve)}).to_string();
    let err = s.work_update(NOW, &op(sup, "work.update", "t1", &arg));
    assert_eq!(invalid(err), "agent is not an active worker identity");
}

#[test]
fn an_update_that_closes_a_prerequisite_cycle_is_refused_with_its_path() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    queue(s, alice, "t2", json!({"after": ["t1"]}));
    let arg = json!({"after": ["t2"]}).to_string();
    let msg = refusal(s.work_update(NOW, &op(alice, "work.update", "t1", &arg)));
    assert_eq!(msg, "the tasks form a cycle: t1 -> t2 -> t1");
}

#[test]
fn an_update_naming_an_unknown_prerequisite_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let arg = json!({"after": ["ghost"]}).to_string();
    let msg = refusal(s.work_update(NOW, &op(alice, "work.update", "t1", &arg)));
    assert_eq!(msg, "task t1 is after unknown task ghost");
}

#[test]
fn a_claimant_unassigns_its_claimed_task_back_to_the_queue() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let unassigned = s
        .work_update(NOW, &op(bob, "work.update", "t1", r#"{"agent":null}"#))
        .expect("unassign");
    assert_eq!(
        (unassigned.state, unassigned.agent, unassigned.claimed_ms),
        (TaskState::Queued, None, None)
    );
}

// ---- reclaim ---------------------------------------------------------------------------------------------------

#[test]
fn a_supervisor_reclaims_a_claimed_task_to_the_queue() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    s.work_reclaim(
        NOW,
        &op(sup, "work.reclaim", "task:t1", r#"{"note":"stale"}"#),
    )
    .expect("reclaim");
    let t = s.work_task("t1").expect("task");
    assert_eq!(
        (t.state, t.agent, t.claimed_ms, t.note.as_str()),
        (TaskState::Queued, None, None, "stale")
    );
}

#[test]
fn a_worker_cannot_reclaim_a_task() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({}));
    claim(s, bob, "t1");
    let err = s.work_reclaim(
        NOW,
        &op(bob, "work.reclaim", "task:t1", r#"{"note":"mine"}"#),
    );
    assert_eq!(forbidden(err), Role::Worker);
}

#[test]
fn a_task_that_was_requested_cannot_be_reclaimed() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob, sup) = (&mut w.s, &w.alice, &w.bob, &w.sup);
    command_task(s, alice, "t1");
    claim(s, bob, "t1");
    accept(s, bob, "t1", TOUCH_DIGEST).expect("accept");
    run(s, bob, "t1").expect("run");
    let holder = claimant_name(s, "t1");
    let msg = refusal(s.work_reclaim(NOW, &op(sup, "work.reclaim", "task:t1", "{}")));
    assert_eq!(
        msg,
        format!(
            "task t1 is waiting for capacity (claimed by {holder}); only a claimed task that has not been requested \
             can be reclaimed"
        )
    );
}

#[test]
fn a_supervisor_releases_a_path_lease_so_it_no_longer_holds_its_path() {
    let mut w = world();
    project(&mut w);
    let (s, bob, sup) = (&mut w.s, &w.bob, &w.sup);
    let lease = path_lease(s, bob, "/proj/docs", None);
    let subject = format!("lease:{}", lease.id);
    s.work_reclaim(
        NOW,
        &op(sup, "work.reclaim", &subject, r#"{"note":"stale"}"#),
    )
    .expect("release");
    assert!(s.work_holders(NOW, None).expect("holders").is_empty());
    assert!(
        s.leases(NOW, Some(LeaseKind::Path), None, true)
            .expect("leases")
            .is_empty()
    );
}

// ---- settings --------------------------------------------------------------------------------------------------

#[test]
fn changing_the_settings_needs_a_controller() {
    let mut w = world();
    let (s, sup) = (&mut w.s, &w.sup);
    let arg = json!({"patch": {"self_accept": "true"}}).to_string();
    let err = s.work_settings_set(NOW, &op(sup, "work.settings", "", &arg));
    assert_eq!(forbidden(err), Role::Supervisor);
}

#[test]
fn a_settings_patch_is_stored_and_read_back() {
    let mut w = world();
    let ctl = controller(&mut w);
    let arg = json!({"patch": {"self_accept": "true"}}).to_string();
    let returned =
        w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &arg))
            .expect("settings");
    assert!(returned.self_accept);
    assert!(w.s.work_settings().expect("read").self_accept);
}

#[test]
fn an_invalid_settings_value_is_refused_and_nothing_is_stored() {
    let mut w = world();
    let ctl = controller(&mut w);
    let arg = json!({"patch": {"self_accept": "maybe"}}).to_string();
    let err =
        w.s.work_settings_set(NOW, &op(&ctl, "work.settings", "", &arg));
    assert!(matches!(err, Err(StoreError::Refused(_))), "{err:?}");
    assert!(!w.s.work_settings().expect("read").self_accept);
}

// ---- log -------------------------------------------------------------------------------------------------------

#[test]
fn the_log_returns_the_newest_events_oldest_first_up_to_the_limit() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    s.work_claim(NOW + 2, &op_at(bob, "work.claim", "t1", "{}", NOW + 2))
        .expect("claim");
    let last_three = s.work_log(None, None, None, 3).expect("log");
    assert_eq!(
        kinds(&last_three),
        vec![
            ("work.added".to_string(), Some("t1".to_string())),
            ("work.added".to_string(), Some("t2".to_string())),
            ("work.claimed".to_string(), Some("t1".to_string())),
        ]
    );
    let newest = s.work_log(None, None, None, 1).expect("log");
    assert_eq!(
        kinds(&newest),
        vec![("work.claimed".to_string(), Some("t1".to_string()))]
    );
}

#[test]
fn the_log_filtered_to_one_task_lists_only_its_events() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({}));
    queue_at(s, alice, "t2", NOW + 1, json!({}));
    s.work_claim(NOW + 2, &op_at(bob, "work.claim", "t1", "{}", NOW + 2))
        .expect("claim");
    let only_t2 = s.work_log(Some("t2"), None, None, 10).expect("log");
    assert_eq!(
        kinds(&only_t2),
        vec![("work.added".to_string(), Some("t2".to_string()))]
    );
}

// ---- plan ------------------------------------------------------------------------------------------------------

#[test]
fn the_plan_orders_only_the_queued_tasks_by_policy_as_json() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue_at(s, alice, "t1", NOW, json!({"kind": "feature"}));
    queue_at(s, alice, "t2", NOW + 1, json!({"kind": "security"}));
    queue_at(
        s,
        alice,
        "t3",
        NOW + 2,
        json!({"kind": "feature", "after": ["t1"]}),
    );
    queue_at(s, alice, "t4", NOW + 3, json!({"kind": "feature"}));
    claim(s, bob, "t4");
    let (text, _) = s
        .work_plan(Some(PROJ), OrderPolicy::Fifo, &Mode::Order)
        .expect("plan");
    let plan: Value = serde_json::from_str(&text).expect("plan json");
    assert_eq!(
        plan,
        json!({"policy": "fifo", "order": [
            {"id": "t2", "tier": 0, "moved_up_for": null},
            {"id": "t1", "tier": 1, "moved_up_for": null},
            {"id": "t3", "tier": 1, "moved_up_for": null},
        ]})
    );
}

// ---- holders ---------------------------------------------------------------------------------------------------

#[test]
fn holders_list_claimed_files_and_active_path_leases_but_not_queued_tasks() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"files": ["/proj/src/a.rs"]}));
    claim(s, bob, "t1");
    queue(s, alice, "t2", json!({"files": ["/proj/src/b.rs"]}));
    let lease = path_lease(s, alice, "/proj/docs", None);
    assert_eq!(
        s.work_holders(NOW, None).expect("holders"),
        vec![
            ("t1".to_string(), vec!["/proj/src/a.rs".to_string()]),
            (
                format!("lease:{}", lease.id),
                vec!["/proj/docs".to_string()]
            ),
        ]
    );
}

#[test]
fn excluding_an_identity_leaves_out_its_own_path_leases_only() {
    let mut w = world();
    project(&mut w);
    let (s, alice, bob) = (&mut w.s, &w.alice, &w.bob);
    queue(s, alice, "t1", json!({"files": ["/proj/src/a.rs"]}));
    claim(s, bob, "t1");
    path_lease(s, alice, "/proj/docs", None);
    let bobs = path_lease(s, bob, "/proj/out", None);
    let alice_pk = pk_hex(alice);
    let holders = s
        .work_holders(NOW, Some(alice_pk.as_str()))
        .expect("holders");
    assert_eq!(
        holders,
        vec![
            ("t1".to_string(), vec!["/proj/src/a.rs".to_string()]),
            (format!("lease:{}", bobs.id), vec!["/proj/out".to_string()]),
        ]
    );
}

#[test]
fn a_path_lease_stops_holding_at_its_expiry() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    path_lease(s, alice, "/proj/docs", Some(60.0));
    let just_before = s.work_holders(NOW + 59_999, None).expect("holders");
    assert_eq!(just_before.len(), 1);
    let at_expiry = s.work_holders(NOW + 60_000, None).expect("holders");
    assert!(at_expiry.is_empty());
}
