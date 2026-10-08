//! Public-API tests of the card extensions (SPEC §8 cards_ext.rs; D4, D7, R3-2, R3-3, R3-4, R3-9): fields at create and
//! update, dependencies, links, escalation and its caps, the WIP gate, the board summary, and the card side of a job's
//! end (`job_finish` with a `card.job_sync` in the same transaction).

mod common;
use common::*;

use agora_core::{CardState, Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::model::{
    CardExt, CardLink, LinkRel, NeedsOwner, NoticeRecord, QuietView, TaskRecord, TaskState, WipView,
};
use agora_sched::model_ops::{CardSyncArgs, JobFinishArgs, JobOutcome, JobStartArgs};
use agora_store::*;
use serde_json::{Value, json};
use std::fmt::Debug;
use std::path::{Path, PathBuf};

const TITLE: &str = "Port scheduler";
const LOG_DIR: &str = "/var/agora/logs";
/// D5 digest of argv ["touch","/tmp/x"] in "/" with no files, as pinned in tests/work.rs.
const TOUCH_DIGEST: &str = "059ca237bc078272521caa7a46631c6869a704487b8fed20619a2d58b43e7323";

// ---- identities and stores -------------------------------------------------------------------------------------

/// The identities a card test acts as: the owner, a supervisor, three workers and the system identity.
struct Crew {
    owner: Identity,
    sup: Identity,
    alice: Identity,
    bob: Identity,
    carol: Identity,
    sys: Identity,
}

/// A store holding the `world()` cast, plus the system identity the daemon signs with.
fn in_memory() -> (Store, Crew) {
    let World {
        mut s,
        owner,
        sup,
        alice,
        bob,
        carol,
        ..
    } = world();
    let sys = Identity::from_seed([10; 32]);
    s.register_system_identity(NOW, &sys.public())
        .expect("setup: system identity");
    (
        s,
        Crew {
            owner,
            sup,
            alice,
            bob,
            carol,
            sys,
        },
    )
}

/// The same cast on an on-disk store, for the one test that reads the evidence table, which the API does not expose.
fn on_disk(path: &Path) -> (Store, Crew) {
    let mut s = Store::open(path).expect("setup: open store");
    let c = Crew {
        owner: Identity::from_seed([1; 32]),
        sup: Identity::from_seed([2; 32]),
        alice: Identity::from_seed([3; 32]),
        bob: Identity::from_seed([4; 32]),
        carol: Identity::from_seed([7; 32]),
        sys: Identity::from_seed([10; 32]),
    };
    s.bootstrap_owner(NOW, &c.owner.public())
        .expect("setup: owner");
    for (id, label) in [
        (&c.sup, "sup"),
        (&c.alice, "alice"),
        (&c.bob, "bob"),
        (&c.carol, "carol"),
    ] {
        s.register_identity(NOW, &id.public(), label)
            .expect("setup: register");
    }
    s.grant_role(
        NOW,
        &op(&c.owner, "role.grant", &pk_hex(&c.sup), "supervisor"),
    )
    .expect("setup: supervisor");
    s.register_system_identity(NOW, &c.sys.public())
        .expect("setup: system identity");
    (s, c)
}

/// A controller, registered and granted by the owner. The World has none.
fn controller(s: &mut Store, owner: &Identity) -> Identity {
    let ctl = Identity::from_seed([9; 32]);
    s.register_identity(NOW, &ctl.public(), "ctl")
        .expect("setup: controller");
    s.grant_role(NOW, &op(owner, "role.grant", &pk_hex(&ctl), "controller"))
        .expect("setup: grant controller");
    ctl
}

/// Removes the database file and its WAL files when dropped, so a failed test leaves nothing behind.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let db = TempDb(PathBuf::from(format!("/tmp/agora-cards-ext-{name}.db")));
        db.remove_files();
        db
    }

    fn remove_files(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove_files();
    }
}

// ---- outcome helpers -------------------------------------------------------------------------------------------

/// The message of a failed operation, whatever its kind; a success fails the test.
fn failure<T: Debug>(result: Result<T, StoreError>) -> String {
    match result {
        Err(e) => e.to_string(),
        Ok(value) => panic!("expected a failure, got {value:?}"),
    }
}

