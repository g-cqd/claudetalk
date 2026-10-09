//! Authorization for the M5 work-queue, timer, pool, lease, quiet, policy and card-extension operations, plus the
//! system identity (SPEC §8 work_authz.rs; D4 roles, D6 system identity, D7 card sync).
//!
//! Each role-matrix test attempts one operation as six kinds of actor on a minimal fixture, and as the system
//! identity. An allowed actor must succeed; a denied one must fail with exactly `Forbidden(its role)`.

#[path = "../common/mod.rs"]
mod common;
use common::*;

use agora_core::{CardState, Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::claims;
use agora_sched::model::TaskState;
use agora_sched::model_ops::{CardSyncArgs, JobFinishArgs, JobOutcome, JobStartArgs};
use agora_store::*;
use serde_json::json;
mod authz;
mod matrix;
mod sync;

const FIVE_MINUTES_MS: i64 = 5 * 60 * 1_000;
const LOG_DIR: &str = "/tmp/agora-authz-logs";

/// Every identity a case acts as. `World` supplies owner, sup, alice, bob, olive and eve; the controller is granted
/// by the owner through a real role.grant, and the system identity is registered through the store.
struct Cast {
    owner: Identity,
    sup: Identity,
    alice: Identity,
    bob: Identity,
    olive: Identity,
    eve: Identity,
    ctl: Identity,
    sys: Identity,
}

/// A fresh store with the whole cast in place.
fn fixture() -> (Store, Cast) {
    let World {
        mut s,
        owner,
        sup,
        alice,
        bob,
        carol: _,
        olive,
        eve,
    } = world();
    let ctl = Identity::from_seed([11; 32]);
    s.register_identity(NOW, &ctl.public(), "ctl").unwrap();
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&ctl), "controller"))
        .unwrap();
    let sys = Identity::from_seed([9; 32]);
    s.register_system_identity(NOW, &sys.public()).unwrap();
    (
        s,
        Cast {
            owner,
            sup,
            alice,
            bob,
            olive,
            eve,
            ctl,
            sys,
        },
    )
}

/// The actors of the matrix. `WorkerOwn` is the author or claimant of the resource a case works on, and
/// `WorkerOther` is a different worker.
#[derive(Clone, Copy, Debug)]
enum Actor {
    Observer,
    WorkerOwn,
    WorkerOther,
    Supervisor,
    Controller,
    Owner,
}

const ACTORS: [Actor; 6] = [
    Actor::Observer,
    Actor::WorkerOwn,
    Actor::WorkerOther,
    Actor::Supervisor,
    Actor::Controller,
    Actor::Owner,
];

impl Actor {
    fn role(self) -> Role {
        match self {
            Actor::Observer => Role::Observer,
            Actor::WorkerOwn | Actor::WorkerOther => Role::Worker,
            Actor::Supervisor => Role::Supervisor,
            Actor::Controller => Role::Controller,
            Actor::Owner => Role::Owner,
        }
    }

