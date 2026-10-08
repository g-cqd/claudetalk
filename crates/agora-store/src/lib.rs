//! SQLite store. The `events` table is the source of truth; `cards` is a
//! projection updated in the same transaction as each appended event.

use agora_core::{CardState, Gate, Role, TransitionError, check_transition};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("{0} not found")]
    NotFound(String),
    #[error("unknown card state {0:?} in database")]
    BadState(String),
    #[error(transparent)]
    Crypto(#[from] agora_crypto::CryptoError),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("not a member of channel {0}")]
    NotMember(String),
}

/// Each entry runs once, in order; `PRAGMA user_version` records progress.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE events (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      INTEGER NOT NULL,
    actor   TEXT    NOT NULL,
    kind    TEXT    NOT NULL,
    subject TEXT    NOT NULL,
    data    TEXT    NOT NULL DEFAULT '{}'
);
CREATE INDEX events_subject ON events(subject, seq);

CREATE TABLE cards (
    id       TEXT PRIMARY KEY,
    title    TEXT NOT NULL,
    lane     TEXT NOT NULL DEFAULT 'default',
    state    TEXT NOT NULL,
    owner    TEXT,
    created  INTEGER NOT NULL,
    updated  INTEGER NOT NULL
);
CREATE INDEX cards_state ON cards(state, lane);

CREATE TABLE evidence (
    card TEXT NOT NULL REFERENCES cards(id),
    ref  TEXT NOT NULL,
    by   TEXT NOT NULL,
    PRIMARY KEY (card, ref)
);
CREATE TABLE approvals (
    card TEXT NOT NULL REFERENCES cards(id),
    by   TEXT NOT NULL,
    PRIMARY KEY (card, by)
);
"#,
    include_str!("migration2.sql"),
];