/// The role named by a forbidden-transition error; any other outcome fails the test.
fn forbidden<T: Debug>(result: Result<T, StoreError>) -> Role {
    match result {
        Err(StoreError::Transition(TransitionError::Forbidden(role))) => role,
        other => panic!("expected a forbidden error, got {other:?}"),
    }
}

/// The subject of a not-found error; any other outcome fails the test.
fn not_found<T: Debug>(result: Result<T, StoreError>) -> String {
    match result {
        Err(StoreError::NotFound(what)) => what,
        other => panic!("expected not found, got {other:?}"),
    }
}

// ---- card builders ---------------------------------------------------------------------------------------------

/// The `card.create` arg: the title plus the extra fields.
fn card_arg(extra: Value) -> String {
    let mut arg = json!({ "title": TITLE });
    if let (Some(map), Value::Object(more)) = (arg.as_object_mut(), extra) {
        map.extend(more);
    }
    arg.to_string()
}

/// Creates card `id` as the supervisor.
fn new_card(s: &mut Store, sup: &Identity, id: &str, extra: Value) {
    s.create_card(NOW, &op(sup, "card.create", id, &card_arg(extra)))
        .expect("setup: card.create");
}

fn move_card(s: &mut Store, who: &Identity, id: &str, to: &str) {
    s.transition(NOW, &op(who, "card.move", id, to), false)
        .expect("setup: card.move");
}

/// Card `id` made ready by the supervisor, then claimed by `who`.
fn claim_card(s: &mut Store, sup: &Identity, who: &Identity, id: &str) {
    move_card(s, sup, id, "ready");
    move_card(s, who, id, "claimed");
}

/// Card `c1` claimed by alice and moved to doing, so that alice owns work in progress.
fn card_in_doing(s: &mut Store, c: &Crew) {
    new_card(s, &c.sup, "c1", json!({}));
    claim_card(s, &c.sup, &c.alice, "c1");
    move_card(s, &c.alice, "c1", "doing");
}

fn ext_of(s: &Store, id: &str) -> CardExt {
    s.card_ext(&[id.to_string()]).expect("card ext").remove(0)
}

fn after_of(s: &Store, id: &str) -> Vec<String> {
    ext_of(s, id).after
}

fn dep(
    s: &mut Store,
    who: &Identity,
    card: &str,
    after: &str,
    remove: bool,
) -> Result<(), StoreError> {
    let arg = json!({ "after": after, "remove": remove }).to_string();
    s.card_dep(NOW, &op(who, "card.dep", card, &arg))
}

fn link(
    s: &mut Store,
    who: &Identity,
    card: &str,
    rel: &str,
    target: &str,
    remove: bool,
) -> Result<(), StoreError> {
    let arg = json!({ "rel": rel, "target": target, "remove": remove }).to_string();
    s.card_link(NOW, &op(who, "card.link", card, &arg))
}

fn escalate(
    s: &mut Store,
    who: &Identity,
    id: &str,
    action: &str,
    to: &[String],
) -> Result<Vec<String>, StoreError> {
    let arg = json!({ "owner_action": action, "to": to }).to_string();
    s.card_escalate(NOW, &op(who, "card.escalate", id, &arg), false)
}

fn notices_for(s: &Store, who: &Identity) -> Vec<NoticeRecord> {
    s.notices(&pk_hex(who), 0, 100).expect("notices")
}

/// The one notice `who` holds; any other count fails the test.
fn only_notice(s: &Store, who: &Identity) -> NoticeRecord {
    let mut found = notices_for(s, who);
    assert_eq!(found.len(), 1, "notices for {}", pk_hex(who));
    found.remove(0)
}

fn set_policy(s: &mut Store, who: &Identity, team: &str, lane: &str, policy: Value) {
    let arg = json!({ "team": team, "lane": lane, "policy": policy }).to_string();
    s.policy_set(NOW, &op(who, "policy.set", &format!("{team}/{lane}"), &arg))
        .expect("setup: policy.set");
}

