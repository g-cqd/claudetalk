//! SQLite store. The `events` table is the source of truth; `cards` is a
//! projection updated in the same transaction as each appended event.

use agora_core::{CardState, Gate, Role, TransitionError, check_transition};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

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
    /// A refusal with a dynamic, user-facing message (work queue rules: "task X is claimed by A", …).
    #[error("{0}")]
    Refused(String),
    #[error("not a member of channel {0}")]
    NotMember(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
    include_str!("migration3.sql"),
    include_str!("migration4.sql"),
];

mod cards_ext;
mod channels;
mod identity;
mod jobs;
mod leases;
mod messages;
mod metrics;
mod policy;
mod reconcile;
mod settings_read;
mod social;
mod sprints;
mod system;
mod timers;
mod work_ops;
mod work_query;
pub use channels::{ChannelInfo, WRAPPED_KEY_LEN, keys_digest};
pub use identity::*;
pub use messages::*;
pub use social::*;
pub use sprints::*;
pub use system::SYSTEM_KINDS;

pub struct Store {
    pub(crate) conn: Connection,
    /// Where blobs larger than `SPILL_THRESHOLD` are written as content-addressed
    /// files. `None`: every blob stays inline in SQLite.
    pub(crate) blob_dir: Option<PathBuf>,
}