    fn of(self, c: &Cast) -> &Identity {
        match self {
            Actor::Observer => &c.olive,
            Actor::WorkerOwn => &c.alice,
            Actor::WorkerOther => &c.bob,
            Actor::Supervisor => &c.sup,
            Actor::Controller => &c.ctl,
            Actor::Owner => &c.owner,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Allowed,
    Forbidden,
    /// An ownership refusal (D4, O-29): Refused, with a message naming the owning agent (alice).
    Owned,
}

use Gate::{Allowed as Yes, Forbidden as No, Owned};

/// One operation attempted as `who` (an actor of the cast) against a fixture the attempt builds itself.
type Attempt = fn(&mut Store, &Cast, &Identity) -> Result<(), StoreError>;

/// Attempts `attempt` as each actor and as the system identity. The six actors follow `gates`; the system
/// identity must always be Forbidden(Observer), since no agent operation may be signed by the system.
fn assert_matrix(gates: [Gate; 6], attempt: Attempt) {
    let mut failures = Vec::new();
    for (actor, gate) in ACTORS.into_iter().zip(gates) {
        let (mut s, c) = fixture();
        let got = attempt(&mut s, &c, actor.of(&c));
        if let Some(problem) = judge(actor.role(), gate, &got, &c.alice.pseudonym()) {
            failures.push(format!("{actor:?}: {problem}"));
        }
    }
    let (mut s, c) = fixture();
    let got = attempt(&mut s, &c, &c.sys);
    if let Some(problem) = judge(Role::Observer, Gate::Forbidden, &got, &c.alice.pseudonym()) {
        failures.push(format!("System: {problem}"));
    }
    assert!(
        failures.is_empty(),
        "role matrix mismatches:\n{}",
        failures.join("\n")
    );
}

/// None when `got` matches the gate for an actor of `role`; otherwise what was wrong. `owner` is the
/// pseudonym an ownership refusal must name.
fn judge(role: Role, gate: Gate, got: &Result<(), StoreError>, owner: &str) -> Option<String> {
    let matches = match gate {
        Gate::Allowed => got.is_ok(),
        Gate::Forbidden => matches!(
            got,
            Err(StoreError::Transition(TransitionError::Forbidden(r))) if *r == role
        ),
        Gate::Owned => matches!(got, Err(StoreError::Refused(m)) if m.contains(owner)),
    };
    (!matches).then(|| format!("expected {gate:?} ({role:?}), got {got:?}"))
}

// ---- setup helpers, each acting as the real actor that would make the state ----

/// The D5 digest of the command every setup task carries: `["true"]` in `/`, with no files.
fn digest() -> String {
    claims::digest(&["true".to_string()], "/", &[])
}

fn add_task(s: &mut Store, by: &Identity, arg: serde_json::Value) {
    s.work_add(NOW, &op(by, "work.add", "", &arg.to_string()))
        .expect("setup: work.add");
}

fn claim_task(s: &mut Store, by: &Identity, id: &str) {
    s.work_claim(NOW, &op(by, "work.claim", id, ""))
        .expect("setup: work.claim");
}

/// A queued task with a command in `/`, authored by the supervisor and claimed by alice, not yet accepted.
fn claimed_command(s: &mut Store, c: &Cast, id: &str) {
    add_task(
        s,
        &c.sup,
        json!({"id": id, "title": id, "command": ["true"], "cwd": "/"}),
    );
    claim_task(s, &c.alice, id);
}

/// `claimed_command`, accepted by alice, who is not its author: the state `work.run` needs.
fn accepted_task(s: &mut Store, c: &Cast, id: &str) {
    claimed_command(s, c, id);
    s.work_accept(
        NOW,
        &op(
            &c.alice,
            "work.accept",
            id,
            &json!({"digest": digest()}).to_string(),
        ),
    )
    .expect("setup: work.accept");
}

fn pool(s: &mut Store, c: &Cast) {
    s.pool_define(
        NOW,
        &op(
            &c.owner,
            "pool.define",
            "gpu",
            r#"{"name":"gpu","capacity":4}"#,
        ),
    )
    .expect("setup: pool.define");
}

fn alice_lease(s: &mut Store, c: &Cast) -> String {
    pool(s, c);
    s.lease_acquire(
        NOW,
        &op(
            &c.alice,
            "lease.acquire",
            "",
            r#"{"kind":"pool","resource":"gpu","amount":1}"#,
        ),
    )
    .expect("setup: lease.acquire")
    .id
}

fn timer_arg(id: &str) -> String {
    json!({"id": id, "spec": {"kind": "every", "every_s": 60}, "template": {"title": "tick"}})
        .to_string()
}

fn create_card(s: &mut Store, c: &Cast, id: &str) {
    s.create_card(
        NOW,
        &op(&c.sup, "card.create", id, r#"{"title":"Port scheduler"}"#),
    )
    .expect("setup: card.create");
}

/// Card c1 walked from backlog to `state` along legal transitions. Claimed, doing, review and done are
/// alice's, so alice owns the card from claimed onwards.
fn reach(s: &mut Store, c: &Cast, state: CardState) {
    create_card(s, c, "c1");
    if state == CardState::Backlog {
        return;
    }
    let mv = |s: &mut Store, who: &Identity, to: &str| {
        s.transition(NOW, &op(who, "card.move", "c1", to), false)
            .expect("setup: card.move");
    };
    match state {
        CardState::Backlog => {}
        CardState::Cancelled => mv(s, &c.sup, "cancelled"),
        CardState::Blocked => mv(s, &c.sup, "blocked"),
        CardState::Ready => mv(s, &c.sup, "ready"),
        CardState::Claimed | CardState::Doing | CardState::Review | CardState::Done => {
            mv(s, &c.sup, "ready");
            mv(s, &c.alice, "claimed");
            if state != CardState::Claimed {
                mv(s, &c.alice, "doing");
            }
            if state == CardState::Review {
                mv(s, &c.alice, "review");
            }
            if state == CardState::Done {
                s.add_evidence(NOW, &op(&c.alice, "card.evidence", "c1", "run.log"))
                    .expect("setup: card.evidence");
                mv(s, &c.alice, "done");
            }
        }
    }
}

fn card_ext_of(s: &Store, card: &str) -> agora_sched::model::CardExt {
    s.card_ext(&[card.to_string()]).unwrap().remove(0)
}

fn start_arg(job: &str) -> String {
    let arg = JobStartArgs {
        pid: 4242,
        supervisor_pid: 4241,
        supervisor_start_s: Some(1_700_000_000),
        qos: String::new(),
        exclusive: false,
        quiet_until_ms: None,
        log: format!("{LOG_DIR}/{job}.log"),
    };
    serde_json::to_string(&arg).unwrap()
}

fn finish_arg(status: Option<i32>, note: &str) -> String {
    let arg = JobFinishArgs {
        status,
        note: note.to_string(),
        cpu_seconds: None,
        rss_peak_kib: None,
        ended_ms: NOW + 5_000,
        before_start: false,
    };
    serde_json::to_string(&arg).unwrap()
}

fn sync_arg(job: &str, outcome: JobOutcome, status: Option<i32>) -> String {
    let arg = CardSyncArgs {
        task: job.to_string(),
        outcome,
        status,
        log: Some(format!("{LOG_DIR}/{job}.log")),
        note: String::new(),
    };
    serde_json::to_string(&arg).unwrap()
}

/// Queues an inline job as the supervisor (optionally linked to `card`) and starts it as the system.
fn start_job(s: &mut Store, c: &Cast, job: &str, card: Option<&str>) {
    let add =
        json!({"add": {"id": job, "title": job, "command": ["true"], "cwd": "/", "card": card}});
    s.work_request(
        NOW,
        &op(&c.sup, "work.run", "", &add.to_string()),
        1.0,
        LOG_DIR,
    )
    .expect("setup: inline work.run");
    s.job_start(NOW, &op(&c.sys, "job.start", job, &start_arg(job)))
        .expect("setup: job.start");
}

// ---- role matrix: one test per operation kind ----