// ---- job builders ----------------------------------------------------------------------------------------------

/// Queues `id` as `author`, linked to `card` when given. The command is argv only, so its digest is fixed.
fn queue_task(s: &mut Store, author: &Identity, id: &str, card: Option<&str>) {
    let mut args = json!({"id": id, "title": id, "command": ["touch", "/tmp/x"], "cwd": "/"});
    if let Some(card) = card {
        args["card"] = json!(card);
    }
    s.work_add(NOW, &op(author, "work.add", "", &args.to_string()))
        .expect("setup: work.add");
}

/// Claims `id` as `who`, accepts its command and runs it: the task now waits for the daemon to start it.
fn claim_accept_run(s: &mut Store, who: &Identity, id: &str) {
    s.work_claim(NOW, &op(who, "work.claim", id, "{}"))
        .expect("setup: work.claim");
    let accept = json!({ "digest": TOUCH_DIGEST }).to_string();
    s.work_accept(NOW, &op(who, "work.accept", id, &accept))
        .expect("setup: work.accept");
    let run = json!({ "task": id }).to_string();
    s.work_request(NOW, &op(who, "work.run", "", &run), 4.0, LOG_DIR)
        .expect("setup: work.run");
}

/// The daemon's `job.start` for a waiting task: the command runs as process 4242 under supervisor 4241.
fn start_job(s: &mut Store, sys: &Identity, id: &str) {
    let arg = JobStartArgs {
        pid: 4242,
        supervisor_pid: 4241,
        supervisor_start_s: Some(1_700_000_000),
        qos: String::new(),
        exclusive: false,
        quiet_until_ms: None,
        log: format!("{LOG_DIR}/{id}.log"),
    };
    s.job_start(
        NOW,
        &op(
            sys,
            "job.start",
            id,
            &serde_json::to_string(&arg).expect("arg"),
        ),
    )
    .expect("setup: job.start");
}

/// Card c1 in doing under alice, with task t1 queued by alice against it.
fn doing_card_with_task(s: &mut Store, c: &Crew) {
    card_in_doing(s, c);
    queue_task(s, &c.alice, "t1", Some("c1"));
}

/// Carol claims, accepts, runs and starts task `id`.
fn start_as_carol(s: &mut Store, c: &Crew, id: &str) {
    claim_accept_run(s, &c.carol, id);
    start_job(s, &c.sys, id);
}

fn sync_args(task: &str, outcome: JobOutcome, status: Option<i32>, note: &str) -> CardSyncArgs {
    CardSyncArgs {
        task: task.to_string(),
        outcome,
        status,
        log: Some(format!("{LOG_DIR}/{task}.log")),
        note: note.to_string(),
    }
}

/// The daemon's `job.finish` for t1 with its `card.job_sync`, both signed by the system identity.
fn finish_with_sync(
    s: &mut Store,
    sys: &Identity,
    task: &str,
    status: Option<i32>,
    sync: &CardSyncArgs,
) -> Result<TaskRecord, StoreError> {
    let finish = JobFinishArgs {
        status,
        note: String::new(),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: NOW + 5_000,
        before_start: false,
    };
    let fin = op(
        sys,
        "job.finish",
        task,
        &serde_json::to_string(&finish).expect("arg"),
    );
    let sync_op = op(
        sys,
        "card.job_sync",
        task,
        &serde_json::to_string(sync).expect("arg"),
    );
    s.job_finish(NOW, &fin, Some(&sync_op))
}

/// The owner of card `card`, read from the board listing of its state.
fn owner_of(s: &Store, card: &str, state: CardState) -> Option<String> {
    s.list_cards(Some(state), None, 10)
        .expect("list cards")
        .into_iter()
        .find(|row| row.id == card)
        .expect("card in that state")
        .owner
}

// ---- create ----------------------------------------------------------------------------------------------------

