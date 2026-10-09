use super::helpers::iso;
use super::*;
use crate::hex;
use agora_crypto::Identity;
use agora_crypto::pseudonym;
use std::sync::atomic::{AtomicU64, Ordering};

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;

static NONCES: AtomicU64 = AtomicU64::new(0);

/// An operation signed at `ts` with a nonce no earlier call in this process used.
fn op(id: &Identity, kind: &str, subject: &str, arg: &str, ts: i64) -> SignedOp {
    let nonce = format!("n{}", NONCES.fetch_add(1, Ordering::Relaxed));
    SignedOp::new(kind, subject, arg, ts, &nonce).sign(id)
}

fn key(id: &Identity) -> String {
    hex(&id.public())
}

struct Fixture {
    s: Store,
    owner: Identity,
    ctl: Identity,
    sup: Identity,
    alice: Identity,
    bob: Identity,
    olive: Identity,
}

/// Owner bootstrapped; ctl is a controller, sup a supervisor, alice and bob workers, olive an observer.
fn fixture() -> Fixture {
    let mut s = Store::open_in_memory().expect("store");
    let owner = Identity::from_seed([1; 32]);
    let ctl = Identity::from_seed([2; 32]);
    let sup = Identity::from_seed([3; 32]);
    let alice = Identity::from_seed([4; 32]);
    let bob = Identity::from_seed([5; 32]);
    let olive = Identity::from_seed([6; 32]);
    s.bootstrap_owner(NOW, &owner.public()).expect("owner");
    for (id, label, role) in [
        (&ctl, "ctl", "controller"),
        (&sup, "sup", "supervisor"),
        (&alice, "alice", "worker"),
        (&bob, "bob", "worker"),
        (&olive, "olive", "observer"),
    ] {
        s.register_identity(NOW, &id.public(), label)
            .expect("register");
        s.grant_role(NOW, &op(&owner, "role.grant", &key(id), role, NOW))
            .expect("grant");
    }
    Fixture {
        s,
        owner,
        ctl,
        sup,
        alice,
        bob,
        olive,
    }
}

fn define_pool(s: &mut Store, ctl: &Identity, name: &str, capacity: f64, unit: &str) -> PoolRecord {
    let arg = serde_json::json!({"name": name, "capacity": capacity, "unit": unit}).to_string();
    s.pool_define(NOW, &op(ctl, "pool.define", name, &arg, NOW))
        .expect("pool defined")
}

fn acquire_path(
    s: &mut Store,
    who: &Identity,
    path: &str,
    ttl_s: Option<f64>,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({"kind": "path", "resource": path, "ttl_s": ttl_s}).to_string();
    s.lease_acquire(now, &op(who, "lease.acquire", "", &arg, now))
}

fn acquire_pool(
    s: &mut Store,
    who: &Identity,
    pool: &str,
    amount: f64,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({"kind": "pool", "resource": pool, "amount": amount}).to_string();
    s.lease_acquire(now, &op(who, "lease.acquire", "", &arg, now))
}

fn open_quiet(
    s: &mut Store,
    who: &Identity,
    expected_end: i64,
    now: i64,
) -> Result<LeaseRecord, StoreError> {
    let arg = serde_json::json!({"expected_end_ms": expected_end, "note": "tests"}).to_string();
    s.quiet_open(now, &op(who, "quiet.open", "", &arg, now))
}

fn held_message(path: &str, holder: &Identity, what: &str, until: Option<i64>) -> String {
    let until = until.map_or_else(|| "-".to_string(), iso);
    format!(
        "path {path} is held by {} ({what}) until {until}",
        pseudonym(&holder.public())
    )
}

#[test]
fn quiet_kind_is_refused_by_lease_acquire() {
    let mut f = fixture();
    let arg = r#"{"kind":"quiet","resource":"machine"}"#;
    let res =
        f.s.lease_acquire(NOW, &op(&f.alice, "lease.acquire", "", arg, NOW));
    match res {
        Err(StoreError::Invalid(message)) => assert_eq!(message, "use quiet.open"),
        other => panic!("expected use quiet.open, got {other:?}"),
    }
}

#[test]
fn path_must_be_absolute() {
    let mut f = fixture();
    for path in ["src/lib.rs", "", "~/x"] {
        let res = acquire_path(&mut f.s, &f.alice, path, None, NOW);
        assert!(matches!(res, Err(StoreError::Invalid(_))), "path {path:?}");
    }
}

