//! Timer creation, deletion, quota and queries. The `timer.fire` tests are in `fire`.

mod fire;
mod template;

use super::*;
use crate::identity::hex;
use agora_core::TransitionError;
use agora_crypto::Identity;
use std::sync::atomic::{AtomicU64, Ordering};

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

static NONCES: AtomicU64 = AtomicU64::new(0);

fn op(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
    let nonce = format!("n{}", NONCES.fetch_add(1, Ordering::Relaxed));
    SignedOp::new(kind, subject, arg, now, &nonce).sign(id)
}

fn hex_of(id: &Identity) -> String {
    hex(&id.public())
}

struct Fixture {
    s: Store,
    owner: Identity,
    sys: Identity,
}

/// Owner, a supervisor, two workers (alice, bob), an observer (olive) and the system identity.
fn fixture() -> Fixture {
    let mut s = Store::open_in_memory().unwrap();
    let owner = Identity::from_seed([1; 32]);
    let sup = Identity::from_seed([2; 32]);
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    let olive = Identity::from_seed([8; 32]);
    let sys = Identity::from_seed([9; 32]);
    s.bootstrap_owner(NOW, &owner.public()).unwrap();
    for (id, label) in [
        (&sup, "sup"),
        (&alice, "alice"),
        (&bob, "bob"),
        (&olive, "olive"),
    ] {
        s.register_identity(NOW, &id.public(), label).unwrap();
    }
    for (who, role) in [(&sup, "supervisor"), (&olive, "observer")] {
        let grant = op(&owner, "role.grant", &hex_of(who), role, NOW);
        s.grant_role(NOW, &grant).unwrap();
    }
    s.register_system_identity(NOW, &sys.public()).unwrap();
    Fixture { s, owner, sys }
}

fn template(title: &str) -> AddArgs {
    AddArgs {
        title: title.to_string(),
        ..AddArgs::default()
    }
}

fn create(
    f: &mut Fixture,
    who: &Identity,
    id: Option<&str>,
    spec: TimerSpec,
    template: AddArgs,
) -> Result<TimerRecord, StoreError> {
    let args = TimerArgs {
        id: id.map(str::to_string),
        spec,
        template,
    };
    let arg = serde_json::to_string(&args).unwrap();
    f.s.timer_create(NOW, &op(who, "timer.create", "", &arg, NOW))
}

fn at(at_ms: i64) -> TimerSpec {
    TimerSpec::At { at_ms }
}

fn fire(
    f: &mut Fixture,
    id: &str,
    fire_ms: i64,
    now: i64,
) -> Result<Option<TaskRecord>, StoreError> {
    let arg = serde_json::to_string(&TimerFireArgs { fire_ms }).unwrap();
    f.s.timer_fire(now, &op(&f.sys, "timer.fire", id, &arg, now))
}

fn count(f: &Fixture, sql: &str) -> i64 {
    f.s.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn set_quota(f: &Fixture, quota: u32) {
    let settings = WorkSettings {
        timer_quota: quota,
        ..WorkSettings::default()
    };
    f.s.conn
        .execute(
            "INSERT INTO work_settings(id,json) VALUES (1,?)",
            [serde_json::to_string(&settings).unwrap()],
        )
        .unwrap();
}

#[test]
fn create_stores_next_fire_owner_and_event() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let rec = create(
        &mut f,
        &alice,
        Some("nightly"),
        at(NOW + HOUR),
        template("build"),
    )
    .unwrap();
    assert_eq!(rec.id, "nightly");
    assert_eq!(rec.owner, hex_of(&alice));
    assert_eq!(rec.owner_name, alice.pseudonym());
    assert!(rec.enabled);
    assert_eq!(rec.next_fire_ms, Some(NOW + HOUR));
    assert_eq!(rec.last_fire_ms, None);
    assert_eq!(rec.fired, 0);
    assert_eq!(rec.template.title, "build");
    let events = count(
        &f,
        "SELECT COUNT(*) FROM events WHERE kind='timer.created' AND subject='nightly'",
    );
    assert_eq!(events, 1);
}

#[test]
fn create_without_id_names_timers_in_sequence() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let first = create(&mut f, &alice, None, at(NOW + HOUR), template("a")).unwrap();
    let second = create(&mut f, &alice, None, at(NOW + HOUR), template("b")).unwrap();
    assert_eq!(first.id, "timer-1");
    assert_eq!(second.id, "timer-2");
}

#[test]
fn observer_cannot_create_a_timer() {
    let mut f = fixture();
    let olive = Identity::from_seed([8; 32]);
    let err = create(&mut f, &olive, Some("x"), at(NOW + HOUR), template("a")).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
}