#[test]
fn card_create_stores_priority_value_size_team_and_prerequisites() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(
        &mut s,
        &c.sup,
        "c2",
        json!({"priority": 5, "value": 3.5, "size": 2.0, "team": "core", "after": ["c1"]}),
    );
    assert_eq!(
        s.card_ext(&["c2".to_string()]).expect("card ext"),
        vec![CardExt {
            id: "c2".into(),
            priority: 5,
            value: 3.5,
            size: 2.0,
            team: "core".into(),
            needs_owner: false,
            owner_action: String::new(),
            after: vec!["c1".into()],
            links: vec![],
            jobs: vec![],
        }]
    );
}

#[test]
fn card_create_refuses_invalid_fields_and_stores_no_card() {
    let (mut s, c) = in_memory();
    let cases: [(Value, &str); 9] = [
        (
            json!({"priority": "high"}),
            "card priority must be an integer",
        ),
        (json!({"value": 0}), "card value must be a positive number"),
        (
            json!({"value": -1.5}),
            "card value must be a positive number",
        ),
        (json!({"size": 0}), "card size must be a positive number"),
        (
            json!({"team": "t".repeat(65)}),
            "card team must be at most 64 characters",
        ),
        (json!({"team": 7}), "card team must be a string"),
        (
            json!({"after": "c1"}),
            "card after must be a list of card ids",
        ),
        (json!({"after": ["c9"]}), "a card cannot depend on itself"),
        (json!({"after": ["nope"]}), "nope not found"),
    ];
    for (extra, message) in cases {
        let arg = card_arg(extra.clone());
        let err = failure(s.create_card(NOW, &op(&c.sup, "card.create", "c9", &arg)));
        assert_eq!(err, message, "for {extra}");
    }
    assert_eq!(not_found(s.card_ext(&["c9".to_string()])), "c9");
}

// ---- update ----------------------------------------------------------------------------------------------------

#[test]
fn card_update_by_the_owner_changes_priority_value_size_and_team() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 3, "value": 2.5, "size": 4.0, "team": "core"}).to_string();
    let updated = s
        .card_update(NOW, &op(&c.alice, "card.update", "c1", &arg))
        .expect("owner update");
    assert_eq!(
        (
            updated.priority,
            updated.value,
            updated.size,
            updated.team.as_str()
        ),
        (3, 2.5, 4.0, "core")
    );
    assert_eq!(ext_of(&s, "c1"), updated);
}

#[test]
fn card_update_by_another_worker_is_forbidden_even_for_priority() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 9}).to_string();
    let role = forbidden(s.card_update(NOW, &op(&c.bob, "card.update", "c1", &arg)));
    assert_eq!(role, Role::Worker);
    assert_eq!(ext_of(&s, "c1").priority, 0);
}

#[test]
fn a_supervisor_may_update_a_card_another_worker_owns() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"priority": 9}).to_string();
    let updated = s
        .card_update(NOW, &op(&c.sup, "card.update", "c1", &arg))
        .expect("supervisor update");
    assert_eq!(updated.priority, 9);
}

#[test]
fn card_update_refuses_a_non_positive_value_and_changes_nothing() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let arg = json!({"value": 0.0}).to_string();
    assert_eq!(
        failure(s.card_update(NOW, &op(&c.alice, "card.update", "c1", &arg))),
        "card value must be a positive number"
    );
    assert_eq!(ext_of(&s, "c1").value, 1.0);
}

// ---- dependencies ----------------------------------------------------------------------------------------------

#[test]
fn card_dep_adds_then_removes_a_prerequisite() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    dep(&mut s, &c.sup, "c1", "c2", false).expect("add");
    assert_eq!(after_of(&s, "c1"), vec!["c2"]);
    dep(&mut s, &c.sup, "c1", "c2", true).expect("remove");
    assert!(after_of(&s, "c1").is_empty());
}

#[test]
fn card_dep_refuses_a_cycle_and_keeps_the_graph_unchanged() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    dep(&mut s, &c.sup, "c1", "c2", false).expect("c1 after c2");
    assert_eq!(
        failure(dep(&mut s, &c.sup, "c2", "c1", false)),
        "card dependencies would form a cycle"
    );
    assert!(after_of(&s, "c2").is_empty());
    assert_eq!(after_of(&s, "c1"), vec!["c2"]);
}