#[test]
fn path_conflicts_on_overlap_only_at_path_boundaries() {
    let mut f = fixture();
    let held =
        acquire_path(&mut f.s, &f.alice, "/work/repo/src", None, NOW).expect("alice holds src");
    let overlapping = [
        "/work/repo/src/lib.rs",
        "/work/repo",
        "/work/repo/src/",
        "/work/repo/./src",
    ];
    for path in overlapping {
        let res = acquire_path(&mut f.s, &f.bob, path, None, NOW);
        assert!(matches!(res, Err(StoreError::Refused(_))), "path {path}");
    }
    acquire_path(&mut f.s, &f.bob, "/work/repo2", None, NOW)
        .expect("a string prefix alone is not an overlap");
    let res = acquire_path(&mut f.s, &f.bob, "/work/repo/src/lib.rs", None, NOW);
    let expected = held_message(
        "/work/repo/src/lib.rs",
        &f.alice,
        &format!("lease {}", held.id),
        Some(NOW + 15 * MIN),
    );
    match res {
        Err(StoreError::Refused(message)) => assert_eq!(message, expected),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn own_holder_may_take_an_overlapping_path() {
    let mut f = fixture();
    acquire_path(&mut f.s, &f.alice, "/a/b", None, NOW).expect("first");
    acquire_path(&mut f.s, &f.alice, "/a/b/c", None, NOW).expect("own overlap is allowed");
}

#[test]
fn expired_path_lease_no_longer_conflicts() {
    let mut f = fixture();
    acquire_path(&mut f.s, &f.alice, "/a/b", Some(10.0), NOW).expect("alice, 10 s");
    let before = acquire_path(&mut f.s, &f.bob, "/a/b", None, NOW + 9_999);
    assert!(matches!(before, Err(StoreError::Refused(_))));
    acquire_path(&mut f.s, &f.bob, "/a/b", None, NOW + 10_000).expect("expired at the boundary");
}

#[test]
fn claimed_and_running_task_files_conflict_with_other_identities() {
    let mut f = fixture();
    let alice = key(&f.alice);
    f.s.conn
        .execute_batch(&format!(
            "INSERT INTO work_tasks(id, author, queued, state, agent, files) VALUES \
             ('t-claimed', '{alice}', {NOW}, 'claimed', '{alice}', '[\"/proj/w/x.rs\"]'); \
             INSERT INTO work_tasks(id, author, queued, state, agent, files) VALUES \
             ('t-running', '{alice}', {NOW}, 'running', '{alice}', '[\"/proj/r\"]'); \
             INSERT INTO work_tasks(id, author, queued, state, agent, files) VALUES \
             ('t-done', '{alice}', {NOW}, 'done', '{alice}', '[\"/proj/z\"]');"
        ))
        .expect("tasks inserted");
    let claimed = acquire_path(&mut f.s, &f.bob, "/proj/w", None, NOW);
    match claimed {
        Err(StoreError::Refused(message)) => {
            assert!(message.ends_with("(task t-claimed) until -"), "{message}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(matches!(
        acquire_path(&mut f.s, &f.bob, "/proj/r/s", None, NOW),
        Err(StoreError::Refused(_))
    ));
    acquire_path(&mut f.s, &f.bob, "/proj/z", None, NOW).expect("a done task holds nothing");
    acquire_path(&mut f.s, &f.alice, "/proj/w", None, NOW)
        .expect("the claimant may take its own files");
}

#[test]
fn renew_is_holder_only_and_moves_the_expiry() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/r", Some(60.0), NOW).expect("alice");
    assert_eq!(lease.expires_ms, Some(NOW + MIN));
    let arg = r#"{"ttl_s":120}"#;
    let bob = f.s.lease_renew(
        NOW + 1_000,
        &op(&f.bob, "lease.renew", &lease.id, arg, NOW + 1_000),
    );
    assert!(matches!(
        bob,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    let olive = f.s.lease_renew(
        NOW + 1_000,
        &op(&f.olive, "lease.renew", &lease.id, arg, NOW + 1_000),
    );
    assert!(matches!(
        olive,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer
        )))
    ));
    let renewed =
        f.s.lease_renew(
            NOW + 5_000,
            &op(&f.alice, "lease.renew", &lease.id, arg, NOW + 5_000),
        )
        .expect("holder renews");
    assert_eq!(renewed.expires_ms, Some(NOW + 5_000 + 2 * MIN));
    assert_eq!(renewed.heartbeat_ms, NOW + 5_000);
}

#[test]
fn renew_of_an_expired_lease_is_refused() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/r", Some(10.0), NOW).expect("alice");
    let late = NOW + 10_000;
    let res =
        f.s.lease_renew(late, &op(&f.alice, "lease.renew", &lease.id, "", late));
    match res {
        Err(StoreError::Invalid(message)) => assert_eq!(message, "lease is not active"),
        other => panic!("expected not active, got {other:?}"),
    }
}