#[test]
fn create_refuses_an_invalid_schedule_with_its_reason() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let short = TimerSpec::Every {
        every_s: 30,
        start_ms: None,
    };
    let err = create(&mut f, &alice, Some("fast"), short, template("a")).unwrap_err();
    assert!(matches!(err, StoreError::Refused(ref m) if m == "every must be at least 60 seconds"));
    let bad_cron = TimerSpec::Cron {
        expr: "61 * * * *".to_string(),
    };
    let err = create(&mut f, &alice, Some("cron"), bad_cron, template("a")).unwrap_err();
    assert!(matches!(err, StoreError::Refused(_)));
    assert!(f.s.timers(None).unwrap().is_empty());
}

#[test]
fn create_refuses_a_bad_template() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let named = AddArgs {
        id: Some("task-x".to_string()),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("t"), at(NOW + HOUR), named).unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
    let bad_kind = AddArgs {
        kind: "not a kind!".to_string(),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("t"), at(NOW + HOUR), bad_kind).unwrap_err();
    assert!(matches!(err, StoreError::Refused(_)));
    let empty_command = AddArgs {
        command: Some(vec![]),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("t"), at(NOW + HOUR), empty_command).unwrap_err();
    assert!(matches!(err, StoreError::Refused(ref m) if m == "no command after --"));
    let zero_size = AddArgs {
        size: 0.0,
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("t"), at(NOW + HOUR), zero_size).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("size must be a positive number")
    ));
    let no_scope = AddArgs {
        scope: Some("nowhere".to_string()),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("t"), at(NOW + HOUR), no_scope).unwrap_err();
    assert!(matches!(err, StoreError::Refused(ref m) if m == "unknown scope nowhere"));
    assert!(f.s.timers(None).unwrap().is_empty());
}

#[test]
fn create_checks_template_files_and_canonicalises_them() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let relative = AddArgs {
        files: vec!["src/lib.rs".to_string()],
        ..template("a")
    };
    assert!(create(&mut f, &alice, Some("r"), at(NOW + HOUR), relative).is_err());
    let absolute = AddArgs {
        files: vec!["/work/./a//b.rs".to_string()],
        ..template("a")
    };
    let rec = create(&mut f, &alice, Some("abs"), at(NOW + HOUR), absolute).unwrap();
    assert_eq!(rec.template.files, vec!["/work/a/b.rs".to_string()]);
}

#[test]
fn template_command_needs_an_absolute_working_directory() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let argv = vec!["make".to_string()];
    let no_cwd = AddArgs {
        command: Some(argv.clone()),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("c"), at(NOW + HOUR), no_cwd).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("a command needs a working directory")
    ));
    let relative = AddArgs {
        command: Some(argv.clone()),
        cwd: Some("build/out".to_string()),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("c"), at(NOW + HOUR), relative).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("the working directory must be an absolute path")
    ));
    let absolute = AddArgs {
        command: Some(argv),
        cwd: Some("/work".to_string()),
        ..template("a")
    };
    let rec = create(&mut f, &alice, Some("c"), at(NOW + HOUR), absolute).unwrap();
    assert_eq!(rec.template.cwd.as_deref(), Some("/work"));
}

#[test]
fn template_caps_files_and_prerequisites_at_1000() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let files = |n: usize| -> Vec<String> { (0..n).map(|i| format!("/w/f{i}.rs")).collect() };
    let too_many_files = AddArgs {
        files: files(1001),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("f"), at(NOW + HOUR), too_many_files).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("too many files or prerequisites")
    ));
    let too_many_after = AddArgs {
        after: (0..1001).map(|i| format!("p{i}")).collect(),
        ..template("a")
    };
    let err = create(&mut f, &alice, Some("f"), at(NOW + HOUR), too_many_after).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("too many files or prerequisites")
    ));
    let at_limit = AddArgs {
        files: files(1000),
        ..template("a")
    };
    let rec = create(&mut f, &alice, Some("f"), at(NOW + HOUR), at_limit).unwrap();
    assert_eq!(rec.template.files.len(), 1000);
    assert_eq!(f.s.timers(None).unwrap().len(), 1);
}

#[test]
fn timer_id_is_capped_at_100_characters() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let long = "a".repeat(101);
    let err = create(
        &mut f,
        &alice,
        Some(long.as_str()),
        at(NOW + HOUR),
        template("a"),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("timer id is longer than 100 characters")
    ));
    let limit = "a".repeat(100);
    let rec = create(
        &mut f,
        &alice,
        Some(limit.as_str()),
        at(NOW + HOUR),
        template("a"),
    )
    .unwrap();
    assert_eq!(rec.id, limit);
    assert_eq!(f.s.timers(None).unwrap().len(), 1);
}

