//! Identities, roles, and the signed-operation envelope that authorizes every
//! state change. Roles are always read from the database inside the same
//! write transaction as the change (taken IMMEDIATE, so the read holds the
//! write lock); a caller can never assert its own role.

use crate::{Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_crypto::{Identity, canonical, verify};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub type Pub = [u8; 32];

/// Operations/messages whose timestamp differs from the daemon clock by more
/// than this are rejected, which bounds how long nonces must be remembered.
pub const MAX_SKEW_MS: i64 = 300_000;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Decodes exactly 64 lowercase hex digits. `from_str_radix` alone would also
/// accept uppercase and a leading '+', so the alphabet is checked first.
pub fn unhex32(s: &str) -> Option<Pub> {
    let b = s.as_bytes();
    if b.len() != 64
        || !b
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn check_clock(now: i64, ts: i64) -> Result<(), StoreError> {
    if now.abs_diff(ts) > MAX_SKEW_MS as u64 {
        return Err(StoreError::Invalid("timestamp outside allowed clock skew"));
    }
    Ok(())
}

pub(crate) fn need(role: Role, min: Role) -> Result<(), StoreError> {
    if role < min {
        return Err(StoreError::Transition(TransitionError::Forbidden(role)));
    }
    Ok(())
}

/// A signed request: who did what to which subject, once (nonce), and when.
#[derive(Debug, Clone)]
pub struct SignedOp {
    pub kind: String,
    pub subject: String,
    pub arg: String,
    pub ts: i64,
    pub nonce: String,
    pub actor: Pub,
    pub sig: Vec<u8>,
}

impl SignedOp {
    pub fn new(kind: &str, subject: &str, arg: &str, ts: i64, nonce: &str) -> Self {
        Self {
            kind: kind.into(),
            subject: subject.into(),
            arg: arg.into(),
            ts,
            nonce: nonce.into(),
            actor: [0; 32],
            sig: vec![],
        }
    }

    fn signing_bytes(&self) -> Vec<u8> {
        canonical(
            "op/v1",
            &[
                self.kind.as_bytes(),
                self.subject.as_bytes(),
                self.arg.as_bytes(),
                &self.ts.to_le_bytes(),
                self.nonce.as_bytes(),
                &self.actor,
            ],
        )
    }

    pub fn sign(mut self, id: &Identity) -> Self {
        self.actor = id.public();
        self.sig = id.sign(&self.signing_bytes()).to_vec();
        self
    }

    pub fn actor_hex(&self) -> String {
        hex(&self.actor)
    }
}

/// Role of a registered, non-revoked identity.
pub(crate) fn active_role(conn: &Connection, pk: &Pub) -> Result<Role, StoreError> {
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT role, revoked FROM identities WHERE pubkey=?",
            [pk.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Err(StoreError::Invalid("unknown identity")),
        Some((_, true)) => Err(StoreError::Invalid("identity revoked")),
        Some((r, false)) => Role::parse(&r).ok_or(StoreError::Invalid("corrupt role in database")),
    }
}

/// Records `nonce` for `actor` so it can be used only once. Old nonces are
/// garbage-collected first; a nonce still on file is a replay.
pub(crate) fn consume_nonce(
    tx: &Transaction,
    now: i64,
    actor: &Pub,
    nonce: &str,
    ts: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM nonces WHERE ts < ?",
        [now.saturating_sub(2 * MAX_SKEW_MS)],
    )?;
    let fresh = tx.execute(
        "INSERT OR IGNORE INTO nonces(actor,nonce,ts) VALUES (?,?,?)",
        params![actor.as_slice(), nonce, ts],
    )?;
    if fresh == 0 {
        return Err(StoreError::Invalid("replayed operation"));
    }
    Ok(())
}

/// Verify an operation end to end and consume its nonce. Must be called inside
/// the transaction that applies the change, so a failed change burns no nonce.
pub(crate) fn authorize(
    tx: &Transaction,
    now: i64,
    op: &SignedOp,
    kind: &str,
) -> Result<Role, StoreError> {
    if op.kind != kind {
        return Err(StoreError::Invalid("wrong operation kind"));
    }
    // Size caps come before any database work or signature check.
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
    verify(&op.actor, &op.signing_bytes(), &op.sig)?;
    let role = active_role(tx, &op.actor)?;
    consume_nonce(tx, now, &op.actor, &op.nonce, op.ts)?;
    Ok(role)
}

impl Store {
    /// Register a key (idempotent). Open registration is a local-mode policy;
    /// the daemon decides who may call this.
    pub fn register_identity(&mut self, now: i64, pk: &Pub, label: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO identities(pubkey,label,first_seen) VALUES (?,?,?)",
            params![pk.as_slice(), label, now],
        )?;
        Ok(())
    }

    /// Make `pk` the owner. Allowed exactly once, before any owner exists.
    pub fn bootstrap_owner(&mut self, now: i64, pk: &Pub) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let has_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM identities WHERE role='owner')",
            [],
            |r| r.get(0),
        )?;
        if has_owner {
            return Err(StoreError::Invalid("an owner already exists"));
        }
        tx.execute(
            "INSERT INTO identities(pubkey,label,role,first_seen) VALUES (?,?,?,?)
             ON CONFLICT(pubkey) DO UPDATE SET role='owner', revoked=0",
            params![pk.as_slice(), "owner", Role::Owner.as_str(), now],
        )?;
        Self::append(
            &tx,
            now,
            &hex(pk),
            "identity.owner",
            &hex(pk),
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    pub fn role_of(&self, pk: &Pub) -> Result<Role, StoreError> {
        active_role(&self.conn, pk)
    }

    /// Controllers may grant up to supervisor; only the owner grants controller/owner.
    /// Controllers and owners are protected: acting on one needs the owner. The last
    /// active owner cannot be demoted.
    pub fn grant_role(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let actor_role = authorize(&tx, now, op, "role.grant")?;
        need(actor_role, Role::Controller)?;
        let target =
            unhex32(&op.subject).ok_or(StoreError::Invalid("subject must be a 64-char hex key"))?;
        let new = Role::parse(&op.arg).ok_or(StoreError::Invalid("unknown role"))?;
        let target_role = active_role(&tx, &target)?;
        if target_role >= Role::Controller {
            need(actor_role, Role::Owner)?;
        }
        if new > Role::Supervisor {
            need(actor_role, Role::Owner)?;
        }
        if target_role == Role::Owner && new != Role::Owner && is_last_owner(&tx, &target)? {
            return Err(StoreError::Invalid("cannot remove the last owner"));
        }
        tx.execute(
            "UPDATE identities SET role=? WHERE pubkey=?",
            params![new.as_str(), target.as_slice()],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "role.granted",
            &hex(&target),
            &serde_json::json!({"role": new.as_str()}),
        )?;
        Ok(tx.commit()?)
    }

    /// Revoked keys can no longer authorize anything. Controllers or above only.
    /// Acting on a controller or owner needs the owner, and the last active owner
    /// cannot be revoked.
    pub fn revoke_identity(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let actor_role = authorize(&tx, now, op, "identity.revoke")?;
        need(actor_role, Role::Controller)?;
        let target =
            unhex32(&op.subject).ok_or(StoreError::Invalid("subject must be a 64-char hex key"))?;
        let target_role = active_role(&tx, &target)?;
        if target_role >= Role::Controller {
            need(actor_role, Role::Owner)?;
        }
        if target_role == Role::Owner && is_last_owner(&tx, &target)? {
            return Err(StoreError::Invalid("cannot remove the last owner"));
        }
        tx.execute(
            "UPDATE identities SET revoked=1 WHERE pubkey=?",
            [target.as_slice()],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "identity.revoked",
            &hex(&target),
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }
}

/// True when no other active owner exists, so removing `pk`'s owner role would
/// leave the store with none. Only meaningful for a `pk` that is an active owner.
fn is_last_owner(conn: &Connection, pk: &Pub) -> Result<bool, StoreError> {
    let others: i64 = conn.query_row(
        "SELECT COUNT(*) FROM identities WHERE role='owner' AND revoked=0 AND pubkey<>?",
        [pk.as_slice()],
        |r| r.get(0),
    )?;
    Ok(others == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        let k = [0xab; 32];
        assert_eq!(unhex32(&hex(&k)), Some(k));
        assert_eq!(unhex32("zz"), None);
        assert_eq!(unhex32(&"é".repeat(32)), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
    }

    #[test]
    fn unhex32_rejects_uppercase_sign_and_wrong_length() {
        let k = [0xab; 32];
        assert_eq!(unhex32(&hex(&k).to_uppercase()), None);
        assert_eq!(unhex32(&format!("+f{}", "0".repeat(62))), None);
        assert_eq!(unhex32(&format!("0+{}", "0".repeat(62))), None);
        assert_eq!(unhex32(&"ab".repeat(31)), None);
        assert_eq!(unhex32(&"ab".repeat(33)), None);
    }

    #[test]
    fn clock_skew_is_symmetric() {
        assert!(check_clock(1_000_000, 1_000_000 + MAX_SKEW_MS).is_ok());
        assert!(check_clock(1_000_000, 1_000_000 + MAX_SKEW_MS + 1).is_err());
        assert!(check_clock(1_000_000, 1_000_000 - MAX_SKEW_MS - 1).is_err());
    }

    #[test]
    fn clock_skew_handles_extreme_timestamps_without_panicking() {
        assert!(check_clock(1_700_000_000_000, i64::MIN).is_err());
        assert!(check_clock(i64::MAX, i64::MIN).is_err());
        assert!(check_clock(i64::MIN, i64::MAX).is_err());
        assert!(check_clock(1_700_000_000_000, 1_700_000_000_000 - (1i64 << 62)).is_err());
        assert!(check_clock(i64::MIN, i64::MIN).is_ok());
    }
}
