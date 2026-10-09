//! Public-API tests of the card extensions (SPEC §8 cards_ext.rs; D4, D7, R3-2, R3-3, R3-4, R3-9): fields at create and
//! update, dependencies, links, escalation and its caps, the WIP gate, the board summary, and the card side of a job's
//! end (`job_finish` with a `card.job_sync` in the same transaction).

#[path = "../common/mod.rs"]
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
mod board;
mod create;
mod escalate;
mod jobs;

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