#[test]
fn release_is_holder_or_supervisor() {
    let mut f = fixture();
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/r", None, NOW).expect("alice");
    let bob =
        f.s.lease_release(NOW, &op(&f.bob, "lease.release", &lease.id, "", NOW));
    assert!(matches!(
        bob,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    f.s.lease_release(
        NOW + 1_000,
        &op(&f.sup, "lease.release", &lease.id, "", NOW + 1_000),
    )
    .expect("supervisor releases another's lease");
    let again = f.s.lease_release(
        NOW + 2_000,
        &op(&f.alice, "lease.release", &lease.id, "", NOW + 2_000),
    );
    match again {
        Err(StoreError::Invalid(message)) => assert_eq!(message, "lease is already released"),
        other => panic!("expected already released, got {other:?}"),
    }
    let released = f.s.leases(NOW + 2_000, None, None, false).expect("leases");
    assert_eq!(released[0].released_ms, Some(NOW + 1_000));
    assert!(
        f.s.leases(NOW + 2_000, None, None, true)
            .expect("leases")
            .is_empty()
    );
}

#[test]
fn lease_ttl_defaults_to_the_work_settings() {
    let mut f = fixture();
    f.s.conn
        .execute(
            "INSERT INTO work_settings(id, json) VALUES (1, '{\"lease_ttl_s\": 60.0}')",
            [],
        )
        .expect("settings row");
    let lease = acquire_path(&mut f.s, &f.alice, "/proj/t", None, NOW).expect("alice");
    assert_eq!(lease.expires_ms, Some(NOW + MIN));
}

#[test]
fn quiet_window_is_controller_only_and_one_at_a_time() {
    let mut f = fixture();
    let end = NOW + 3 * MIN;
    assert!(matches!(
        open_quiet(&mut f.s, &f.alice, end, NOW),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    assert!(matches!(
        open_quiet(&mut f.s, &f.sup, end, NOW),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Supervisor
        )))
    ));
    assert!(matches!(
        open_quiet(&mut f.s, &f.ctl, NOW, NOW),
        Err(StoreError::Invalid(_))
    ));
    let window = open_quiet(&mut f.s, &f.ctl, end, NOW).expect("controller opens");
    assert!(matches!(window.kind, LeaseKind::Quiet));
    assert_eq!(window.resource, "machine");
    assert_eq!(window.expected_end_ms, Some(end));
    match open_quiet(&mut f.s, &f.ctl, end, NOW) {
        Err(StoreError::Invalid(message)) => {
            assert_eq!(message, "a quiet window is already open")
        }
        other => panic!("expected one at a time, got {other:?}"),
    }
}

#[test]
fn quiet_close_is_holder_or_controller() {
    let mut f = fixture();
    let window = open_quiet(&mut f.s, &f.ctl, NOW + 3 * MIN, NOW).expect("controller opens");
    let alice =
        f.s.quiet_close(NOW, &op(&f.alice, "quiet.close", &window.id, "", NOW));
    assert!(matches!(
        alice,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    let sup =
        f.s.quiet_close(NOW, &op(&f.sup, "quiet.close", &window.id, "", NOW));
    assert!(matches!(
        sup,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Supervisor
        )))
    ));
    f.s.quiet_close(
        NOW + 1_000,
        &op(&f.ctl, "quiet.close", &window.id, "", NOW + 1_000),
    )
    .expect("holder closes");
    let twice = f.s.quiet_close(
        NOW + 2_000,
        &op(&f.ctl, "quiet.close", &window.id, "", NOW + 2_000),
    );
    assert!(matches!(twice, Err(StoreError::Invalid(_))));
    open_quiet(&mut f.s, &f.ctl, NOW + 3 * MIN, NOW + 2_000)
        .expect("a closed window frees the slot");
}

#[test]
fn leases_filter_by_kind_holder_and_activity() {
    let mut f = fixture();
    let alice_path = acquire_path(&mut f.s, &f.alice, "/proj/f1", None, NOW).expect("alice path");
    acquire_path(&mut f.s, &f.bob, "/proj/f2", None, NOW).expect("bob path");
    define_pool(&mut f.s, &f.ctl, "gpu", 1.0, "");
    acquire_pool(&mut f.s, &f.alice, "gpu", 1.0, NOW).expect("alice pool");
    f.s.lease_release(NOW, &op(&f.sup, "lease.release", &alice_path.id, "", NOW))
        .expect("release");
    let by_kind =
        f.s.leases(NOW, Some(LeaseKind::Path), None, false)
            .expect("kind filter");
    assert_eq!(by_kind.len(), 2);
    let by_holder =
        f.s.leases(NOW, None, Some(&key(&f.alice)), false)
            .expect("holder filter");
    assert_eq!(by_holder.len(), 2);
    assert_eq!(by_holder[0].holder_name, pseudonym(&f.alice.public()));
    let active = f.s.leases(NOW, None, None, true).expect("active filter");
    assert_eq!(active.len(), 2);
    assert!(active.iter().all(|l| l.released_ms.is_none()));
    let later =
        f.s.leases(NOW + 15 * MIN, None, None, true)
            .expect("after expiry");
    assert!(
        later.is_empty(),
        "every default-ttl lease has expired by then"
    );
}

#[cfg(test)]
mod breadth;
#[cfg(test)]
mod limits;
#[cfg(test)]
mod pools;
