//! Reactions and knowledge notes. Signed inputs are verified before they touch the database.

use crate::channels::is_member;
use crate::identity::{Pub, active_role, check_clock, consume_nonce, hex, need};
use crate::{Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_crypto::{Identity, canonical, verify};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct SignedReaction {
    pub kind: String,
    pub target: String,
    pub by: Pub,
    pub emoji: String,
    pub remove: bool,
    pub ts: i64,
    pub sig: Vec<u8>,
}

impl SignedReaction {
    fn signing_bytes(&self) -> Vec<u8> {
        canonical(
            "react/v1",
            &[
                self.kind.as_bytes(),
                self.target.as_bytes(),
                &self.by,
                self.emoji.as_bytes(),
                &[self.remove as u8],
                &self.ts.to_le_bytes(),
            ],
        )
    }

    pub fn sign(mut self, id: &Identity) -> Self {
        self.by = id.public();
        self.sig = id.sign(&self.signing_bytes()).to_vec();
        self
    }
}

#[derive(Debug, Clone)]
pub struct SignedNote {
    pub id: String,
    pub author: Pub,
    pub ts: i64,
    pub title: String,
    pub body: String,
    pub tags: String,
    pub supersedes: Option<String>,
    pub sig: Vec<u8>,
}

impl SignedNote {
    fn signing_bytes(&self) -> Vec<u8> {
        canonical(
            "note/v1",
            &[
                self.id.as_bytes(),
                &self.author,
                &self.ts.to_le_bytes(),
                self.title.as_bytes(),
                self.body.as_bytes(),
                self.tags.as_bytes(),
                self.supersedes.as_deref().unwrap_or("").as_bytes(),
            ],
        )
    }

    pub fn sign(mut self, id: &Identity) -> Self {
        self.author = id.public();
        self.sig = id.sign(&self.signing_bytes()).to_vec();
        self
    }
}

impl Store {
    /// Add or remove a reaction. Any active identity may react. Removing a reaction
    /// that does not exist succeeds without an event.
    pub fn react(&mut self, now: i64, r: &SignedReaction) -> Result<(), StoreError> {
        if r.emoji.is_empty() || r.emoji.len() > 64 {
            return Err(StoreError::Invalid("reaction must be 1..=64 bytes"));
        }
        if r.target.len() > 256 {
            return Err(StoreError::Invalid("reaction target longer than 256 bytes"));
        }
        check_clock(now, r.ts)?;
        verify(&r.by, &r.signing_bytes(), &r.sig)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_role(&tx, &r.by)?;
        // Signed reactions have no op nonce; the signed content is the one-use token,
        // so an exact resubmission of an earlier add or remove is refused.
        consume_nonce(
            &tx,
            now,
            &r.by,
            &format!("react:{}", agora_crypto::content_hash(&r.signing_bytes())),
            r.ts,
        )?;
        let exists: bool = match r.kind.as_str() {
            "message" => {
                let channel: Option<String> = tx
                    .query_row(
                        "SELECT channel FROM messages WHERE id=?",
                        [&r.target],
                        |x| x.get(0),
                    )
                    .optional()?;
                match channel {
                    None => false,
                    Some(channel) => {
                        if !is_member(&tx, &channel, &r.by)? {
                            return Err(StoreError::NotMember(channel));
                        }
                        true
                    }
                }
            }
            "file" => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?)",
                [&r.target],
                |x| x.get(0),
            )?,
            "card" => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)",
                [&r.target],
                |x| x.get(0),
            )?,
            _ => return Err(StoreError::Invalid("unknown reaction target kind")),
        };
        if !exists {
            return Err(StoreError::NotFound(r.target.clone()));
        }
        let event = if r.remove {
            let removed = tx.execute(
                "DELETE FROM reactions WHERE kind=? AND target=? AND by=? AND emoji=?",
                params![r.kind, r.target, r.by.as_slice(), r.emoji],
            )?;
            if removed == 0 {
                // Nothing to remove: keep the consumed nonce, record no event.
                tx.commit()?;
                return Ok(());
            }
            "reaction.removed"
        } else {
            tx.execute(
                "INSERT OR REPLACE INTO reactions(kind,target,by,emoji,ts,sig) VALUES (?,?,?,?,?,?)",
                params![r.kind, r.target, r.by.as_slice(), r.emoji, r.ts, r.sig],
            )?;
            "reaction.added"
        };
        let data = json!({"kind": r.kind, "target": r.target, "emoji": r.emoji});
        Store::append(&tx, now, &hex(&r.by), event, &r.target, &data)?;
        tx.commit()?;
        Ok(())
    }

    pub fn reaction_counts(
        &self,
        kind: &str,
        target: &str,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT emoji, COUNT(*) FROM reactions WHERE kind=? AND target=? GROUP BY emoji ORDER BY 2 DESC, 1",
        )?;
        let rows = st.query_map(params![kind, target], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Publish a knowledge note, optionally superseding an earlier one.
    pub fn publish_note(&mut self, now: i64, n: &SignedNote) -> Result<(), StoreError> {
        // Field caps come before any signature work.
        if n.id.is_empty() || n.id.len() > 256 {
            return Err(StoreError::Invalid("note id must be 1..=256 bytes"));
        }
        if n.title.is_empty() || n.title.len() > 512 {
            return Err(StoreError::Invalid("note title must be 1..=512 bytes"));
        }
        if n.body.len() > crate::MAX_BODY {
            return Err(StoreError::Invalid("note body too large"));
        }
        if n.tags.len() > 1024 {
            return Err(StoreError::Invalid("note tags longer than 1024 bytes"));
        }
        if n.supersedes.as_deref().is_some_and(|k| k.len() > 256) {
            return Err(StoreError::Invalid("supersedes longer than 256 bytes"));
        }
        check_clock(now, n.ts)?;
        verify(&n.author, &n.signing_bytes(), &n.sig)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = active_role(&tx, &n.author)?;
        need(role, Role::Worker)?;
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge WHERE id=?)",
            [&n.id],
            |x| x.get(0),
        )?;
        if duplicate {
            return Err(StoreError::Invalid("note already exists"));
        }
        if let Some(k) = &n.supersedes {
            let prev_author: Option<Vec<u8>> = tx
                .query_row("SELECT author FROM knowledge WHERE id=?", [k], |x| x.get(0))
                .optional()?;
            let prev_author = prev_author.ok_or_else(|| StoreError::NotFound(k.clone()))?;
            if prev_author.as_slice() != n.author.as_slice() && role < Role::Supervisor {
                return Err(StoreError::Transition(TransitionError::Forbidden(role)));
            }
        }
        tx.execute(
            "INSERT INTO knowledge(id,author,ts,title,body,tags,supersedes,sig) VALUES (?,?,?,?,?,?,?,?)",
            params![n.id, n.author.as_slice(), n.ts, n.title, n.body, n.tags, n.supersedes, n.sig],
        )?;
        let data = json!({"supersedes": n.supersedes});
        Store::append(&tx, now, &hex(&n.author), "note.published", &n.id, &data)?;
        tx.commit()?;
        Ok(())
    }

    /// Full-text search over current (non-superseded) notes, best match first.
    pub fn search_notes(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>, StoreError> {
        // NULs are dropped, and the query is bounded to 1024 chars and 16 terms.
        // Each term is quoted so user input can't inject FTS operators.
        let bounded: String = query.chars().filter(|c| *c != '\0').take(1024).collect();
        let q = bounded
            .split_whitespace()
            .take(16)
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        if q.is_empty() {
            return Ok(vec![]);
        }
        let mut st = self.conn.prepare_cached(
            "SELECT k.id, k.title FROM knowledge_fts f JOIN knowledge k ON k.rowid=f.rowid
             WHERE knowledge_fts MATCH ?1
               AND NOT EXISTS (SELECT 1 FROM knowledge n WHERE n.supersedes=k.id)
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = st.query_map(params![q, limit.clamp(0, 200)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