#[test]
fn duplicate_timer_id_is_refused() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("dup"), at(NOW + HOUR), template("a")).unwrap();
    let err = create(&mut f, &alice, Some("dup"), at(NOW + HOUR), template("a")).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("timer already exists")));
}

#[test]
fn quota_is_per_identity_and_counts_only_enabled_timers() {
    let mut f = fixture();
    set_quota(&f, 2);
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    create(&mut f, &alice, Some("a1"), at(NOW + HOUR), template("a")).unwrap();
    create(&mut f, &alice, Some("a2"), at(NOW + HOUR), template("a")).unwrap();
    let err = create(&mut f, &alice, Some("a3"), at(NOW + HOUR), template("a")).unwrap_err();
    assert!(
        matches!(err, StoreError::Refused(ref m) if m == "timer quota reached: 2 of 2 enabled timers")
    );
    create(&mut f, &bob, Some("b1"), at(NOW + HOUR), template("b")).unwrap();
    // Firing the at-timers disables them, which frees their quota slots.
    fire(&mut f, "a1", NOW + HOUR, NOW + HOUR).unwrap();
    fire(&mut f, "a2", NOW + HOUR, NOW + HOUR).unwrap();
    create(
        &mut f,
        &alice,
        Some("a3"),
        at(NOW + 2 * HOUR),
        template("a"),
    )
    .unwrap();
}

#[test]
fn delete_follows_owner_or_supervisor_rules() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    create(&mut f, &alice, Some("mine"), at(NOW + HOUR), template("a")).unwrap();
    create(
        &mut f,
        &alice,
        Some("theirs"),
        at(NOW + HOUR),
        template("a"),
    )
    .unwrap();
    let bob = Identity::from_seed([4; 32]);
    let err =
        f.s.timer_delete(NOW, &op(&bob, "timer.delete", "mine", "", NOW))
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    f.s.timer_delete(NOW, &op(&alice, "timer.delete", "mine", "", NOW))
        .unwrap();
    let sup = Identity::from_seed([2; 32]);
    f.s.timer_delete(NOW, &op(&sup, "timer.delete", "theirs", "", NOW))
        .unwrap();
    assert!(f.s.timers(None).unwrap().is_empty());
    let err =
        f.s.timer_delete(NOW, &op(&alice, "timer.delete", "mine", "", NOW))
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref m) if m == "timer mine"));
    assert_eq!(
        count(&f, "SELECT COUNT(*) FROM events WHERE kind='timer.deleted'"),
        2
    );
}

#[test]
fn timers_lists_all_or_one_owner() {
    let mut f = fixture();
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    create(&mut f, &alice, Some("a1"), at(NOW + HOUR), template("a")).unwrap();
    create(&mut f, &alice, Some("a2"), at(NOW + HOUR), template("a")).unwrap();
    create(&mut f, &bob, Some("b1"), at(NOW + HOUR), template("b")).unwrap();
    assert_eq!(f.s.timers(None).unwrap().len(), 3);
    let mine = f.s.timers(Some(&hex_of(&alice))).unwrap();
    let ids: Vec<&str> = mine.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["a1", "a2"]);
    assert!(
        f.s.timers(Some(&hex_of(&Identity::from_seed([2; 32]))))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn next_and_due_queries_follow_enabled_fire_times() {
    let mut f = fixture();
    assert_eq!(f.s.next_timer_due().unwrap(), None);
    assert!(f.s.due_timers(NOW + HOUR).unwrap().is_empty());
    let alice = Identity::from_seed([3; 32]);
    create(
        &mut f,
        &alice,
        Some("late"),
        at(NOW + 10 * MIN),
        template("a"),
    )
    .unwrap();
    create(
        &mut f,
        &alice,
        Some("early"),
        at(NOW + 5 * MIN),
        template("a"),
    )
    .unwrap();
    assert_eq!(f.s.next_timer_due().unwrap(), Some(NOW + 5 * MIN));
    let due: Vec<String> =
        f.s.due_timers(NOW + 5 * MIN)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
    assert_eq!(due, ["early"]);
    let due: Vec<String> =
        f.s.due_timers(NOW + 20 * MIN)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
    assert_eq!(due, ["early", "late"]);
    assert!(f.s.due_timers(NOW).unwrap().is_empty());
}
