//! Authorization for the M5 work-queue, timer, pool, lease, quiet, policy and card-extension operations, plus the
//! system identity (SPEC §8 work_authz.rs; D4 roles, D6 system identity, D7 card sync).
//!
//! Each role-matrix test attempts one operation as six kinds of actor on a minimal fixture, and as the system
//! identity. An allowed actor must succeed; a denied one must fail with exactly `Forbidden(its role)`.

mod common;
use common::*;

use agora_core::{CardState, Role, TransitionError};
use agora_crypto::Identity;
use agora_sched::claims;
use agora_sched::model::TaskState;
use agora_sched::model_ops::{CardSyncArgs, JobFinishArgs, JobOutcome, JobStartArgs};
use agora_store::*;
use serde_json::json;

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

#[test]
fn work_scope_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_scope_set(NOW, &op(a, "work.scope", "", r#"{"id":"proj"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_add_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_add(NOW, &op(a, "work.add", "", r#"{"id":"m1","title":"mine"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_next_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_next(NOW, &op(a, "work.next", "", "{}")).map(|_| ())
    });
}

#[test]
fn work_claim_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "k1", "title": "k"}));
        s.work_claim(NOW, &op(a, "work.claim", "k1", ""))
            .map(|_| ())
    });
}

#[test]
fn work_accept_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        claimed_command(s, c, "a1");
        s.work_accept(
            NOW,
            &op(
                a,
                "work.accept",
                "a1",
                &json!({"digest": digest()}).to_string(),
            ),
        )
        .map(|_| ())
    });
}

#[test]
fn work_run_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        accepted_task(s, c, "a1");
        s.work_request(
            NOW,
            &op(a, "work.run", "a1", &json!({"task": "a1"}).to_string()),
            1.0,
            LOG_DIR,
        )
        .map(|_| ())
    });
}

#[test]
fn work_finish_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "f1", "title": "f1"}));
        s.work_finish(NOW, &op(a, "work.finish", "f1", r#"{"state":"done"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_cancel_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "x1", "title": "x1"}));
        s.work_cancel(NOW, &op(a, "work.cancel", "x1", r#"{"note":"stop"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_update_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "u1", "title": "u1"}));
        s.work_update(NOW, &op(a, "work.update", "u1", r#"{"title":"renamed"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_reclaim_follows_the_role_matrix() {
    assert_matrix([No, No, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "t1", "title": "t1"}));
        claim_task(s, &c.alice, "t1");
        s.work_reclaim(
            NOW,
            &op(a, "work.reclaim", "task:t1", r#"{"note":"stale"}"#),
        )
    });
}

#[test]
fn work_settings_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        s.work_settings_set(
            NOW,
            &op(a, "work.settings", "", r#"{"patch":{"keep_days":"30"}}"#),
        )
        .map(|_| ())
    });
}

#[test]
fn timer_create_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.timer_create(NOW, &op(a, "timer.create", "", &timer_arg("tm1")))
            .map(|_| ())
    });
}

#[test]
fn timer_delete_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        s.timer_create(NOW, &op(&c.alice, "timer.create", "", &timer_arg("tm1")))
            .expect("setup: timer.create");
        s.timer_delete(NOW, &op(a, "timer.delete", "tm1", ""))
    });
}

#[test]
fn pool_define_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        s.pool_define(
            NOW,
            &op(a, "pool.define", "gpu", r#"{"name":"gpu","capacity":2}"#),
        )
        .map(|_| ())
    });
}

#[test]
fn lease_acquire_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, c, a| {
        pool(s, c);
        s.lease_acquire(
            NOW,
            &op(
                a,
                "lease.acquire",
                "",
                r#"{"kind":"pool","resource":"gpu","amount":1}"#,
            ),
        )
        .map(|_| ())
    });
}

#[test]
fn lease_renew_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, No, No, No], |s, c, a| {
        let lease = alice_lease(s, c);
        s.lease_renew(NOW, &op(a, "lease.renew", &lease, "{}"))
            .map(|_| ())
    });
}

#[test]
fn lease_release_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        let lease = alice_lease(s, c);
        s.lease_release(NOW, &op(a, "lease.release", &lease, ""))
    });
}