#[test]
fn card_dep_on_an_unknown_card_is_not_found() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    assert_eq!(not_found(dep(&mut s, &c.sup, "c1", "nope", false)), "nope");
}

#[test]
fn card_dep_removal_of_a_missing_prerequisite_is_refused() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    new_card(&mut s, &c.sup, "c2", json!({}));
    assert_eq!(
        failure(dep(&mut s, &c.sup, "c1", "c2", true)),
        "card has no such dependency"
    );
}

// ---- links -----------------------------------------------------------------------------------------------------

#[test]
fn card_link_adds_and_removes_each_link_rel() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    let rels = [
        ("re", LinkRel::Re),
        ("blocks", LinkRel::Blocks),
        ("relates", LinkRel::Relates),
        ("duplicates", LinkRel::Duplicates),
        ("caused_by", LinkRel::CausedBy),
        ("job", LinkRel::Job),
        ("message", LinkRel::Message),
        ("note", LinkRel::Note),
    ];
    for (name, rel) in rels {
        link(&mut s, &c.sup, "c1", name, "c2", false).expect(name);
        assert_eq!(
            ext_of(&s, "c1").links,
            vec![CardLink {
                rel,
                target: "c2".into()
            }],
            "{name} is stored"
        );
        link(&mut s, &c.sup, "c1", name, "c2", true).expect(name);
        assert!(ext_of(&s, "c1").links.is_empty(), "{name} is removed");
    }
}

#[test]
fn card_link_refuses_a_repeated_link() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    link(&mut s, &c.sup, "c1", "relates", "c2", false).expect("first");
    assert_eq!(
        failure(link(&mut s, &c.sup, "c1", "relates", "c2", false)),
        "card already has that link"
    );
}

#[test]
fn card_link_removal_of_a_missing_link_is_refused() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    assert_eq!(
        failure(link(&mut s, &c.sup, "c1", "blocks", "c2", true)),
        "card has no such link"
    );
}

#[test]
fn card_ext_reports_defaults_prerequisites_links_and_the_jobs_linked_to_the_card() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c0", json!({}));
    new_card(&mut s, &c.sup, "c1", json!({}));
    dep(&mut s, &c.sup, "c1", "c0", false).expect("dep");
    link(&mut s, &c.sup, "c1", "relates", "c2", false).expect("link");
    queue_task(&mut s, &c.sup, "t1", Some("c1"));
    assert_eq!(
        s.card_ext(&["c1".to_string()]).expect("card ext"),
        vec![CardExt {
            id: "c1".into(),
            priority: 0,
            value: 1.0,
            size: 1.0,
            team: String::new(),
            needs_owner: false,
            owner_action: String::new(),
            after: vec!["c0".into()],
            links: vec![CardLink {
                rel: LinkRel::Relates,
                target: "c2".into()
            }],
            jobs: vec!["t1".into()],
        }]
    );
}

// ---- escalation ------------------------------------------------------------------------------------------------

#[test]
fn escalate_by_the_owner_marks_the_card_and_stores_the_owner_action() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    escalate(&mut s, &c.alice, "c1", "decide the API shape", &[]).expect("owner escalates");
    let ext = ext_of(&s, "c1");
    assert!(ext.needs_owner);
    assert_eq!(ext.owner_action, "decide the API shape");
}

#[test]
fn escalate_is_allowed_to_the_owner_and_supervisors_but_forbidden_to_other_workers() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    assert_eq!(
        forbidden(escalate(&mut s, &c.bob, "c1", "help", &[])),
        Role::Worker
    );
    assert!(!ext_of(&s, "c1").needs_owner);
    escalate(&mut s, &c.sup, "c1", "help", &[]).expect("supervisor escalates");
    assert!(ext_of(&s, "c1").needs_owner);
}

#[test]
fn escalate_without_targets_or_chain_notifies_every_supervisor_and_no_one_else() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[]).expect("escalate");
    let mut expected = vec![pk_hex(&c.owner), pk_hex(&c.sup)];
    expected.sort();
    assert_eq!(targets, expected);
    for who in [&c.owner, &c.sup] {
        let notice = only_notice(&s, who);
        assert_eq!(notice.kind, "escalation");
        assert_eq!(notice.subject, "c1");
        assert!(notice.text.contains("decide"), "{}", notice.text);
    }
    assert!(notices_for(&s, &c.bob).is_empty());
}

