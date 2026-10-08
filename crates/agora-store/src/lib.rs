//! SQLite store. The `events` table is the source of truth; `cards` is a
//! projection updated in the same transaction as each appended event.

use agora_core::{check_transition, CardState, Gate, Role, TransitionError};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("card {0} not found")]
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
const MIGRATIONS: &[&str] = &[r#"
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
"#, include_str!("migration2.sql")];

mod collab;
pub use collab::*;

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

    pub fn create_card(
        &mut self,
        now: i64,
        actor: &str,
        id: &str,
        title: &str,
        lane: &str,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO cards(id,title,lane,state,created,updated) VALUES (?,?,?,?,?,?)",
            params![id, title, lane, CardState::Backlog.as_str(), now, now],
        )?;
        Self::append(&tx, now, actor, "card.created", id, &serde_json::json!({"title": title, "lane": lane}))?;
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

    pub fn add_evidence(&mut self, now: i64, actor: &str, card: &str, r: &str) -> Result<(), StoreError> {
        self.card_state(card)?;
        let tx = self.conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO evidence(card,ref,by) VALUES (?,?,?)", params![card, r, actor])?;
        Self::append(&tx, now, actor, "card.evidence", card, &serde_json::json!({"ref": r}))?;
        Ok(tx.commit()?)
    }

    pub fn approve(&mut self, now: i64, actor: &str, card: &str) -> Result<(), StoreError> {
        self.card_state(card)?;
        let tx = self.conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO approvals(card,by) VALUES (?,?)", params![card, actor])?;
        Self::append(&tx, now, actor, "card.approved", card, &serde_json::json!({}))?;
        Ok(tx.commit()?)
    }

    /// Move a card, enforcing the state machine and the done gate. Claiming
    /// sets the owner; returning a claim to `ready` clears it.
    pub fn transition(
        &mut self,
        now: i64,
        actor: &str,
        role: Role,
        card: &str,
        to: CardState,
        require_review: bool,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let (state, owner): (String, Option<String>) = tx
            .query_row("SELECT state, owner FROM cards WHERE id=?", [card], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or_else(|| StoreError::NotFound(card.into()))?;
        let from = CardState::parse(&state).ok_or(StoreError::BadState(state))?;
        let has_evidence: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence WHERE card=?)", [card], |r| r.get(0))?;
        let independent_approval: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM approvals WHERE card=?1 AND by IS NOT ?2)",
            params![card, owner],
            |r| r.get(0),
        )?;
        check_transition(from, to, role, Gate { has_evidence, independent_approval, require_review })?;
        let new_owner = match to {
            CardState::Claimed => Some(actor.to_string()),
            CardState::Ready | CardState::Backlog => None,
            _ => owner,
        };
        tx.execute(
            "UPDATE cards SET state=?, owner=?, updated=? WHERE id=?",
            params![to.as_str(), new_owner, now, card],
        )?;
        Self::append(&tx, now, actor, "card.moved", card, &serde_json::json!({"from": from.as_str(), "to": to.as_str()}))?;
        Ok(tx.commit()?)
    }

    pub fn event_count(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.create_card(1, "sup", "c1", "Port scheduler", "work").unwrap();
        s
    }

    #[test]
    fn card_lifecycle_with_review_gate() {
        let mut s = store();
        s.transition(2, "sup", Role::Supervisor, "c1", CardState::Ready, true).unwrap();
        s.transition(3, "alice", Role::Worker, "c1", CardState::Claimed, true).unwrap();
        s.transition(4, "alice", Role::Worker, "c1", CardState::Doing, true).unwrap();
        s.transition(5, "alice", Role::Worker, "c1", CardState::Review, true).unwrap();
        // No evidence yet.
        assert!(matches!(
            s.transition(6, "alice", Role::Worker, "c1", CardState::Done, true),
            Err(StoreError::Transition(TransitionError::EvidenceRequired))
        ));
        s.add_evidence(6, "alice", "c1", "/tmp/log").unwrap();
        // The owner's own approval does not count.
        s.approve(7, "alice", "c1").unwrap();
        assert!(matches!(
            s.transition(8, "alice", Role::Worker, "c1", CardState::Done, true),
            Err(StoreError::Transition(TransitionError::ReviewRequired))
        ));
        s.approve(9, "bob", "c1").unwrap();
        s.transition(10, "alice", Role::Worker, "c1", CardState::Done, true).unwrap();
        assert_eq!(s.card_state("c1").unwrap(), CardState::Done);
    }

    #[test]
    fn failed_transition_appends_no_event() {
        let mut s = store();
        let before = s.event_count().unwrap();
        assert!(s.transition(2, "alice", Role::Worker, "c1", CardState::Ready, false).is_err());
        assert_eq!(s.event_count().unwrap(), before);
    }

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

    #[test]
    fn missing_card_is_not_found() {
        let s = store();
        assert!(matches!(s.card_state("nope"), Err(StoreError::NotFound(_))));
    }
}
