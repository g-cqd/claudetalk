//! Per-identity status: the signed, mutable "what I am doing now" record an
//! agent broadcasts to its channel peers (`state`, `harness`, `folder`,
//! `matter`). One row per (identity, harness): two harnesses that share a
//! folder share its key and pseudonym, and each broadcasts its own status.
//! Visibility follows the `Online` rule: the caller's own rows plus the rows of
//! identities that share at least one channel with the caller.

use crate::identity::{Pub, authorize, hex, need};
use crate::{SignedOp, Store, StoreError};
use agora_core::Role;
use agora_crypto::pseudonym;
use rusqlite::{TransactionBehavior, params};
use serde_json::json;

/// One status row, as returned by [`Store::statuses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    /// hex64
    pub pk: String,
    pub pseudonym: String,
    pub state: String,
    pub harness: String,
    pub folder: String,
    pub matter: String,
    pub ts: i64,
}

/// The `status.set` arg: `{"state" (required), "harness"?, "folder"?, "matter"?}`.
/// Unknown fields are refused, so a typo cannot silently drop a value.
#[derive(Debug, Default, PartialEq, Eq)]
struct StatusArg {
    state: String,
    harness: String,
    folder: String,
    matter: String,
}

impl StatusArg {
    fn parse(arg: &str) -> Result<Self, StoreError> {
        let v: serde_json::Value = serde_json::from_str(arg)
            .map_err(|_| StoreError::Invalid("status arg must be JSON"))?;
        let serde_json::Value::Object(map) = v else {
            return Err(StoreError::Invalid("status arg must be a JSON object"));
        };
        let mut out = Self::default();
        for (key, value) in &map {
            let text = value
                .as_str()
                .ok_or(StoreError::Invalid("status fields must be strings"))?;
            let field = match key.as_str() {
                "state" => &mut out.state,
                "harness" => &mut out.harness,
                "folder" => &mut out.folder,
                "matter" => &mut out.matter,
                _ => return Err(StoreError::Invalid("unknown status field")),
            };
            *field = text.to_string();
        }
        Ok(out)
    }
}