#[test]
fn quiet_open_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        let arg = json!({"expected_end_ms": NOW + 3_600_000}).to_string();
        s.quiet_open(NOW, &op(a, "quiet.open", "", &arg))
            .map(|_| ())
    });
}

#[test]
fn quiet_close_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, c, a| {
        let arg = json!({"expected_end_ms": NOW + 3_600_000}).to_string();
        let window = s
            .quiet_open(NOW, &op(&c.ctl, "quiet.open", "", &arg))
            .expect("setup: quiet.open");
        s.quiet_close(NOW, &op(a, "quiet.close", &window.id, ""))
    });
}

#[test]
fn policy_set_follows_the_role_matrix() {
    assert_matrix([No, No, No, Yes, Yes, Yes], |s, _c, a| {
        let arg = r#"{"team":"core","lane":"backend","policy":{"wip_limit":3}}"#;
        s.policy_set(NOW, &op(a, "policy.set", "core/backend", arg))
            .map(|_| ())
    });
}

#[test]
fn card_update_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        s.card_update(NOW, &op(a, "card.update", "c1", r#"{"value":2}"#))
            .map(|_| ())
    });
}

#[test]
fn card_dep_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        create_card(s, c, "c2");
        s.card_dep(NOW, &op(a, "card.dep", "c1", r#"{"after":"c2"}"#))
    });
}

#[test]
fn card_link_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        create_card(s, c, "c2");
        s.card_link(
            NOW,
            &op(a, "card.link", "c1", r#"{"rel":"relates","target":"c2"}"#),
        )
    });
}

#[test]
fn card_escalate_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        s.card_escalate(
            NOW,
            &op(
                a,
                "card.escalate",
                "c1",
                r#"{"owner_action":"needs a decision"}"#,
            ),
            false,
        )
        .map(|_| ())
    });
}

// ---- replay and envelope ----

#[test]
fn replaying_a_signed_operation_is_refused() {
    let (mut s, c) = fixture();
    let signed = op(&c.alice, "work.add", "", r#"{"id":"r1","title":"once"}"#);
    s.work_add(NOW, &signed).unwrap();
    assert!(matches!(
        s.work_add(NOW, &signed),
        Err(StoreError::Invalid("replayed operation"))
    ));
}

#[test]
fn an_operation_stamped_over_five_minutes_from_the_clock_is_refused() {
    let (mut s, c) = fixture();
    let arg = r#"{"id":"s1","title":"skew"}"#;
    let late = op_at(&c.alice, "work.add", "", arg, NOW + FIVE_MINUTES_MS + 1);
    let early = op_at(&c.alice, "work.add", "", arg, NOW - FIVE_MINUTES_MS - 1);
    assert!(matches!(
        s.work_add(NOW, &late),
        Err(StoreError::Invalid("timestamp outside allowed clock skew"))
    ));
    assert!(matches!(
        s.work_add(NOW, &early),
        Err(StoreError::Invalid("timestamp outside allowed clock skew"))
    ));
}

#[test]
fn an_operation_stamped_exactly_five_minutes_away_is_accepted() {
    let (mut s, c) = fixture();
    let late = op_at(
        &c.alice,
        "work.add",
        "",
        r#"{"id":"s2","title":"late"}"#,
        NOW + FIVE_MINUTES_MS,
    );
    let early = op_at(
        &c.alice,
        "work.add",
        "",
        r#"{"id":"s3","title":"early"}"#,
        NOW - FIVE_MINUTES_MS,
    );
    s.work_add(NOW, &late).unwrap();
    s.work_add(NOW, &early).unwrap();
}

#[test]
fn an_operation_signed_for_another_kind_is_refused_by_the_method() {
    let (mut s, c) = fixture();
    let signed = op(&c.alice, "work.add", "", r#"{"id":"k2","title":"kind"}"#);
    assert!(matches!(
        s.work_claim(NOW, &signed),
        Err(StoreError::Invalid("wrong operation kind"))
    ));
}

#[test]
fn a_revoked_identity_cannot_sign_work_operations() {
    let (mut s, c) = fixture();
    s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.alice), ""))
        .unwrap();
    assert!(matches!(
        s.work_add(
            NOW,
            &op(&c.alice, "work.add", "", r#"{"id":"v1","title":"v"}"#)
        ),
        Err(StoreError::Invalid("identity revoked"))
    ));
}

