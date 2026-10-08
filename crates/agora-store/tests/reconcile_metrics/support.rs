//! Fixtures for reconcile_metrics.rs: the daemon's system-signed calls, the card and job drivers the tests share, a
//! scripted history for the flow metrics, and a file-backed store for the few checks that read the audit tables.

use crate::common::*;
use agora_crypto::Identity;
use agora_sched::model::TaskRecord;
use agora_sched::predict::Run;
use agora_sched::reconcile::ReconcileAction;
use agora_sched::signature::Signatures;
use agora_store::{SignedOp, Store};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub const HOUR: i64 = 3_600_000;
pub const SEC: i64 = 1_000;
pub const LOG_DIR: &str = "/var/agora/logs";

/// The instant `h` hours after NOW.
pub fn at(h: i64) -> i64 {
    NOW + h * HOUR
}

/// The instant `s` seconds after the hour `h`.
pub fn at_s(h: i64, s: i64) -> i64 {
    at(h) + s * SEC
}

/// The system identity the daemon signs with, registered as the store's one system identity.
pub fn system(w: &mut World) -> Identity {
    let sys = Identity::from_seed([12; 32]);
    w.s.register_system_identity(NOW, &sys.public())
        .expect("register system identity");
    sys
}

/// A system-signed op at `ts`; the store call that consumes it runs at the same instant.
pub fn sys_op(sys: &Identity, kind: &str, subject: &str, arg: &Value, ts: i64) -> SignedOp {
    op_at(sys, kind, subject, &arg.to_string(), ts)
}

/// The system-signed `reconcile.*` op for one planned action, at `ts`.
pub fn act(sys: &Identity, action: &ReconcileAction, ts: i64) -> SignedOp {
    let arg = serde_json::to_string(action).expect("action JSON");
    op_at(sys, action.op_kind(), "", &arg, ts)
}

/// A card created by the supervisor at `ts`, in `team` and `lane`.
pub fn card_at(s: &mut Store, sup: &Identity, id: &str, team: &str, lane: &str, ts: i64) {
    let arg = json!({"title": id, "team": team, "lane": lane}).to_string();
    s.create_card(ts, &op_at(sup, "card.create", id, &arg, ts))
        .expect("create card");
}

/// Moves a card as `who` at `ts`, without the review gate.
pub fn move_card(s: &mut Store, who: &Identity, id: &str, to: &str, ts: i64) {
    s.transition(ts, &op_at(who, "card.move", id, to, ts), false)
        .expect("move card");
}

/// Adds evidence to a card as `who` at `ts`: the gate for a move to done.
pub fn evidence(s: &mut Store, who: &Identity, id: &str, ts: i64) {
    s.add_evidence(ts, &op_at(who, "card.evidence", id, "checked", ts))
        .expect("add evidence");
}

/// `who` runs an inline command named `id` at `ts`, which claims and requests it. Linked to `card` when given.
pub fn request_job(
    s: &mut Store,
    who: &Identity,
    id: &str,
    card: Option<&str>,
    ts: i64,
) -> TaskRecord {
    let mut add = json!({"id": id, "title": id, "command": ["true"], "cwd": "/"});
    if let Some(card) = card {
        add["card"] = json!(card);
    }
    let arg = json!({ "add": add }).to_string();
    s.work_request(ts, &op_at(who, "work.run", "", &arg, ts), 1.0, LOG_DIR)
        .expect("request job")
}

/// The daemon's `job.start` for a requested job at `ts`: the command runs as process 4242.
pub fn start_job(s: &mut Store, sys: &Identity, id: &str, ts: i64) -> TaskRecord {
    let arg = json!({"pid": 4242, "supervisor_pid": 4200, "log": format!("{LOG_DIR}/{id}.log")});
    s.job_start(ts, &sys_op(sys, "job.start", id, &arg, ts))
        .expect("start job")
}