#[test]
fn escalate_without_targets_uses_the_policy_chain_instead_of_the_supervisors() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    set_policy(
        &mut s,
        &ctl,
        "core",
        "default",
        json!({"escalation_chain": [pk_hex(&c.carol)]}),
    );
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[]).expect("escalate");
    assert_eq!(targets, vec![pk_hex(&c.carol)]);
    assert_eq!(only_notice(&s, &c.carol).kind, "escalation");
    assert!(notices_for(&s, &c.sup).is_empty());
}

#[test]
fn escalate_with_explicit_targets_notifies_them_instead_of_the_policy_chain() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    set_policy(
        &mut s,
        &ctl,
        "core",
        "default",
        json!({"escalation_chain": [pk_hex(&c.carol)]}),
    );
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let targets = escalate(&mut s, &c.alice, "c1", "decide", &[pk_hex(&c.sup)]).expect("escalate");
    assert_eq!(targets, vec![pk_hex(&c.sup)]);
    assert_eq!(only_notice(&s, &c.sup).kind, "escalation");
    assert!(notices_for(&s, &c.carol).is_empty());
}

#[test]
fn escalate_refuses_an_owner_action_longer_than_1000_characters() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let too_long = "a".repeat(1001);
    assert_eq!(
        failure(escalate(&mut s, &c.alice, "c1", &too_long, &[])),
        "owner action is longer than 1000 characters"
    );
    assert!(!ext_of(&s, "c1").needs_owner);
    let at_cap = "a".repeat(1000);
    escalate(&mut s, &c.alice, "c1", &at_cap, &[]).expect("1000 characters is allowed");
    assert_eq!(ext_of(&s, "c1").owner_action.chars().count(), 1000);
}

#[test]
fn escalate_accepts_at_most_16_explicit_targets() {
    let (mut s, c) = in_memory();
    let mut keys = Vec::new();
    for i in 0u8..16 {
        let id = Identity::from_seed([0x40 + i; 32]);
        s.register_identity(NOW, &id.public(), &format!("target{i}"))
            .expect("setup: register target");
        keys.push(pk_hex(&id));
    }
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    let mut seventeen = keys.clone();
    seventeen.push(pk_hex(&c.sup));
    assert_eq!(
        failure(escalate(&mut s, &c.alice, "c1", "decide", &seventeen)),
        "too many escalation targets"
    );
    let notified = escalate(&mut s, &c.alice, "c1", "decide", &keys).expect("16 targets");
    assert_eq!(notified.len(), 16);
}

#[test]
fn escalate_refuses_a_revoked_or_system_identity_as_a_target() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.carol), ""))
        .expect("setup: revoke carol");
    for key in [pk_hex(&c.carol), pk_hex(&c.sys)] {
        assert_eq!(
            failure(escalate(
                &mut s,
                &c.alice,
                "c1",
                "decide",
                std::slice::from_ref(&key)
            )),
            "escalation target is not an active identity",
            "target {key}"
        );
    }
    assert!(!ext_of(&s, "c1").needs_owner);
}

#[test]
fn escalate_refuses_a_target_that_is_not_a_public_key() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    assert_eq!(
        failure(escalate(
            &mut s,
            &c.alice,
            "c1",
            "decide",
            &["NOT-A-KEY".to_string()]
        )),
        "escalation target must be a 64-character lowercase hex public key"
    );
}

// ---- WIP gate --------------------------------------------------------------------------------------------------

#[test]
fn wip_limit_refuses_a_claim_that_would_pass_the_lane_limit() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 1}));
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c2", json!({"team": "core"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    move_card(&mut s, &c.sup, "c2", "ready");
    assert_eq!(
        failure(s.transition(NOW, &op(&c.bob, "card.move", "c2", "claimed"), false)),
        "wip limit reached: lane core/default has 1 of 1"
    );
    assert_eq!(s.card_state("c2").expect("state"), CardState::Ready);
}

