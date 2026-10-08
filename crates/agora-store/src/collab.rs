//! Channels, signed messages, attachments, reactions, sprints, knowledge,
//! and the supervision watchdog. Every write that carries a signature is
//! verified here, so a bad sender can never reach the database.

use crate::{Store, StoreError};
use agora_crypto::{canonical, content_hash, verify, Identity};
use rusqlite::{params, OptionalExtension};

pub type Pub = [u8; 32];
/// (seq, id, sender, body, encrypted)
pub type MessageRow = (i64, String, Pub, Vec<u8>, bool);

/// Largest blob accepted (clients encrypt before upload when needed).
pub const MAX_BLOB: usize = 16 * 1024 * 1024;
pub const MAX_BODY: usize = 256 * 1024;

pub struct Attachment {
    pub hash: String,
    pub name: String,
    pub mime: String,
}

pub struct SignedMessage {
    pub id: String,
    pub channel: String,
    pub thread_of: Option<String>,
    pub sender: Pub,
    pub ts: i64,
    pub encrypted: bool,
    /// Plaintext, or nonce||ciphertext when `encrypted`.
    pub body: Vec<u8>,
    pub attachments: Vec<Attachment>,
    pub sig: Vec<u8>,
}

impl SignedMessage {
    fn signing_bytes(&self) -> Vec<u8> {
        let ts = self.ts.to_le_bytes();
        let enc = [self.encrypted as u8];
        let att: Vec<u8> = self
            .attachments
            .iter()
            .flat_map(|a| canonical("att", &[a.hash.as_bytes(), a.name.as_bytes(), a.mime.as_bytes()]))
            .collect();
        canonical(
            "msg/v1",
            &[
                self.id.as_bytes(),
                self.channel.as_bytes(),
                self.thread_of.as_deref().unwrap_or("").as_bytes(),
                &self.sender,
                &ts,
                &enc,
                &self.body,
                &att,
            ],
        )
    }

    pub fn sign(mut self, id: &Identity) -> Self {
        self.sender = id.public();
        self.sig = id.sign(&self.signing_bytes()).to_vec();
        self
    }
}

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

#[derive(Debug, PartialEq, Eq, Default)]
pub struct SprintSummary {
    pub total: i64,
    pub done: i64,
    pub in_flight: i64,
    pub blocked: i64,
}

