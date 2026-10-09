//! The daemon's own signing identity. It is registered with `system = 1` and role
//! `observer`, may sign only [`SYSTEM_KINDS`], and can never sign an agent operation.

use crate::identity::{check_clock, consume_nonce};
use crate::{Pub, SignedOp, Store, StoreError, hex};
use agora_core::{Role, TransitionError};
use agora_crypto::verify;
use rusqlite::{OptionalExtension, params};

/// The only operation kinds the system identity may sign.
pub const SYSTEM_KINDS: &[&str] = &[
    "job.start",
    "job.finish",
    "job.note",
    "card.job_sync",
    "timer.fire",
    "reconcile.expire",
    "reconcile.notice",
    "reconcile.reclaim",
    "reconcile.blocker",
    "reconcile.admission",
    "reconcile.quiet_close",
    "reconcile.claim_expire",
];

impl Store {
    /// Register `pk` as THE system identity (system=1, role observer); a previous system row is marked revoked.
    /// Idempotent: registering the current system key again changes nothing.
    pub fn register_system_identity(&mut self, now: i64, pk: &Pub) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(bool, bool, String)> = tx
            .query_row(
                "SELECT system, revoked, role FROM identities WHERE pubkey=?",
                [pk.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((true, false, role)) if role == "observer" => return Ok(()),
            Some((false, _, _)) => {
                return Err(StoreError::Invalid(
                    "the system key is already registered as a regular identity",
                ));
            }
            _ => {}
        }
        tx.execute(
            "UPDATE identities SET revoked=1 WHERE system=1 AND pubkey<>?",
            [pk.as_slice()],
        )?;
        tx.execute(
            "INSERT INTO identities(pubkey,label,role,revoked,first_seen,system) VALUES (?,?,?,0,?,1)
             ON CONFLICT(pubkey) DO UPDATE SET role='observer', revoked=0, system=1",
            params![pk.as_slice(), "system", "observer", now],
        )?;
        Self::append(
            &tx,
            now,
            &hex(pk),
            "identity.system",
            &hex(pk),
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    /// The active system identity, if one is registered.
    pub fn system_identity(&self) -> Result<Option<Pub>, StoreError> {
        let raw: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT pubkey FROM identities WHERE system=1 AND revoked=0 ORDER BY first_seen DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|v| Pub::try_from(v.as_slice()).ok()))
    }
}

/// Verify a system operation end to end and consume its nonce: the actor must be the
/// active system identity and `kind` must be in [`SYSTEM_KINDS`].
pub(crate) fn authorize_system(
    tx: &rusqlite::Transaction,
    now: i64,
    op: &SignedOp,
    kind: &str,
) -> Result<(), StoreError> {
    if op.kind != kind {
        return Err(StoreError::Invalid("wrong operation kind"));
    }
    // The same envelope caps as `authorize`, checked before any database work or signature check.
    if op.nonce.is_empty() || op.nonce.len() > 128 {
        return Err(StoreError::Invalid("nonce must be 1..=128 bytes"));
    }
    if op.subject.len() > 256 {
        return Err(StoreError::Invalid("subject longer than 256 bytes"));
    }
    if op.arg.len() > 64 * 1024 {
        return Err(StoreError::Invalid("arg longer than 64 KiB"));
    }
    check_clock(now, op.ts)?;
    if !SYSTEM_KINDS.contains(&kind) {
        return Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer,
        )));
    }
    verify(&op.actor, &op.signing_bytes(), &op.sig)?;
    if !is_current_system(tx, &op.actor)? {
        let role = stored_role_or_observer(tx, &op.actor)?;
        return Err(StoreError::Transition(TransitionError::Forbidden(role)));
    }
    consume_nonce(tx, now, &op.actor, &op.nonce, op.ts)
}

/// True when `pk` is the active system identity: system=1 and not revoked. A revoked
/// previous system key is refused even though `is_system` still reports it.
fn is_current_system(conn: &rusqlite::Connection, pk: &Pub) -> Result<bool, StoreError> {
    let current: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM identities WHERE pubkey=? AND system=1 AND revoked=0)",
        [pk.as_slice()],
        |r| r.get(0),
    )?;
    Ok(current)
}

/// The stored role of `pk` whether or not it is revoked, or Observer when the key is unknown.
fn stored_role_or_observer(conn: &rusqlite::Connection, pk: &Pub) -> Result<Role, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT role FROM identities WHERE pubkey=?",
            [pk.as_slice()],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        None => Ok(Role::Observer),
        Some(r) => Role::parse(&r).ok_or(StoreError::Invalid("corrupt role in database")),
    }
}