#[test]
fn a_card_already_claimed_in_an_over_limit_lane_can_still_move_to_doing() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 2}));
    for id in ["c1", "c2", "c3"] {
        new_card(&mut s, &c.sup, id, json!({"team": "core"}));
        move_card(&mut s, &c.sup, id, "ready");
    }
    move_card(&mut s, &c.alice, "c1", "claimed");
    move_card(&mut s, &c.bob, "c2", "claimed");
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 1}));
    assert_eq!(
        failure(s.transition(NOW, &op(&c.carol, "card.move", "c3", "claimed"), false)),
        "wip limit reached: lane core/default has 2 of 1"
    );
    move_card(&mut s, &c.alice, "c1", "doing");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}

// ---- board -----------------------------------------------------------------------------------------------------

#[test]
fn board_lists_the_cards_that_need_an_owner_with_their_action() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    escalate(&mut s, &c.alice, "c1", "decide scope", &[]).expect("escalate");
    assert_eq!(
        s.board_extras(NOW).expect("board").needs_owner,
        vec![NeedsOwner {
            card: "c1".into(),
            title: TITLE.into(),
            owner_action: "decide scope".into(),
        }]
    );
}

#[test]
fn board_reports_wip_per_lane_with_its_limit_and_shows_a_limited_idle_lane() {
    let (mut s, c) = in_memory();
    set_policy(&mut s, &c.sup, "core", "default", json!({"wip_limit": 2}));
    set_policy(&mut s, &c.sup, "core", "ops", json!({"wip_limit": 3}));
    new_card(&mut s, &c.sup, "c1", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c2", json!({"team": "core"}));
    new_card(&mut s, &c.sup, "c3", json!({"team": "core", "lane": "ops"}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    claim_card(&mut s, &c.sup, &c.bob, "c2");
    assert_eq!(
        s.board_extras(NOW).expect("board").wip,
        vec![
            WipView {
                team: "core".into(),
                lane: "default".into(),
                count: 2,
                limit: Some(2),
            },
            WipView {
                team: "core".into(),
                lane: "ops".into(),
                count: 0,
                limit: Some(3),
            },
        ]
    );
}

#[test]
fn board_counts_queued_waiting_and_running_jobs() {
    let (mut s, c) = in_memory();
    queue_task(&mut s, &c.alice, "q1", None);
    queue_task(&mut s, &c.alice, "w1", None);
    claim_accept_run(&mut s, &c.bob, "w1");
    queue_task(&mut s, &c.alice, "r1", None);
    start_as_carol(&mut s, &c, "r1");
    let board = s.board_extras(NOW).expect("board");
    assert_eq!(
        (board.queued_tasks, board.waiting_jobs, board.running_jobs),
        (1, 1, 1)
    );
}

#[test]
fn board_counts_open_leases_and_drops_one_at_its_expiry() {
    let (mut s, c) = in_memory();
    let arg = json!({"kind": "path", "resource": "/proj/docs", "ttl_s": 60.0}).to_string();
    s.lease_acquire(NOW, &op(&c.alice, "lease.acquire", "", &arg))
        .expect("setup: lease");
    assert_eq!(s.board_extras(NOW).expect("board").open_leases, 1);
    assert_eq!(s.board_extras(NOW + 60_000).expect("board").open_leases, 0);
}

#[test]
fn board_shows_the_open_manual_quiet_window_and_its_seconds_left() {
    let (mut s, c) = in_memory();
    let ctl = controller(&mut s, &c.owner);
    let arg = json!({"expected_end_ms": NOW + 90_000, "note": ""}).to_string();
    let lease = s
        .quiet_open(NOW, &op(&ctl, "quiet.open", "", &arg))
        .expect("quiet open");
    assert_eq!(
        s.board_extras(NOW).expect("board").quiet,
        Some(QuietView {
            state: "manual".into(),
            job: None,
            lease: Some(lease.id.clone()),
            seconds_left: 90.0,
            text: format!("manual quiet window {}", lease.id),
        })
    );
}

#[test]
fn board_shows_the_admission_pause_until_it_is_cleared() {
    let (mut s, c) = in_memory();
    let pause =
        json!({"action": "admission", "key": "admission:disk", "paused": "disk low"}).to_string();
    assert!(
        s.apply_reconcile(NOW, &op(&c.sys, "reconcile.admission", "", &pause))
            .expect("pause")
    );
    assert_eq!(
        s.board_extras(NOW).expect("board").paused,
        Some("disk low".to_string())
    );
    let clear =
        json!({"action": "admission", "key": "admission:clear", "paused": null}).to_string();
    s.apply_reconcile(NOW, &op(&c.sys, "reconcile.admission", "", &clear))
        .expect("clear");
    assert_eq!(s.board_extras(NOW).expect("board").paused, None);
}

// ---- job sync (D7) ---------------------------------------------------------------------------------------------

#[test]
fn a_done_job_moves_its_doing_card_to_review_and_records_the_job_as_evidence() {
    let db = TempDb::new("done");
    let (mut s, c) = on_disk(&db.0);
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Review);
    assert_eq!(
        evidence_refs(&db.0, "c1"),
        vec!["job:t1 status=0 log=/var/agora/logs/t1.log".to_string()]
    );
}

/// The evidence references of a card, read from the store's database: the API has no reader for them.
fn evidence_refs(path: &Path, card: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("open database");
    let mut st = conn
        .prepare("SELECT ref FROM evidence WHERE card=? ORDER BY ref")
        .expect("prepare");
    st.query_map([card], |r| r.get(0))
        .expect("query")
        .collect::<Result<Vec<String>, _>>()
        .expect("rows")
}

#[test]
fn a_done_job_on_a_claimed_card_passes_through_doing_to_review() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    queue_task(&mut s, &c.alice, "t1", Some("c1"));
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Review);
}