#[test]
fn an_unregistered_identity_cannot_sign_work_operations() {
    let (mut s, c) = fixture();
    assert!(matches!(
        s.work_add(
            NOW,
            &op(&c.eve, "work.add", "", r#"{"id":"e1","title":"e"}"#)
        ),
        Err(StoreError::Invalid("unknown identity"))
    ));
}

// ---- system identity ----

#[test]
fn system_identity_starts_notes_and_finishes_a_job() {
    let (mut s, c) = fixture();
    start_job(&mut s, &c, "j1", None);
    s.job_note(NOW, &op(&c.sys, "job.note", "j1", r#"{"note":"halfway"}"#))
        .unwrap();
    let done = s
        .job_finish(
            NOW,
            &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        )
        .unwrap();
    assert_eq!(done.state, TaskState::Done);
}

#[test]
fn system_identity_fires_a_timer_into_a_task_its_owner_authored() {
    let (mut s, c) = fixture();
    let created = s
        .timer_create(NOW, &op(&c.alice, "timer.create", "", &timer_arg("tm1")))
        .unwrap();
    let due = created.next_fire_ms.expect("the timer has a next fire");
    let fire = json!({"fire_ms": due}).to_string();
    let task = s
        .timer_fire(due, &op(&c.sys, "timer.fire", "tm1", &fire))
        .unwrap()
        .expect("the timer fires a task");
    assert_eq!(task.id, "tm1-1");
    assert_eq!(task.author, pk_hex(&c.alice));
}

#[test]
fn system_identity_applies_each_reconcile_action_kind() {
    let (mut s, c) = fixture();
    reach(&mut s, &c, CardState::Claimed);
    let alice = pk_hex(&c.alice);
    let actions = [
        json!({"action": "expire_lease", "key": "expire:l1", "lease": "l1"}),
        json!({"action": "close_quiet", "key": "quiet:q1", "lease": "q1"}),
        json!({"action": "notice", "key": "notice:1", "target": alice, "kind": "stale", "subject": "c1", "text": "c1 is stale"}),
        json!({"action": "reclaim_card", "key": "reclaim:c1", "card": "c1", "owner": alice}),
        json!({"action": "blocker", "key": "blocker:disk", "open": true, "title": "disk low", "owner_action": "free disk"}),
        json!({"action": "admission", "key": "admission:pause", "paused": "disk low"}),
    ];
    for action in actions {
        let kind = match action["action"].as_str().unwrap() {
            "expire_lease" => "reconcile.expire",
            "close_quiet" => "reconcile.quiet_close",
            "notice" => "reconcile.notice",
            "reclaim_card" => "reconcile.reclaim",
            "blocker" => "reconcile.blocker",
            _ => "reconcile.admission",
        };
        let got = s.apply_reconcile(NOW, &op(&c.sys, kind, "", &action.to_string()));
        assert!(matches!(got, Ok(true)), "{kind}: {got:?}");
    }
    assert_eq!(s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn system_identity_cannot_move_a_card_through_a_transition() {
    let (mut s, c) = fixture();
    create_card(&mut s, &c, "c1");
    assert!(matches!(
        s.transition(NOW, &op(&c.sys, "card.move", "c1", "ready"), false),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer
        )))
    ));
    assert_eq!(s.card_state("c1").unwrap(), CardState::Backlog);
}

/// Every system kind except card.job_sync (which only travels inside a job finish), each with the store method
/// that applies it. The arguments are inert: an agent must be refused before any of them is read.
fn system_attempts() -> [(&'static str, Attempt); 10] {
    [
        ("job.start", |s, _c, a| {
            s.job_start(NOW, &op(a, "job.start", "j1", &start_arg("j1")))
                .map(|_| ())
        }),
        ("job.note", |s, _c, a| {
            s.job_note(NOW, &op(a, "job.note", "j1", r#"{"note":"n"}"#))
        }),
        ("job.finish", |s, _c, a| {
            s.job_finish(
                NOW,
                &op(a, "job.finish", "j1", &finish_arg(Some(0), "")),
                None,
            )
            .map(|_| ())
        }),
        ("timer.fire", |s, _c, a| {
            s.timer_fire(NOW, &op(a, "timer.fire", "tm1", r#"{"fire_ms":1}"#))
                .map(|_| ())
        }),
        ("reconcile.expire", |s, _c, a| {
            let arg = r#"{"action":"expire_lease","key":"k","lease":"l1"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.expire", "", arg))
                .map(|_| ())
        }),
        ("reconcile.quiet_close", |s, _c, a| {
            let arg = r#"{"action":"close_quiet","key":"k","lease":"q1"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.quiet_close", "", arg))
                .map(|_| ())
        }),
        ("reconcile.notice", |s, _c, a| {
            let arg =
                r#"{"action":"notice","key":"k","target":"t","kind":"k","subject":"s","text":"x"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.notice", "", arg))
                .map(|_| ())
        }),
        ("reconcile.reclaim", |s, _c, a| {
            let arg = r#"{"action":"reclaim_card","key":"k","card":"c1","owner":"o"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.reclaim", "", arg))
                .map(|_| ())
        }),
        ("reconcile.blocker", |s, _c, a| {
            let arg =
                r#"{"action":"blocker","key":"k","open":true,"title":"t","owner_action":"o"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.blocker", "", arg))
                .map(|_| ())
        }),
        ("reconcile.admission", |s, _c, a| {
            let arg = r#"{"action":"admission","key":"k","paused":null}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.admission", "", arg))
                .map(|_| ())
        }),
    ]
}

#[test]
fn agents_cannot_sign_any_system_kind() {
    let mut failures = Vec::new();
    for (kind, attempt) in system_attempts() {
        for actor in [Actor::WorkerOwn, Actor::Owner, Actor::Observer] {
            let (mut s, c) = fixture();
            let got = attempt(&mut s, &c, actor.of(&c));
            if let Some(problem) = judge(actor.role(), Gate::Forbidden, &got, &c.alice.pseudonym())
            {
                failures.push(format!("{kind} signed by {actor:?}: {problem}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn an_agent_signed_card_sync_inside_a_system_job_finish_is_refused_and_the_job_keeps_running() {
    let (mut s, c) = fixture();
    reach(&mut s, &c, CardState::Claimed);
    start_job(&mut s, &c, "j1", Some("c1"));
    let forged = op(
        &c.alice,
        "card.job_sync",
        "j1",
        &sync_arg("j1", JobOutcome::Done, Some(0)),
    );
    let got = s.job_finish(
        NOW,
        &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
        Some(&forged),
    );
    assert!(matches!(
        got,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    assert_eq!(s.work_task("j1").unwrap().state, TaskState::Running);
    assert_eq!(s.card_state("c1").unwrap(), CardState::Claimed);
}

#[test]
fn a_replaced_system_key_is_refused_after_re_registration() {
    let (mut s, c) = fixture();
    start_job(&mut s, &c, "j1", None);
    let new = Identity::from_seed([10; 32]);
    s.register_system_identity(NOW, &new.public()).unwrap();
    assert!(matches!(
        s.job_finish(
            NOW,
            &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        ),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer
        )))
    ));
    let done = s
        .job_finish(
            NOW,
            &op(&new, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        )
        .unwrap();
    assert_eq!(done.state, TaskState::Done);
}

#[test]
fn an_owner_cannot_grant_a_role_to_the_system_identity() {
    let (mut s, c) = fixture();
    let got = s.grant_role(
        NOW,
        &op(&c.owner, "role.grant", &pk_hex(&c.sys), "controller"),
    );
    assert!(matches!(
        got,
        Err(StoreError::Refused(m)) if m == "the system identity cannot be granted roles or revoked by an agent"
    ));
    assert_eq!(
        s.identity_status(&c.sys.public()).unwrap(),
        IdentityStatus::Active(Role::Observer)
    );
}

#[test]
fn an_owner_cannot_revoke_the_system_identity() {
    let (mut s, c) = fixture();
    let got = s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.sys), ""));
    assert!(matches!(
        got,
        Err(StoreError::Refused(m)) if m == "the system identity cannot be granted roles or revoked by an agent"
    ));
    assert_eq!(s.system_identity().unwrap(), Some(c.sys.public()));
}

// ---- card.job_sync: only the D7 edges ----

/// D7: the state a job outcome moves a card to from `from`, or None when the edge is not a sync edge.
fn d7_target(outcome: JobOutcome, from: CardState) -> Option<CardState> {
    match (outcome, from) {
        (JobOutcome::Done, CardState::Claimed | CardState::Doing) => Some(CardState::Review),
        (JobOutcome::Failed, CardState::Claimed | CardState::Doing | CardState::Review) => {
            Some(CardState::Blocked)
        }
        (JobOutcome::Cancelled, CardState::Claimed | CardState::Doing | CardState::Review) => {
            Some(CardState::Ready)
        }
        _ => None,
    }
}

const ALL_STATES: [CardState; 8] = [
    CardState::Backlog,
    CardState::Ready,
    CardState::Claimed,
    CardState::Doing,
    CardState::Review,
    CardState::Blocked,
    CardState::Done,
    CardState::Cancelled,
];

/// Ends job `job` with `outcome` as the daemon would: a cancelled job is flagged first, and the finish carries
/// the card sync signed by the system in the same call.
fn finish_job(s: &mut Store, c: &Cast, job: &str, outcome: JobOutcome) -> Result<(), StoreError> {
    let status = match outcome {
        JobOutcome::Done => Some(0),
        JobOutcome::Failed => Some(1),
        JobOutcome::Cancelled => Some(143),
    };
    if outcome == JobOutcome::Cancelled {
        s.work_cancel(NOW, &op(&c.sup, "work.cancel", job, r#"{"note":"stop"}"#))
            .expect("setup: cancel a running job");
    }
    let sync = op(
        &c.sys,
        "card.job_sync",
        job,
        &sync_arg(job, outcome, status),
    );
    s.job_finish(
        NOW,
        &op(&c.sys, "job.finish", job, &finish_arg(status, "boom")),
        Some(&sync),
    )
    .map(|_| ())
}

/// For every card state, a job that ends with `outcome` must leave the card where D7 says. An illegal edge
/// leaves the card unchanged and tells the owner (or the supervisors, when unowned) why.
fn assert_job_sync(outcome: JobOutcome) {
    let mut failures = Vec::new();
    for from in ALL_STATES {
        let (mut s, c) = fixture();
        reach(&mut s, &c, from);
        let before = s.list_cards(None, None, 100).unwrap();
        let owner_before = before
            .iter()
            .find(|r| r.id == "c1")
            .and_then(|r| r.owner.clone());
        start_job(&mut s, &c, "j1", Some("c1"));
        finish_job(&mut s, &c, "j1", outcome).expect("the job finish with its sync");

        let target = d7_target(outcome, from);
        let expected = target.unwrap_or(from);
        let landed = s.card_state("c1").unwrap();
        let mut problems = Vec::new();
        if landed != expected {
            problems.push(format!("card is {landed:?}, D7 says {expected:?}"));
        }
        let notify = owner_before.unwrap_or_else(|| pk_hex(&c.sup));
        let heard = s
            .notices(&notify, 0, 100)
            .unwrap()
            .iter()
            .any(|n| n.subject == "c1");
        if target.is_none() && !heard {
            problems.push(format!("no notice to {notify} for the skipped sync"));
        }
        if target.is_some() && heard {
            problems.push("a skip notice was sent for a legal sync".to_string());
        }
        let needs_owner = card_ext_of(&s, "c1").needs_owner;
        let flagged = outcome == JobOutcome::Failed && target.is_some();
        if needs_owner != flagged {
            problems.push(format!("needs_owner is {needs_owner}, expected {flagged}"));
        }
        if outcome == JobOutcome::Cancelled && target.is_some() {
            let owner_now = s
                .list_cards(None, None, 100)
                .unwrap()
                .into_iter()
                .find(|r| r.id == "c1")
                .and_then(|r| r.owner);
            if owner_now.is_some() {
                problems.push("a cancelled sync did not clear the owner".to_string());
            }
        }
        if !problems.is_empty() {
            failures.push(format!("from {from:?}: {}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn job_sync_done_moves_only_claimed_or_doing_cards_to_review() {
    assert_job_sync(JobOutcome::Done);
}

#[test]
fn job_sync_failed_moves_only_claimed_doing_or_review_cards_to_blocked() {
    assert_job_sync(JobOutcome::Failed);
}

#[test]
fn job_sync_cancelled_moves_only_claimed_doing_or_review_cards_to_ready() {
    assert_job_sync(JobOutcome::Cancelled);
}