/// True when `pk` is a registered system identity (revoked or not).
pub(crate) fn is_system(conn: &rusqlite::Connection, pk: &Pub) -> Result<bool, StoreError> {
    let flag: Option<bool> = conn
        .query_row(
            "SELECT system FROM identities WHERE pubkey=?",
            [pk.as_slice()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(flag.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{MAX_SKEW_MS, authorize};
    use agora_crypto::Identity;

    const NOW: i64 = 1_700_000_000_000;

    /// A store whose system identity is seed 9; returns the store and that identity.
    fn store_with_system() -> (Store, Identity) {
        let mut s = Store::open_in_memory().unwrap();
        let sys = Identity::from_seed([9; 32]);
        s.register_system_identity(NOW, &sys.public()).unwrap();
        (s, sys)
    }

    /// Runs `authorize_system` in a transaction that commits only on success, as the writer does.
    fn run_system(s: &mut Store, op: &SignedOp, kind: &str) -> Result<(), StoreError> {
        let tx = s.conn.transaction().unwrap();
        authorize_system(&tx, NOW, op, kind)?;
        tx.commit().unwrap();
        Ok(())
    }

    /// Runs the agent `authorize` in the same committing style.
    fn run_agent(s: &mut Store, op: &SignedOp, kind: &str) -> Result<Role, StoreError> {
        let tx = s.conn.transaction().unwrap();
        let role = authorize(&tx, NOW, op, kind)?;
        tx.commit().unwrap();
        Ok(role)
    }

    #[test]
    fn system_identity_may_sign_a_system_kind() {
        let (mut s, sys) = store_with_system();
        let op = SignedOp::new("job.start", "task-1", "", NOW, "n-1").sign(&sys);
        run_system(&mut s, &op, "job.start").unwrap();
    }

    #[test]
    fn system_identity_is_refused_by_agent_authorization() {
        let (mut s, sys) = store_with_system();
        let card = SignedOp::new("card.move", "c1", "ready", NOW, "n-1").sign(&sys);
        assert!(matches!(
            run_agent(&mut s, &card, "card.move"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Observer
            )))
        ));
        let job = SignedOp::new("job.start", "t1", "", NOW, "n-2").sign(&sys);
        assert!(matches!(
            run_agent(&mut s, &job, "job.start"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Observer
            )))
        ));
    }

    #[test]
    fn worker_signing_a_system_kind_is_forbidden_with_its_role() {
        let (mut s, _sys) = store_with_system();
        let worker = Identity::from_seed([5; 32]);
        s.register_identity(NOW, &worker.public(), "worker")
            .unwrap();
        let op = SignedOp::new("job.start", "t1", "", NOW, "n-1").sign(&worker);
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Worker
            )))
        ));
    }

    #[test]
    fn unknown_key_signing_a_system_kind_is_forbidden_as_observer() {
        let (mut s, _sys) = store_with_system();
        let stranger = Identity::from_seed([6; 32]);
        let op = SignedOp::new("job.start", "t1", "", NOW, "n-1").sign(&stranger);
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Observer
            )))
        ));
    }

    #[test]
    fn system_identity_cannot_authorize_a_kind_outside_the_list() {
        let (mut s, sys) = store_with_system();
        let op = SignedOp::new("card.move", "c1", "ready", NOW, "n-1").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &op, "card.move"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Observer
            )))
        ));
    }

    #[test]
    fn wrong_operation_kind_is_invalid() {
        let (mut s, sys) = store_with_system();
        let op = SignedOp::new("job.note", "t1", "", NOW, "n-1").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Invalid("wrong operation kind"))
        ));
    }

    #[test]
    fn replayed_nonce_is_refused() {
        let (mut s, sys) = store_with_system();
        let op = SignedOp::new("job.start", "t1", "", NOW, "n-1").sign(&sys);
        run_system(&mut s, &op, "job.start").unwrap();
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Invalid("replayed operation"))
        ));
    }

    #[test]
    fn nonce_length_bounds_are_enforced() {
        let (mut s, sys) = store_with_system();
        let empty = SignedOp::new("job.start", "t1", "", NOW, "").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &empty, "job.start"),
            Err(StoreError::Invalid("nonce must be 1..=128 bytes"))
        ));
        let long = SignedOp::new("job.start", "t1", "", NOW, &"n".repeat(129)).sign(&sys);
        assert!(matches!(
            run_system(&mut s, &long, "job.start"),
            Err(StoreError::Invalid("nonce must be 1..=128 bytes"))
        ));
    }

    #[test]
    fn subject_and_arg_size_caps_are_enforced() {
        let (mut s, sys) = store_with_system();
        let subject = SignedOp::new("job.start", &"s".repeat(257), "", NOW, "n-1").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &subject, "job.start"),
            Err(StoreError::Invalid("subject longer than 256 bytes"))
        ));
        let arg =
            SignedOp::new("job.start", "t1", &"a".repeat(64 * 1024 + 1), NOW, "n-2").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &arg, "job.start"),
            Err(StoreError::Invalid("arg longer than 64 KiB"))
        ));
    }

    #[test]
    fn skewed_timestamp_is_refused() {
        let (mut s, sys) = store_with_system();
        let op = SignedOp::new("job.start", "t1", "", NOW + MAX_SKEW_MS + 1, "n-1").sign(&sys);
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Invalid("timestamp outside allowed clock skew"))
        ));
    }

    #[test]
    fn tampered_arg_fails_signature_verification() {
        let (mut s, sys) = store_with_system();
        let mut op = SignedOp::new("job.start", "t1", "", NOW, "n-1").sign(&sys);
        op.arg = "tampered".into();
        assert!(matches!(
            run_system(&mut s, &op, "job.start"),
            Err(StoreError::Crypto(_))
        ));
    }

    #[test]
    fn re_registering_a_new_system_key_revokes_the_old_one() {
        let (mut s, old) = store_with_system();
        let new = Identity::from_seed([10; 32]);
        s.register_system_identity(NOW, &new.public()).unwrap();
        let stale = SignedOp::new("job.start", "t1", "", NOW, "n-1").sign(&old);
        assert!(matches!(
            run_system(&mut s, &stale, "job.start"),
            Err(StoreError::Transition(TransitionError::Forbidden(
                Role::Observer
            )))
        ));
        let fresh = SignedOp::new("job.start", "t1", "", NOW, "n-2").sign(&new);
        run_system(&mut s, &fresh, "job.start").unwrap();
    }

    #[test]
    fn worker_still_passes_agent_authorization() {
        let (mut s, _sys) = store_with_system();
        let worker = Identity::from_seed([5; 32]);
        s.register_identity(NOW, &worker.public(), "worker")
            .unwrap();
        let op = SignedOp::new("card.move", "c1", "ready", NOW, "n-1").sign(&worker);
        assert_eq!(run_agent(&mut s, &op, "card.move").unwrap(), Role::Worker);
    }
}