#[test]
fn a_failed_job_blocks_its_card_and_gives_the_owner_an_action() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Failed, Some(1), "boom");
    finish_with_sync(&mut s, &c.sys, "t1", Some(1), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Blocked);
    let ext = ext_of(&s, "c1");
    assert!(ext.needs_owner);
    assert_eq!(
        ext.owner_action,
        "job t1 failed with status 1: boom; inspect /var/agora/logs/t1.log and re-run or cancel"
    );
}

#[test]
fn a_cancelled_job_returns_its_card_to_ready_without_an_owner() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    s.work_cancel(
        NOW,
        &op(&c.carol, "work.cancel", "t1", r#"{"note":"stop"}"#),
    )
    .expect("cancel running job");
    let sync = sync_args("t1", JobOutcome::Cancelled, Some(143), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(143), &sync).expect("finish");
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Cancelled);
    assert_eq!(owner_of(&s, "c1", CardState::Ready), None);
}

#[test]
fn a_job_ending_after_its_card_changed_owner_leaves_the_card_and_tells_the_new_owner() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    s.reclaim_stale(NOW, &op(&c.sup, "card.reclaim", "c1", ""))
        .expect("reclaim");
    move_card(&mut s, &c.bob, "c1", "claimed");
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Done);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Claimed);
    assert_eq!(owner_of(&s, "c1", CardState::Claimed), Some(pk_hex(&c.bob)));
    let notice = only_notice(&s, &c.bob);
    assert_eq!(notice.kind, "job_sync");
    assert_eq!(notice.subject, "c1");
    assert!(
        notice.text.contains("job t1 ended done") && notice.text.contains("was not moved"),
        "{}",
        notice.text
    );
}

#[test]
fn a_sync_naming_another_job_refuses_the_whole_finish_and_the_job_keeps_running() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t9", JobOutcome::Done, Some(0), "");
    assert_eq!(
        failure(finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync)),
        "card.job_sync does not match the job it finishes"
    );
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Running);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}

#[test]
fn a_sync_whose_outcome_differs_from_the_final_state_refuses_the_whole_finish() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Failed, Some(0), "");
    assert_eq!(
        failure(finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync)),
        "card.job_sync does not match the job it finishes"
    );
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Running);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}