/// The job's end as the daemon reports it at `ts`: `status` (None when the supervisor recorded none), 1.5 CPU
/// seconds and no finish-time peak. Callers change keys to vary the report.
pub fn finish_arg(status: Option<i32>, ts: i64) -> Value {
    json!({"status": status, "note": "", "cpu_seconds": 1.5, "ended_ms": ts})
}

/// The daemon's `job.finish` at `ts` with the report `arg`.
pub fn finish_job(
    s: &mut Store,
    sys: &Identity,
    id: &str,
    arg: &Value,
    ts: i64,
    sync: Option<&SignedOp>,
) -> TaskRecord {
    s.job_finish(ts, &sys_op(sys, "job.finish", id, arg, ts), sync)
        .expect("finish job")
}

/// Every ran job in `job_history`, newest first. A weight floor of 0 admits every run to the exclusive list, which
/// is the only public read of the history rows (their signatures are not needed for it).
pub fn ran_jobs(s: &Store) -> Vec<Run> {
    let sigs = Signatures {
        full: String::new(),
        head: String::new(),
        prog: String::new(),
        tool: String::new(),
    };
    s.work_history(&sigs, 0.0).expect("history").exclusive_runs
}

/// The scripted history behind the metrics tests. Times are hours after NOW; the metrics window is [10 h, 100 h].
///
/// Cards (team/lane, timeline):
/// - a core/alpha: created 20, ready 21, claimed 22, doing 23, review 25, evidence 29, done 30.
/// - b core/beta: created 15, ready 16, claimed 17, doing 18, blocked 35, ready 38, claimed 39, doing 40,
///   evidence 41, done 42.
/// - c ops/alpha: created 60, ready 61, claimed 62, doing 63, blocked 90 (still blocked at 100 h).
/// - d core/alpha: created 50, ready 51, claimed 52, doing 55, review 58, evidence 65, done 66.
/// - e core/beta: created 1, ready 2, claimed 3, doing 4, blocked 5, ready 6.
/// - f core/alpha: created 1, ready 2, claimed 3, doing 4, blocked 8, ready 12.
///
/// Jobs, all run by `alice` (requested at the hour, started after the given seconds):
/// - j0: requested 1 h, started 5 s later, ended 2 h, status 0 (before the window).
/// - j1: requested 20 h, started 30 s later, ended 21 h, status 0.
/// - j2: requested 25 h, started 90 s later, ended 26 h, status 3.
/// - j3: requested 50 h, started 600 s later, ended 51 h, status 0.
/// - j4: requested 60 h, started 10 s later, cancelled by the supervisor 20 s after the start, ended 61 h, status 143.
pub fn scripted_history(w: &mut World, sys: &Identity) {
    let s = &mut w.s;
    let sup = &w.sup;
    let alice = &w.alice;

    card_at(s, sup, "a", "core", "alpha", at(20));
    move_card(s, sup, "a", "ready", at(21));
    move_card(s, sup, "a", "claimed", at(22));
    move_card(s, sup, "a", "doing", at(23));
    move_card(s, sup, "a", "review", at(25));
    evidence(s, sup, "a", at(29));
    move_card(s, sup, "a", "done", at(30));

    card_at(s, sup, "b", "core", "beta", at(15));
    move_card(s, sup, "b", "ready", at(16));
    move_card(s, sup, "b", "claimed", at(17));
    move_card(s, sup, "b", "doing", at(18));
    move_card(s, sup, "b", "blocked", at(35));
    move_card(s, sup, "b", "ready", at(38));
    move_card(s, sup, "b", "claimed", at(39));
    move_card(s, sup, "b", "doing", at(40));
    evidence(s, sup, "b", at(41));
    move_card(s, sup, "b", "done", at(42));

    card_at(s, sup, "c", "ops", "alpha", at(60));
    move_card(s, sup, "c", "ready", at(61));
    move_card(s, sup, "c", "claimed", at(62));
    move_card(s, sup, "c", "doing", at(63));
    move_card(s, sup, "c", "blocked", at(90));

    card_at(s, sup, "d", "core", "alpha", at(50));
    move_card(s, sup, "d", "ready", at(51));
    move_card(s, sup, "d", "claimed", at(52));
    move_card(s, sup, "d", "doing", at(55));
    move_card(s, sup, "d", "review", at(58));
    evidence(s, sup, "d", at(65));
    move_card(s, sup, "d", "done", at(66));

    card_at(s, sup, "e", "core", "beta", at(1));
    move_card(s, sup, "e", "ready", at(2));
    move_card(s, sup, "e", "claimed", at(3));
    move_card(s, sup, "e", "doing", at(4));
    move_card(s, sup, "e", "blocked", at(5));
    move_card(s, sup, "e", "ready", at(6));

    card_at(s, sup, "f", "core", "alpha", at(1));
    move_card(s, sup, "f", "ready", at(2));
    move_card(s, sup, "f", "claimed", at(3));
    move_card(s, sup, "f", "doing", at(4));
    move_card(s, sup, "f", "blocked", at(8));
    move_card(s, sup, "f", "ready", at(12));

    request_job(s, alice, "j0", None, at(1));
    start_job(s, sys, "j0", at_s(1, 5));
    finish_job(s, sys, "j0", &finish_arg(Some(0), at(2)), at(2), None);

    request_job(s, alice, "j1", None, at(20));
    start_job(s, sys, "j1", at_s(20, 30));
    finish_job(s, sys, "j1", &finish_arg(Some(0), at(21)), at(21), None);

    request_job(s, alice, "j2", None, at(25));
    start_job(s, sys, "j2", at_s(25, 90));
    finish_job(s, sys, "j2", &finish_arg(Some(3), at(26)), at(26), None);

    request_job(s, alice, "j3", None, at(50));
    start_job(s, sys, "j3", at_s(50, 600));
    finish_job(s, sys, "j3", &finish_arg(Some(0), at(51)), at(51), None);

    request_job(s, alice, "j4", None, at(60));
    start_job(s, sys, "j4", at_s(60, 10));
    s.work_cancel(
        at_s(60, 20),
        &op_at(sup, "work.cancel", "j4", "{}", at_s(60, 20)),
    )
    .expect("cancel running job");
    finish_job(s, sys, "j4", &finish_arg(Some(143), at(61)), at(61), None);
}