/// One row of the card board, as returned by [`Store::list_cards`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub state: CardState,
    /// Hex public key of the owner, if claimed.
    pub owner: Option<String>,
    pub updated: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?, None)
    }

    /// Open a database whose large blobs spill to files under `blob_dir`
    /// (created with mode 0700 if missing).
    pub fn open_with_blob_dir(path: &Path, blob_dir: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = blob_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Mode 0700 applies only to a directory this call creates. An existing directory
        // keeps its mode, whatever it is.
        match std::fs::DirBuilder::new().mode(0o700).create(blob_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && blob_dir.is_dir() => {}
            Err(e) => return Err(e.into()),
        }
        Self::init(Connection::open(path)?, Some(blob_dir.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, blob_dir: Option<PathBuf>) -> Result<Self, StoreError> {
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
        Ok(Self { conn, blob_dir })
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
        let fields = cards_ext::NewCardFields::parse(&arg)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)",
            [op.subject.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid("card already exists"));
        }
        tx.execute(
            "INSERT INTO cards(id,title,lane,state,created,updated,priority,value,size,team) \
             VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                op.subject,
                title,
                lane,
                CardState::Backlog.as_str(),
                now,
                now,
                fields.priority,
                fields.value,
                fields.size,
                fields.team,
            ],
        )?;
        fields.insert_deps(&tx, &op.subject)?;
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
        // The WIP gate applies only when work enters claimed or doing from outside them, so a
        // card already in progress may move between the two in a lane over its limit.
        if matches!(to, CardState::Claimed | CardState::Doing)
            && !matches!(from, CardState::Claimed | CardState::Doing)
        {
            cards_ext::check_wip(&tx, card, require_review)?;
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

    /// Cards newest-updated first (ties by id), optionally filtered by state and lane.
    /// `limit` is clamped to 0..=500. No identity gate here: the daemon checks the
    /// reader is an active identity first.
    pub fn list_cards(
        &self,
        state: Option<CardState>,
        lane: Option<&str>,
        limit: i64,
    ) -> Result<Vec<CardRow>, StoreError> {
        let state = state.map(CardState::as_str);
        let mut st = self.conn.prepare_cached(
            "SELECT id, title, lane, state, owner, updated FROM cards \
             WHERE (?1 IS NULL OR state = ?1) AND (?2 IS NULL OR lane = ?2) \
             ORDER BY updated DESC, id LIMIT ?3",
        )?;
        let rows = st.query_map(params![state, lane, limit.clamp(0, 500)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut cards = Vec::new();
        for row in rows {
            let (id, title, lane, state, owner, updated) = row?;
            let Some(state) = CardState::parse(&state) else {
                return Err(StoreError::BadState(state));
            };
            cards.push(CardRow {
                id,
                title,
                lane,
                state,
                owner,
                updated,
            });
        }
        Ok(cards)
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

    use agora_crypto::Identity;

    const NOW: i64 = 1_700_000_000_000;

    /// A store whose only identity is a bootstrapped owner, who may do every card move.
    fn owner_store() -> (Store, Identity) {
        let mut s = Store::open_in_memory().unwrap();
        let owner = Identity::from_seed([1; 32]);
        s.bootstrap_owner(NOW, &owner.public()).unwrap();
        (s, owner)
    }

    /// Signed at `now`; the nonce is derived from the op so each op in a test is unique.
    fn signed(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
        let nonce = format!("{kind}/{subject}/{arg}/{now}");
        SignedOp::new(kind, subject, arg, now, &nonce).sign(id)
    }

    fn create(s: &mut Store, owner: &Identity, id: &str, lane: &str, now: i64) {
        let arg = serde_json::json!({"title": id, "lane": lane}).to_string();
        s.create_card(now, &signed(owner, "card.create", id, &arg, now))
            .unwrap();
    }

    fn move_card(s: &mut Store, owner: &Identity, id: &str, to: &str) {
        s.transition(NOW, &signed(owner, "card.move", id, to, NOW), false)
            .unwrap();
    }

    fn ids(cards: &[CardRow]) -> Vec<&str> {
        cards.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn list_cards_filters_by_state() {
        let (mut s, owner) = owner_store();
        create(&mut s, &owner, "a", "x", NOW);
        create(&mut s, &owner, "b", "x", NOW);
        create(&mut s, &owner, "c", "x", NOW);
        move_card(&mut s, &owner, "b", "ready");
        move_card(&mut s, &owner, "c", "ready");
        move_card(&mut s, &owner, "c", "claimed");

        let backlog = s.list_cards(Some(CardState::Backlog), None, 10).unwrap();
        assert_eq!(ids(&backlog), ["a"]);
        let ready = s.list_cards(Some(CardState::Ready), None, 10).unwrap();
        assert_eq!(ids(&ready), ["b"]);
        let claimed = s.list_cards(Some(CardState::Claimed), None, 10).unwrap();
        assert_eq!(ids(&claimed), ["c"]);
        assert_eq!(claimed[0].state, CardState::Claimed);
        assert_eq!(claimed[0].owner, Some(hex(&owner.public())));
        assert_eq!(backlog[0].owner, None);
        let all = s.list_cards(None, None, 10).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn list_cards_filters_by_lane_and_combines_with_state() {
        let (mut s, owner) = owner_store();
        create(&mut s, &owner, "a", "x", NOW);
        create(&mut s, &owner, "b", "y", NOW);
        create(&mut s, &owner, "c", "x", NOW);
        move_card(&mut s, &owner, "c", "ready");

        let x = s.list_cards(None, Some("x"), 10).unwrap();
        assert_eq!(ids(&x), ["a", "c"]);
        assert_eq!(x[0].lane, "x");
        let y = s.list_cards(None, Some("y"), 10).unwrap();
        assert_eq!(ids(&y), ["b"]);
        let none = s.list_cards(None, Some("z"), 10).unwrap();
        assert!(none.is_empty());
        let both = s
            .list_cards(Some(CardState::Backlog), Some("x"), 10)
            .unwrap();
        assert_eq!(ids(&both), ["a"]);
    }

    #[test]
    fn list_cards_orders_newest_first_then_by_id() {
        let (mut s, owner) = owner_store();
        create(&mut s, &owner, "b", "x", NOW);
        create(&mut s, &owner, "a", "x", NOW);
        create(&mut s, &owner, "c", "x", NOW + 1_000);

        let cards = s.list_cards(None, None, 10).unwrap();
        assert_eq!(ids(&cards), ["c", "a", "b"]);
        assert_eq!(cards[0].updated, NOW + 1_000);
        assert_eq!(cards[1].updated, NOW);
    }

    #[test]
    fn list_cards_clamps_limit_to_zero_through_500() {
        let (mut s, owner) = owner_store();
        create(&mut s, &owner, "a", "x", NOW);
        create(&mut s, &owner, "b", "x", NOW);
        create(&mut s, &owner, "c", "x", NOW);

        assert!(s.list_cards(None, None, -1).unwrap().is_empty());
        assert!(s.list_cards(None, None, 0).unwrap().is_empty());
        assert_eq!(s.list_cards(None, None, 2).unwrap().len(), 2);

        // 600 more cards directly, so the upper clamp has something to cut.
        s.conn
            .execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
                 INSERT INTO cards(id, title, lane, state, created, updated)
                 SELECT 'bulk' || i, 't', 'bulk', 'backlog', 0, 0 FROM n;",
            )
            .unwrap();
        assert_eq!(s.list_cards(None, None, 10_000).unwrap().len(), 500);
    }

    #[test]
    fn list_cards_rejects_unknown_stored_state() {
        let (mut s, owner) = owner_store();
        create(&mut s, &owner, "a", "x", NOW);
        s.conn
            .execute("UPDATE cards SET state = 'bogus' WHERE id = 'a'", [])
            .unwrap();

        let err = s.list_cards(None, None, 10).unwrap_err();
        assert!(matches!(err, StoreError::BadState(ref st) if st == "bogus"));
    }
}