mod channels;
mod identity;
mod messages;
mod social;
mod sprints;
pub use channels::WRAPPED_KEY_LEN;
pub use identity::*;
pub use messages::*;
pub use social::*;
pub use sprints::*;

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        let done: usize = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(done) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    pub(crate) fn append(
        tx: &rusqlite::Transaction,
        now: i64,
        actor: &str,
        kind: &str,
        subject: &str,
        data: &serde_json::Value,
    ) -> Result<i64, StoreError> {
        tx.execute(
            "INSERT INTO events(ts, actor, kind, subject, data) VALUES (?,?,?,?,?)",
            params![now, actor, kind, subject, data.to_string()],
        )?;
        Ok(tx.last_insert_rowid())
    }

    /// Create a card in `backlog`. The arg is `{"title": string, "lane"?: string}`.
    pub fn create_card(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.create")?;
        need(role, Role::Worker)?;
        if op.subject.is_empty() {
            return Err(StoreError::Invalid("card id must not be empty"));
        }
        let arg: serde_json::Value = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("card arg must be JSON"))?;
        let title = arg
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or(StoreError::Invalid("card title must be a non-empty string"))?;
        let lane = match arg.get("lane") {
            None => "default",
            Some(v) => v
                .as_str()
                .ok_or(StoreError::Invalid("card lane must be a string"))?,
        };
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)",
            [op.subject.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid("card already exists"));
        }
        tx.execute(
            "INSERT INTO cards(id,title,lane,state,created,updated) VALUES (?,?,?,?,?,?)",
            params![
                op.subject,
                title,
                lane,
                CardState::Backlog.as_str(),
                now,
                now
            ],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "card.created",
            &op.subject,
            &serde_json::json!({"title": title, "lane": lane}),
        )?;
        Ok(tx.commit()?)
    }

    pub fn card_state(&self, id: &str) -> Result<CardState, StoreError> {
        let s: Option<String> = self
            .conn
            .query_row("SELECT state FROM cards WHERE id=?", [id], |r| r.get(0))
            .optional()?;
        let s = s.ok_or_else(|| StoreError::NotFound(id.into()))?;
        CardState::parse(&s).ok_or(StoreError::BadState(s))
    }

    /// Attach an evidence reference (the arg) to a card.
    pub fn add_evidence(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.evidence")?;
        need(role, Role::Worker)?;
        if op.arg.is_empty() {
            return Err(StoreError::Invalid("evidence ref must not be empty"));
        }
        require_card(&tx, &op.subject)?;
        let by = op.actor_hex();
        tx.execute(
            "INSERT OR IGNORE INTO evidence(card,ref,by) VALUES (?,?,?)",
            params![op.subject, op.arg, by],
        )?;
        Self::append(
            &tx,
            now,
            &by,
            "card.evidence",
            &op.subject,
            &serde_json::json!({"ref": op.arg}),
        )?;
        Ok(tx.commit()?)
    }

    /// Record the actor's approval of a card. The arg is ignored. Only cards in
    /// `review` can be approved.
    pub fn approve(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.approve")?;
        need(role, Role::Worker)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM cards WHERE id=?",
                [op.subject.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let state = state.ok_or_else(|| StoreError::NotFound(op.subject.clone()))?;
        let from = CardState::parse(&state).ok_or(StoreError::BadState(state))?;
        if !matches!(from, CardState::Review) {
            return Err(StoreError::Invalid("approval only in review"));
        }
        let by = op.actor_hex();
        tx.execute(
            "INSERT OR IGNORE INTO approvals(card,by) VALUES (?,?)",
            params![op.subject, by],
        )?;
        Self::append(
            &tx,
            now,
            &by,
            "card.approved",
            &op.subject,
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    /// Move a card to the state named by the arg, enforcing the state machine and
    /// the done gate. Claiming sets the owner; returning a claim to `ready` or
    /// `backlog` clears it. Leaving `backlog` (to any state) needs a supervisor.
    /// Work that is claimed, doing or in review, and blocked work that has an
    /// owner, may only be moved by its owner or by a supervisor.
    /// Approvals are cleared when the card returns to the pool or re-enters work.
    pub fn transition(
        &mut self,
        now: i64,
        op: &SignedOp,
        require_review: bool,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.move")?;
        let to = CardState::parse(&op.arg).ok_or(StoreError::Invalid("unknown card state"))?;
        let card = op.subject.as_str();
        let (state, owner): (String, Option<String>) = tx
            .query_row("SELECT state, owner FROM cards WHERE id=?", [card], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound(card.into()))?;
        let from = CardState::parse(&state).ok_or(StoreError::BadState(state))?;
        if from == CardState::Backlog {
            need(role, Role::Supervisor)?;
        }
        let has_evidence: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM evidence WHERE card=?)",
            [card],
            |r| r.get(0),
        )?;
        let independent_approval: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM approvals a JOIN identities i ON lower(hex(i.pubkey)) = a.by \
             WHERE a.card=?1 AND a.by IS NOT ?2 AND i.revoked=0)",
            params![card, owner],
            |r| r.get(0),
        )?;
        check_transition(
            from,
            to,
            role,
            Gate {
                has_evidence,
                independent_approval,
                require_review,
            },
        )?;
        let actor = op.actor_hex();
        let guarded = matches!(
            from,
            CardState::Claimed | CardState::Doing | CardState::Review
        ) || (matches!(from, CardState::Blocked) && owner.is_some());
        if guarded && role < Role::Supervisor && owner.as_deref() != Some(actor.as_str()) {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        let new_owner = match to {
            CardState::Claimed => Some(actor.clone()),
            CardState::Ready | CardState::Backlog => None,
            _ => owner,
        };
        if matches!(
            to,
            CardState::Claimed | CardState::Ready | CardState::Backlog | CardState::Doing
        ) {
            tx.execute("DELETE FROM approvals WHERE card=?", [card])?;
        }
        tx.execute(
            "UPDATE cards SET state=?, owner=?, updated=? WHERE id=?",
            params![to.as_str(), new_owner, now, card],
        )?;
        Self::append(
            &tx,
            now,
            &actor,
            "card.moved",
            card,
            &serde_json::json!({"from": from.as_str(), "to": to.as_str()}),
        )?;
        Ok(tx.commit()?)
    }

    /// Supervision watchdog: claimed/doing cards untouched for `max_idle`.
    pub fn stale_cards(
        &self,
        now: i64,
        max_idle: i64,
    ) -> Result<Vec<(String, Option<String>)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, owner FROM cards WHERE state IN ('claimed','doing') AND updated < ? ORDER BY updated",
        )?;
        let rows = st.query_map([now.saturating_sub(max_idle)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Return a stale claim to the pool. Supervisors only.
    pub fn reclaim_stale(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.reclaim")?;
        need(role, Role::Supervisor)?;
        let n = tx.execute(
            "UPDATE cards SET state='ready', owner=NULL, updated=? WHERE id=? AND state IN ('claimed','doing')",
            params![now, op.subject],
        )?;
        if n == 0 {
            return Err(StoreError::Invalid("card is not claimed or in progress"));
        }
        tx.execute("DELETE FROM approvals WHERE card=?", [op.subject.as_str()])?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "card.reclaimed",
            &op.subject,
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    pub fn event_count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }
}

/// Fails with `NotFound` unless the card exists. Runs inside the caller's transaction.
fn require_card(tx: &Transaction, id: &str) -> Result<(), StoreError> {
    let exists: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)", [id], |r| {
            r.get(0)
        })?;
    if !exists {
        return Err(StoreError::NotFound(id.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent_on_reopen() {
        let dir = std::env::temp_dir().join(format!("agora-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("db.sqlite");
        drop(Store::open(&p).unwrap());
        let s = Store::open(&p).unwrap();
        assert_eq!(s.event_count().unwrap(), 0);
        std::fs::remove_dir_all(dir).ok();
    }
}