impl Store {
    pub fn create_channel(
        &mut self,
        now: i64,
        id: &str,
        kind: &str,
        name: &str,
        parent: Option<&str>,
        members: &[(Pub, &str, Option<Vec<u8>>)],
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO channels(id,kind,name,parent,created) VALUES (?,?,?,?,?)",
            params![id, kind, name, parent, now],
        )?;
        for (pk, role, wrapped) in members {
            tx.execute(
                "INSERT INTO channel_members(channel,pubkey,role,wrapped_key) VALUES (?,?,?,?)",
                params![id, pk.as_slice(), role, wrapped],
            )?;
        }
        Ok(tx.commit()?)
    }

    pub fn wrapped_key(&self, channel: &str, member: &Pub) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT wrapped_key FROM channel_members WHERE channel=? AND pubkey=?",
                params![channel, member.as_slice()],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Store a blob; the hash is always recomputed, never trusted.
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<String, StoreError> {
        if bytes.len() > MAX_BLOB {
            return Err(StoreError::Invalid("blob too large"));
        }
        let hash = content_hash(bytes);
        self.conn.execute(
            "INSERT OR IGNORE INTO blobs(hash,size,bytes) VALUES (?,?,?)",
            params![hash, bytes.len() as i64, bytes],
        )?;
        Ok(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT bytes FROM blobs WHERE hash=?", [hash], |r| r.get(0))
            .optional()?)
    }

    /// Verify, authorize and persist a message. Returns its channel sequence number.
    pub fn post_message(&mut self, m: &SignedMessage) -> Result<i64, StoreError> {
        if m.body.len() > MAX_BODY {
            return Err(StoreError::Invalid("body too large"));
        }
        verify(&m.sender, &m.signing_bytes(), &m.sig)?;
        let tx = self.conn.transaction()?;
        let member: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM channel_members WHERE channel=? AND pubkey=?)",
            params![m.channel, m.sender.as_slice()],
            |r| r.get(0),
        )?;
        if !member {
            return Err(StoreError::NotMember(m.channel.clone()));
        }
        if let Some(parent) = &m.thread_of {
            let same: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id=? AND channel=?)",
                params![parent, m.channel],
                |r| r.get(0),
            )?;
            if !same {
                return Err(StoreError::Invalid("thread parent not in this channel"));
            }
        }
        for a in &m.attachments {
            let known: bool =
                tx.query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?)", [&a.hash], |r| r.get(0))?;
            if !known {
                return Err(StoreError::Invalid("attachment blob missing"));
            }
        }
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq),0)+1 FROM messages", [], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO messages(id,seq,channel,thread_of,sender,ts,encrypted,body,sig)
             VALUES (?,?,?,?,?,?,?,?,?)",
            params![
                m.id, seq, m.channel, m.thread_of, m.sender.as_slice(), m.ts,
                m.encrypted, m.body, m.sig
            ],
        )?;
        for a in &m.attachments {
            tx.execute(
                "INSERT INTO message_attachments(message,hash,name,mime) VALUES (?,?,?,?)",
                params![m.id, a.hash, a.name, a.mime],
            )?;
        }
        Self::append(&tx, m.ts, &hex(&m.sender), "message.posted", &m.id, &serde_json::json!({"channel": m.channel}))?;
        tx.commit()?;
        Ok(seq)
    }

    /// Messages after `cursor` in a channel, oldest first. Only members may read.
    pub fn read_channel(
        &self,
        reader: &Pub,
        channel: &str,
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<MessageRow>, StoreError> {
        let member: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM channel_members WHERE channel=? AND pubkey=?)",
            params![channel, reader.as_slice()],
            |r| r.get(0),
        )?;
        if !member {
            return Err(StoreError::NotMember(channel.into()));
        }
        let mut st = self.conn.prepare_cached(
            "SELECT seq,id,sender,body,encrypted FROM messages WHERE channel=? AND seq>? ORDER BY seq LIMIT ?",
        )?;
        let rows = st.query_map(params![channel, cursor, limit], |r| {
            let s: Vec<u8> = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, s.try_into().unwrap_or([0; 32]), r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Unacknowledged messages for `reader` across all their channels, oldest
    /// first, never including their own. Delivery adapters call this to drain.
    pub fn inbox(&self, reader: &Pub, limit: i64) -> Result<Vec<(String, MessageRow)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT m.channel, m.seq, m.id, m.sender, m.body, m.encrypted
             FROM messages m
             JOIN channel_members cm ON cm.channel=m.channel AND cm.pubkey=?1
             LEFT JOIN cursors c ON c.channel=m.channel AND c.reader=?1
             WHERE m.seq > COALESCE(c.acked,0) AND m.sender != ?1
             ORDER BY m.seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![reader.as_slice(), limit], |r| {
            let s: Vec<u8> = r.get(3)?;
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?, s.try_into().unwrap_or([0; 32]), r.get(4)?, r.get(5)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Advance the reader's cursor for a channel; never moves backwards.
    pub fn ack(&mut self, reader: &Pub, channel: &str, seq: i64) -> Result<(), StoreError> {
        let member: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM channel_members WHERE channel=? AND pubkey=?)",
            params![channel, reader.as_slice()],
            |r| r.get(0),
        )?;
        if !member {
            return Err(StoreError::NotMember(channel.into()));
        }
        self.conn.execute(
            "INSERT INTO cursors(reader,channel,acked) VALUES (?,?,?)
             ON CONFLICT(reader,channel) DO UPDATE SET acked=MAX(acked, excluded.acked)",
            params![reader.as_slice(), channel, seq],
        )?;
        Ok(())
    }

    pub fn react(&mut self, r: &SignedReaction) -> Result<(), StoreError> {
        if r.emoji.is_empty() || r.emoji.len() > 64 {
            return Err(StoreError::Invalid("reaction must be 1..=64 bytes"));
        }
        verify(&r.by, &r.signing_bytes(), &r.sig)?;
        let exists: bool = match r.kind.as_str() {
            "message" => self.conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?)", [&r.target], |x| x.get(0))?,
            "file" => self.conn.query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?)", [&r.target], |x| x.get(0))?,
            "card" => self.conn.query_row("SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)", [&r.target], |x| x.get(0))?,
            _ => return Err(StoreError::Invalid("unknown reaction target kind")),
        };
        if !exists {
            return Err(StoreError::NotFound(r.target.clone()));
        }
        if r.remove {
            self.conn.execute(
                "DELETE FROM reactions WHERE kind=? AND target=? AND by=? AND emoji=?",
                params![r.kind, r.target, r.by.as_slice(), r.emoji],
            )?;
        } else {
            self.conn.execute(
                "INSERT OR REPLACE INTO reactions(kind,target,by,emoji,ts,sig) VALUES (?,?,?,?,?,?)",
                params![r.kind, r.target, r.by.as_slice(), r.emoji, r.ts, r.sig],
            )?;
        }
        Ok(())
    }

    pub fn reaction_counts(&self, kind: &str, target: &str) -> Result<Vec<(String, i64)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT emoji, COUNT(*) FROM reactions WHERE kind=? AND target=? GROUP BY emoji ORDER BY 2 DESC, 1",
        )?;
        let rows = st.query_map(params![kind, target], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn create_sprint(&mut self, id: &str, name: &str, goal: &str, starts: i64, ends: i64) -> Result<(), StoreError> {
        if ends <= starts {
            return Err(StoreError::Invalid("sprint must end after it starts"));
        }
        self.conn.execute(
            "INSERT INTO sprints(id,name,goal,starts,ends) VALUES (?,?,?,?,?)",
            params![id, name, goal, starts, ends],
        )?;
        Ok(())
    }

    pub fn add_to_sprint(&mut self, sprint: &str, card: &str) -> Result<(), StoreError> {
        self.card_state(card)?;
        let state: Option<String> = self
            .conn
            .query_row("SELECT state FROM sprints WHERE id=?", [sprint], |r| r.get(0))
            .optional()?;
        match state.as_deref() {
            None => return Err(StoreError::NotFound(sprint.into())),
            Some("closed") => return Err(StoreError::Invalid("sprint is closed")),
            _ => {}
        }
        self.conn.execute("INSERT OR IGNORE INTO sprint_cards(sprint,card) VALUES (?,?)", params![sprint, card])?;
        Ok(())
    }

    /// Only one sprint may be active at a time.
    pub fn start_sprint(&mut self, sprint: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sprints WHERE state='active')", [], |r| r.get(0))?;
        if active {
            return Err(StoreError::Invalid("another sprint is already active"));
        }
        let n = tx.execute("UPDATE sprints SET state='active' WHERE id=? AND state='planned'", [sprint])?;
        if n == 0 {
            return Err(StoreError::Invalid("sprint not found or not planned"));
        }
        Ok(tx.commit()?)
    }

    pub fn close_sprint(&mut self, sprint: &str) -> Result<(), StoreError> {
        let n = self.conn.execute("UPDATE sprints SET state='closed' WHERE id=? AND state='active'", [sprint])?;
        if n == 0 {
            return Err(StoreError::Invalid("sprint not found or not active"));
        }
        Ok(())
    }

    pub fn sprint_summary(&self, sprint: &str) -> Result<SprintSummary, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.state, COUNT(*) FROM sprint_cards s JOIN cards c ON c.id=s.card WHERE s.sprint=? GROUP BY c.state",
        )?;
        let mut out = SprintSummary::default();
        let rows = st.query_map([sprint], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (state, n) = row?;
            out.total += n;
            match state.as_str() {
                "done" => out.done += n,
                "blocked" => out.blocked += n,
                "claimed" | "doing" | "review" => out.in_flight += n,
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn publish_note(&mut self, n: &SignedNote) -> Result<(), StoreError> {
        verify(&n.author, &n.signing_bytes(), &n.sig)?;
        self.conn.execute(
            "INSERT INTO knowledge(id,author,ts,title,body,tags,supersedes,sig) VALUES (?,?,?,?,?,?,?,?)",
            params![n.id, n.author.as_slice(), n.ts, n.title, n.body, n.tags, n.supersedes, n.sig],
        )?;
        Ok(())
    }

    /// Full-text search over current (non-superseded) notes, best match first.
    pub fn search_notes(&self, query: &str, limit: i64) -> Result<Vec<(String, String)>, StoreError> {
        // Quote each term so user input can't inject FTS operators.
        let q = query
            .split_whitespace()
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
        let rows = st.query_map(params![q, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Supervision watchdog: claimed/doing cards untouched for `max_idle`.
    pub fn stale_cards(&self, now: i64, max_idle: i64) -> Result<Vec<(String, Option<String>)>, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, owner FROM cards WHERE state IN ('claimed','doing') AND updated < ? ORDER BY updated",
        )?;
        let rows = st.query_map([now - max_idle], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Return a stale claim to the pool, recording who reclaimed it.
    pub fn reclaim_stale(&mut self, now: i64, supervisor: &str, card: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let n = tx.execute(
            "UPDATE cards SET state='ready', owner=NULL, updated=? WHERE id=? AND state IN ('claimed','doing')",
            params![now, card],
        )?;
        if n == 0 {
            return Err(StoreError::Invalid("card is not claimed or in progress"));
        }
        Self::append(&tx, now, supervisor, "card.reclaimed", card, &serde_json::json!({}))?;
        Ok(tx.commit()?)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_crypto::ChannelKey;

    fn msg(id: &str, ch: &str) -> SignedMessage {
        SignedMessage {
            id: id.into(), channel: ch.into(), thread_of: None, sender: [0; 32], ts: 10,
            encrypted: false, body: b"hi".to_vec(), attachments: vec![], sig: vec![],
        }
    }

    fn setup() -> (Store, Identity, Identity) {
        let mut s = Store::open_in_memory().unwrap();
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        s.create_channel(1, "c", "group", "dev", None, &[(a.public(), "worker", None), (b.public(), "worker", None)]).unwrap();
        (s, a, b)
    }

    #[test]
    fn signed_message_round_trip_and_forgery_rejected() {
        let (mut s, a, b) = setup();
        assert_eq!(s.post_message(&msg("m1", "c").sign(&a)).unwrap(), 1);
        let rows = s.read_channel(&b.public(), "c", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, a.public());

        // Tamper after signing.
        let mut forged = msg("m2", "c").sign(&a);
        forged.body = b"evil".to_vec();
        assert!(matches!(s.post_message(&forged), Err(StoreError::Crypto(_))));
        // Claiming to be someone else.
        let mut spoof = msg("m3", "c").sign(&a);
        spoof.sender = b.public();
        assert!(s.post_message(&spoof).is_err());
    }

    #[test]
    fn inbox_drains_with_monotonic_acks() {
        let (mut s, a, b) = setup();
        let s1 = s.post_message(&msg("m1", "c").sign(&a)).unwrap();
        let s2 = s.post_message(&msg("m2", "c").sign(&a)).unwrap();
        s.post_message(&msg("m3", "c").sign(&b)).unwrap();
        // Own messages never appear: a sees only b's m3, b sees only a's m1/m2.
        let a_in = s.inbox(&a.public(), 10).unwrap();
        assert_eq!(a_in.iter().map(|(_, m)| m.1.as_str()).collect::<Vec<_>>(), ["m3"]);
        let got = s.inbox(&b.public(), 10).unwrap();
        assert_eq!(got.iter().map(|(_, m)| m.1.as_str()).collect::<Vec<_>>(), ["m1", "m2"]);
        s.ack(&b.public(), "c", s1).unwrap();
        assert_eq!(s.inbox(&b.public(), 10).unwrap().len(), 1);
        // An older ack cannot resurrect delivered messages.
        s.ack(&b.public(), "c", 0).unwrap();
        assert_eq!(s.inbox(&b.public(), 10).unwrap().len(), 1);
        s.ack(&b.public(), "c", s2).unwrap();
        assert!(s.inbox(&b.public(), 10).unwrap().is_empty());
        let eve = Identity::generate().unwrap();
        assert!(s.ack(&eve.public(), "c", 1).is_err());
        assert!(s.inbox(&eve.public(), 10).unwrap().is_empty());
    }

    #[test]
    fn non_members_cannot_post_or_read() {
        let (mut s, _, _) = setup();
        let eve = Identity::generate().unwrap();
        assert!(matches!(s.post_message(&msg("m", "c").sign(&eve)), Err(StoreError::NotMember(_))));
        assert!(matches!(s.read_channel(&eve.public(), "c", 0, 10), Err(StoreError::NotMember(_))));
    }

    #[test]
    fn threads_stay_in_their_channel() {
        let (mut s, a, _) = setup();
        s.create_channel(1, "d", "group", "", None, &[(a.public(), "worker", None)]).unwrap();
        s.post_message(&msg("root", "c").sign(&a)).unwrap();
        let mut ok = msg("reply", "c");
        ok.thread_of = Some("root".into());
        s.post_message(&ok.sign(&a)).unwrap();
        let mut cross = msg("bad", "d");
        cross.thread_of = Some("root".into());
        assert!(matches!(s.post_message(&cross.sign(&a)), Err(StoreError::Invalid(_))));
    }

    #[test]
    fn encrypted_message_end_to_end() {
        let (mut s, a, b) = setup();
        let key = ChannelKey::generate().unwrap();
        let wrapped = a.wrap_key(&b.public(), "c", &key).unwrap();
        let ct = key.seal(b"c|m1", b"top secret").unwrap();
        let mut m = msg("m1", "c");
        m.encrypted = true;
        m.body = ct;
        s.post_message(&m.sign(&a)).unwrap();
        let (_, _, sender, body, enc) = s.read_channel(&b.public(), "c", 0, 1).unwrap().remove(0);
        assert!(enc);
        let k = b.unwrap_key(&sender, "c", &wrapped).unwrap();
        assert_eq!(k.open(b"c|m1", &body).unwrap(), b"top secret");
        assert!(!body.windows(10).any(|w| w == b"top secret"));
    }

    #[test]
    fn attachments_are_content_addressed() {
        let (mut s, a, _) = setup();
        let h = s.put_blob(b"file bytes").unwrap();
        assert_eq!(h, content_hash(b"file bytes"));
        assert_eq!(s.get_blob(&h).unwrap().unwrap(), b"file bytes");
        let mut m = msg("m", "c");
        m.attachments = vec![Attachment { hash: h.clone(), name: "a.txt".into(), mime: "text/plain".into() }];
        s.post_message(&m.sign(&a)).unwrap();
        let mut missing = msg("m2", "c");
        missing.attachments = vec![Attachment { hash: "nope".into(), name: "x".into(), mime: "x".into() }];
        assert!(s.post_message(&missing.sign(&a)).is_err());
        assert!(s.put_blob(&vec![0; MAX_BLOB + 1]).is_err());
    }

    fn reaction(kind: &str, target: &str, emoji: &str, remove: bool) -> SignedReaction {
        SignedReaction { kind: kind.into(), target: target.into(), by: [0; 32], emoji: emoji.into(), remove, ts: 5, sig: vec![] }
    }

    #[test]
    fn reactions_on_messages_files_and_cards() {
        let (mut s, a, b) = setup();
        s.post_message(&msg("m", "c").sign(&a)).unwrap();
        let h = s.put_blob(b"f").unwrap();
        s.create_card(1, "sup", "card1", "t", "l").unwrap();
        s.react(&reaction("message", "m", "👍", false).sign(&a)).unwrap();
        s.react(&reaction("message", "m", "👍", false).sign(&b)).unwrap();
        s.react(&reaction("message", "m", "🎉", false).sign(&b)).unwrap();
        s.react(&reaction("file", &h, "👀", false).sign(&a)).unwrap();
        s.react(&reaction("card", "card1", "🔥", false).sign(&a)).unwrap();
        assert_eq!(s.reaction_counts("message", "m").unwrap(), vec![("👍".into(), 2), ("🎉".into(), 1)]);
        s.react(&reaction("message", "m", "👍", true).sign(&a)).unwrap();
        // Only alice's 👍 was removed; bob's remains.
        assert_eq!(s.reaction_counts("message", "m").unwrap(), vec![("🎉".to_string(), 1), ("👍".to_string(), 1)]);
    }

    #[test]
    fn reaction_rejects_bad_targets_and_signatures() {
        let (mut s, a, b) = setup();
        assert!(matches!(s.react(&reaction("message", "ghost", "👍", false).sign(&a)), Err(StoreError::NotFound(_))));
        assert!(s.react(&reaction("bogus", "x", "👍", false).sign(&a)).is_err());
        let mut r = reaction("card", "x", "👍", false).sign(&a);
        r.by = b.public();
        assert!(s.react(&r).is_err());
        assert!(s.react(&reaction("card", "x", "", false).sign(&a)).is_err());
    }

    #[test]
    fn sprint_lifecycle_and_summary() {
        let (mut s, _, _) = setup();
        s.create_card(1, "sup", "c1", "a", "l").unwrap();
        s.create_card(1, "sup", "c2", "b", "l").unwrap();
        s.create_sprint("s1", "Sprint 1", "ship", 0, 100).unwrap();
        assert!(s.create_sprint("bad", "x", "", 5, 5).is_err());
        s.add_to_sprint("s1", "c1").unwrap();
        s.add_to_sprint("s1", "c2").unwrap();
        s.start_sprint("s1").unwrap();
        s.create_sprint("s2", "Sprint 2", "", 0, 100).unwrap();
        assert!(s.start_sprint("s2").is_err());
        s.transition(2, "sup", agora_core::Role::Supervisor, "c1", agora_core::CardState::Ready, false).unwrap();
        s.transition(3, "w", agora_core::Role::Worker, "c1", agora_core::CardState::Claimed, false).unwrap();
        assert_eq!(s.sprint_summary("s1").unwrap(), SprintSummary { total: 2, done: 0, in_flight: 1, blocked: 0 });
        s.close_sprint("s1").unwrap();
        assert!(s.add_to_sprint("s1", "c1").is_err());
        s.start_sprint("s2").unwrap();
    }

    fn note(id: &str, title: &str, body: &str, sup: Option<&str>) -> SignedNote {
        SignedNote { id: id.into(), author: [0; 32], ts: 1, title: title.into(), body: body.into(), tags: "wine".into(), supersedes: sup.map(Into::into), sig: vec![] }
    }

    #[test]
    fn knowledge_search_hides_superseded_and_survives_hostile_queries() {
        let (mut s, a, _) = setup();
        s.publish_note(&note("k1", "Wine prefix", "use a clean prefix per build", None).sign(&a)).unwrap();
        assert_eq!(s.search_notes("prefix", 5).unwrap().len(), 1);
        s.publish_note(&note("k2", "Wine prefix v2", "prefix per worktree", Some("k1")).sign(&a)).unwrap();
        let hits = s.search_notes("prefix", 5).unwrap();
        assert_eq!(hits, vec![("k2".to_string(), "Wine prefix v2".to_string())]);
        assert!(s.search_notes("\" OR *", 5).is_ok());
        assert!(s.search_notes("   ", 5).unwrap().is_empty());
        let mut bad = note("k3", "t", "b", None).sign(&a);
        bad.body = "changed".into();
        assert!(s.publish_note(&bad).is_err());
    }

    #[test]
    fn watchdog_finds_and_reclaims_stale_claims() {
        let (mut s, _, _) = setup();
        s.create_card(0, "sup", "c1", "a", "l").unwrap();
        s.transition(1, "sup", agora_core::Role::Supervisor, "c1", agora_core::CardState::Ready, false).unwrap();
        s.transition(2, "w", agora_core::Role::Worker, "c1", agora_core::CardState::Claimed, false).unwrap();
        assert!(s.stale_cards(100, 1000).unwrap().is_empty());
        assert_eq!(s.stale_cards(2000, 1000).unwrap(), vec![("c1".to_string(), Some("w".to_string()))]);
        s.reclaim_stale(2001, "sup", "c1").unwrap();
        assert_eq!(s.card_state("c1").unwrap(), agora_core::CardState::Ready);
        assert!(s.reclaim_stale(2002, "sup", "c1").is_err());
    }
}