impl Store {
    /// Set the caller's status. The op kind is `status.set`, the subject must
    /// be the actor's own hex key, and the arg is
    /// `{"state", "harness"?, "folder"?, "matter"?}`. Any active identity may
    /// set its own status; the `(key, harness)` row replaces any earlier one,
    /// so two harnesses that share one folder key each keep their own row.
    pub fn set_status(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        // Field caps come before any signature work.
        let arg = StatusArg::parse(&op.arg)?;
        if arg.state.is_empty() || arg.state.len() > 64 {
            return Err(StoreError::Invalid("status state must be 1..=64 bytes"));
        }
        if arg.harness.len() > 64 {
            return Err(StoreError::Invalid("status harness longer than 64 bytes"));
        }
        if arg.folder.len() > 512 {
            return Err(StoreError::Invalid("status folder longer than 512 bytes"));
        }
        if arg.matter.len() > 512 {
            return Err(StoreError::Invalid("status matter longer than 512 bytes"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "status.set")?;
        need(role, Role::Worker)?;
        require_own_subject(op)?;
        tx.execute(
            "INSERT INTO statuses(pk,harness,state,folder,matter,ts) VALUES (?,?,?,?,?,?)
             ON CONFLICT(pk, harness) DO UPDATE SET
               state=excluded.state,
               folder=excluded.folder, matter=excluded.matter, ts=excluded.ts",
            params![
                op.actor.as_slice(),
                arg.harness,
                arg.state,
                arg.folder,
                arg.matter,
                now
            ],
        )?;
        let data = json!({"state": arg.state, "harness": arg.harness});
        Store::append(
            &tx,
            now,
            &op.actor_hex(),
            "status.set",
            &op.actor_hex(),
            &data,
        )?;
        Ok(tx.commit()?)
    }

    /// Clear the caller's status. The op kind is `status.clear`, the subject
    /// must be the actor's own hex key, and the arg is `{"harness"?}` (the
    /// row to clear; empty means the row with no harness). Clearing a status
    /// that does not exist succeeds without an event.
    pub fn clear_status(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let arg = StatusArg::parse(&op.arg)?;
        if arg.harness.len() > 64 {
            return Err(StoreError::Invalid("status harness longer than 64 bytes"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "status.clear")?;
        need(role, Role::Worker)?;
        require_own_subject(op)?;
        let deleted = tx.execute(
            "DELETE FROM statuses WHERE pk=? AND harness=?",
            params![op.actor.as_slice(), arg.harness],
        )?;
        if deleted > 0 {
            Store::append(
                &tx,
                now,
                &op.actor_hex(),
                "status.cleared",
                &op.actor_hex(),
                &json!({"harness": arg.harness}),
            )?;
        }
        Ok(tx.commit()?)
    }

    /// The statuses the caller may see: its own plus those of identities that
    /// share at least one channel with it, most recent first, then by key and
    /// harness. Never anyone else (the same privacy rule as `Online`). The
    /// caller must be active.
    pub fn statuses(&self, reader: &Pub) -> Result<Vec<StatusRow>, StoreError> {
        self.role_of(reader)?;
        self.status_rows(
            "SELECT s.pk, s.harness, s.state, s.folder, s.matter, s.ts FROM statuses s
             WHERE s.pk=?1 OR EXISTS (
                 SELECT 1 FROM channel_members a
                 JOIN channel_members b ON b.channel=a.channel
                 WHERE a.pubkey=s.pk AND b.pubkey=?1)
             ORDER BY s.ts DESC, s.pk, s.harness",
            params![reader.as_slice()],
        )
    }

    /// Supervisor or above: every status row, most recent first. This is the
    /// oversight read the stalker uses; a worker still gets `statuses` only.
    pub fn all_statuses(&self, reader: &Pub) -> Result<Vec<StatusRow>, StoreError> {
        let role = self.role_of(reader)?;
        need(role, Role::Supervisor)?;
        self.status_rows(
            "SELECT s.pk, s.harness, s.state, s.folder, s.matter, s.ts FROM statuses s
             ORDER BY s.ts DESC, s.pk, s.harness",
            [],
        )
    }

    /// Rows of one status query: `?1`-free SQL plus its params, mapped to
    /// [`StatusRow`] with the pseudonym of each key.
    fn status_rows<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<StatusRow>, StoreError> {
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st.query_map(params, |r| {
            let pk: Vec<u8> = r.get(0)?;
            Ok((
                pk,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out: Vec<StatusRow> = Vec::new();
        for row in rows {
            let (pk, harness, state, folder, matter, ts) = row?;
            let key: Pub = pk
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::Invalid("corrupt status key in database"))?;
            out.push(StatusRow {
                pk: hex(&key),
                pseudonym: pseudonym(&key),
                state,
                harness,
                folder,
                matter,
                ts,
            });
        }
        Ok(out)
    }
}

/// A status op may act only on its actor's own key.
fn require_own_subject(op: &SignedOp) -> Result<(), StoreError> {
    if op.subject != hex(&op.actor) {
        return Err(StoreError::Invalid(
            "status subject must be the actor's own key",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_SKEW_MS;
    use agora_crypto::Identity;

    const NOW: i64 = 1_700_000_000_000;

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let owner = Identity::from_seed([1; 32]);
        s.bootstrap_owner(NOW, &owner.public()).unwrap();
        s
    }

    fn worker(s: &mut Store, seed: u8) -> Identity {
        let id = Identity::from_seed([seed; 32]);
        s.register_identity(NOW, &id.public(), "w").unwrap();
        id
    }

    fn set_op(id: &Identity, arg: &str, nonce: &str) -> SignedOp {
        SignedOp::new("status.set", &hex(&id.public()), arg, NOW, nonce).sign(id)
    }

    fn clear_op(id: &Identity, arg: &str, nonce: &str) -> SignedOp {
        SignedOp::new("status.clear", &hex(&id.public()), arg, NOW, nonce).sign(id)
    }

    #[test]
    fn set_and_read_own_status_round_trips() {
        let mut s = store();
        let w = worker(&mut s, 5);
        let arg = r#"{"state":"busy","harness":"codex","folder":"~/x","matter":"card-1"}"#;
        s.set_status(NOW, &set_op(&w, arg, "n1")).unwrap();
        let rows = s.statuses(&w.public()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pk, hex(&w.public()));
        assert_eq!(rows[0].state, "busy");
        assert_eq!(rows[0].harness, "codex");
        assert_eq!(rows[0].folder, "~/x");
        assert_eq!(rows[0].matter, "card-1");
        assert_eq!(rows[0].pseudonym, agora_crypto::pseudonym(&w.public()));
    }

    #[test]
    fn two_harnesses_sharing_one_key_keep_separate_rows() {
        let mut s = store();
        let w = worker(&mut s, 5);
        let codex = r#"{"state":"busy","harness":"codex"}"#;
        let claude = r#"{"state":"idle","harness":"claude"}"#;
        s.set_status(NOW, &set_op(&w, codex, "n1")).unwrap();
        s.set_status(NOW + 1, &set_op(&w, claude, "n2")).unwrap();
        let rows = s.statuses(&w.public()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.pk == hex(&w.public())));
        assert!(
            rows.iter()
                .any(|r| r.harness == "codex" && r.state == "busy")
        );
        assert!(
            rows.iter()
                .any(|r| r.harness == "claude" && r.state == "idle")
        );
        // Clearing one harness leaves the other.
        s.clear_status(NOW + 2, &clear_op(&w, r#"{"harness":"claude"}"#, "n3"))
            .unwrap();
        let rows = s.statuses(&w.public()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, "codex");
    }

    #[test]
    fn set_replaces_the_earlier_row_of_the_same_harness() {
        let mut s = store();
        let w = worker(&mut s, 5);
        s.set_status(NOW, &set_op(&w, r#"{"state":"online"}"#, "n1"))
            .unwrap();
        s.set_status(NOW + 1, &set_op(&w, r#"{"state":"busy"}"#, "n2"))
            .unwrap();
        let rows = s.statuses(&w.public()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "busy");
        assert_eq!(rows[0].ts, NOW + 1);
    }

    #[test]
    fn unknown_arg_field_and_missing_state_are_refused() {
        let mut s = store();
        let w = worker(&mut s, 5);
        assert!(
            s.set_status(NOW, &set_op(&w, r#"{"state":"x","nope":1}"#, "n1"))
                .is_err()
        );
        assert!(s.set_status(NOW, &set_op(&w, "{}", "n2")).is_err());
        assert!(s.set_status(NOW, &set_op(&w, "not json", "n3")).is_err());
    }

    #[test]
    fn oversize_and_empty_fields_are_refused() {
        let mut s = store();
        let w = worker(&mut s, 5);
        let long = "x".repeat(65);
        assert!(
            s.set_status(NOW, &set_op(&w, &format!(r#"{{"state":"{long}"}}"#), "n1"))
                .is_err()
        );
        assert!(
            s.set_status(NOW, &set_op(&w, r#"{"state":""}"#, "n2"))
                .is_err()
        );
        let big = "x".repeat(513);
        assert!(
            s.set_status(
                NOW,
                &set_op(&w, &format!(r#"{{"state":"a","matter":"{big}"}}"#), "n3")
            )
            .is_err()
        );
    }

    #[test]
    fn another_identitys_subject_is_refused() {
        let mut s = store();
        let a = worker(&mut s, 5);
        let b = worker(&mut s, 6);
        let op = SignedOp::new(
            "status.set",
            &hex(&b.public()),
            r#"{"state":"x"}"#,
            NOW,
            "n1",
        )
        .sign(&a);
        assert!(s.set_status(NOW, &op).is_err());
        assert!(s.statuses(&a.public()).unwrap().is_empty());
    }

    #[test]
    fn replayed_set_and_wrong_kind_are_refused() {
        let mut s = store();
        let w = worker(&mut s, 5);
        let op = set_op(&w, r#"{"state":"x"}"#, "n1");
        s.set_status(NOW, &op).unwrap();
        assert!(s.set_status(NOW, &op).is_err());
        let wrong = SignedOp::new(
            "status.clear",
            &hex(&w.public()),
            r#"{"state":"x"}"#,
            NOW,
            "n2",
        )
        .sign(&w);
        assert!(s.set_status(NOW, &wrong).is_err());
    }

    #[test]
    fn unregistered_or_stale_clock_is_refused() {
        let mut s = store();
        let stranger = Identity::from_seed([9; 32]);
        assert!(
            s.set_status(NOW, &set_op(&stranger, r#"{"state":"x"}"#, "n1"))
                .is_err()
        );
        let w = worker(&mut s, 5);
        assert!(
            s.set_status(NOW + MAX_SKEW_MS + 1, &set_op(&w, r#"{"state":"x"}"#, "n2"))
                .is_err()
        );
    }

    #[test]
    fn clear_removes_the_row_and_is_idempotent() {
        let mut s = store();
        let w = worker(&mut s, 5);
        s.set_status(NOW, &set_op(&w, r#"{"state":"x"}"#, "n1"))
            .unwrap();
        s.clear_status(NOW, &clear_op(&w, "{}", "n2")).unwrap();
        assert!(s.statuses(&w.public()).unwrap().is_empty());
        // A second clear succeeds and appends no event.
        let before: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        s.clear_status(NOW, &clear_op(&w, "{}", "n3")).unwrap();
        let after: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn visibility_shows_channel_peers_but_not_strangers() {
        let mut s = store();
        let a = worker(&mut s, 5);
        let peer = worker(&mut s, 6);
        let stranger = worker(&mut s, 7);
        for (id, state, ts) in [
            (&a, "busy", NOW),
            (&peer, "online", NOW + 1),
            (&stranger, "idle", NOW + 2),
        ] {
            let arg = format!(r#"{{"state":"{state}","harness":"h"}}"#);
            let nonce = format!("n-{state}");
            s.set_status(ts, &set_op(id, &arg, &nonce)).unwrap();
        }
        // a and peer share a channel; stranger shares none with a.
        s.conn
            .execute(
                "INSERT INTO channels(id,kind,name,created) VALUES ('c1','group','',?1)",
                [NOW],
            )
            .unwrap();
        for pk in [&a.public(), &peer.public()] {
            s.conn
                .execute(
                    "INSERT INTO channel_members(channel,pubkey,role) VALUES ('c1',?,'member')",
                    [pk.as_slice()],
                )
                .unwrap();
        }
        let rows = s.statuses(&a.public()).unwrap();
        let seen: Vec<&str> = rows.iter().map(|r| r.state.as_str()).collect();
        assert_eq!(seen, ["online", "busy"]); // most recent ts first, then pk
        let stranger_rows = s.statuses(&stranger.public()).unwrap();
        assert!(stranger_rows.iter().all(|r| r.pk != hex(&a.public())));
    }

    #[test]
    fn statuses_requires_an_active_reader() {
        let s = store();
        let stranger = Identity::from_seed([9; 32]);
        assert!(s.statuses(&stranger.public()).is_err());
    }

    #[test]
    fn all_statuses_reads_every_row_for_a_supervisor_and_refuses_a_worker() {
        let mut s = store();
        let owner = Identity::from_seed([1; 32]);
        let w = worker(&mut s, 5);
        s.set_status(
            NOW,
            &set_op(&w, r#"{"state":"busy","harness":"codex"}"#, "n1"),
        )
        .unwrap();
        // The worker sees only its own rows.
        let own = s.statuses(&w.public()).unwrap();
        assert_eq!(own.len(), 1);
        // The owner (above supervisor) sees every row.
        let all = s.all_statuses(&owner.public()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state, "busy");
        assert_eq!(all[0].harness, "codex");
        // A worker is refused the oversight read.
        assert!(s.all_statuses(&w.public()).is_err());
        // So is a stranger.
        let stranger = Identity::from_seed([9; 32]);
        assert!(s.all_statuses(&stranger.public()).is_err());
    }
}