/// A directory under /tmp for a file-backed store, removed when dropped.
pub struct Scratch(PathBuf);

static SCRATCH: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    pub fn fresh() -> Self {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/agora-m5-rm-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Scratch(dir)
    }

    pub fn db(&self) -> PathBuf {
        self.0.join("store.db")
    }

    /// A second connection, for reading the audit tables the store API does not expose.
    pub fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).expect("open the store file")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The same roster as `common::world()`, on a file-backed store.
pub fn disk_world(scratch: &Scratch) -> World {
    let mut s = Store::open(&scratch.db()).expect("open the store file");
    let owner = Identity::from_seed([1; 32]);
    let sup = Identity::from_seed([2; 32]);
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    let carol = Identity::from_seed([7; 32]);
    let olive = Identity::from_seed([8; 32]);
    let eve = Identity::from_seed([6; 32]);
    s.bootstrap_owner(NOW, &owner.public())
        .expect("bootstrap owner");
    for (id, label) in [
        (&sup, "sup"),
        (&alice, "alice"),
        (&bob, "bob"),
        (&carol, "carol"),
        (&olive, "olive"),
    ] {
        s.register_identity(NOW, &id.public(), label)
            .expect("register identity");
    }
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&sup), "supervisor"))
        .expect("grant supervisor");
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&olive), "observer"))
        .expect("grant observer");
    World {
        s,
        owner,
        sup,
        alice,
        bob,
        carol,
        olive,
        eve,
    }
}
