//! Blobs, signed messages, channel reads, the inbox, and read cursors.
//! Every message is verified here before it reaches the database.

use crate::channels::{current_epoch, is_encrypted, is_member};
use crate::identity::{Pub, active_role, check_clock, hex, need};
use crate::{Store, StoreError};
use agora_core::Role;
use agora_crypto::{Identity, canonical, content_hash, verify};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

/// Upper bound on rows returned by one `read_channel` or `inbox` call.
const MAX_PAGE: i64 = 500;

/// Largest blob accepted (clients encrypt before upload when needed).
pub const MAX_BLOB: usize = 16 * 1024 * 1024;
pub const MAX_BODY: usize = 256 * 1024;

/// Upper bound on message, channel and thread ids, in bytes.
const MAX_ID_LEN: usize = 256;
/// Upper bound on attachments per message.
const MAX_ATTACHMENTS: usize = 32;
/// Upper bound on an attachment's name and mime type, in bytes.
const MAX_ATTACHMENT_FIELD: usize = 255;
/// Shortest encrypted body: a 24-byte nonce plus a 16-byte tag.
const MIN_ENCRYPTED_BODY: usize = 24 + 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub hash: String,
    pub name: String,
    pub mime: String,
}

#[derive(Debug, Clone)]
pub struct SignedMessage {
    pub id: String,
    pub channel: String,
    pub thread_of: Option<String>,
    pub sender: Pub,
    pub ts: i64,
    pub encrypted: bool,
    pub epoch: i64,
    /// Plaintext, or nonce||ciphertext when `encrypted`.
    pub body: Vec<u8>,
    pub attachments: Vec<Attachment>,
    pub sig: Vec<u8>,
}

impl SignedMessage {
    fn signing_bytes(&self) -> Vec<u8> {
        let ts = self.ts.to_le_bytes();
        let enc = [self.encrypted as u8];
        let epoch = self.epoch.to_le_bytes();
        let att: Vec<u8> = self
            .attachments
            .iter()
            .flat_map(|a| {
                canonical(
                    "att",
                    &[a.hash.as_bytes(), a.name.as_bytes(), a.mime.as_bytes()],
                )
            })
            .collect();
        canonical(
            "msg/v2",
            &[
                self.id.as_bytes(),
                self.channel.as_bytes(),
                self.thread_of.as_deref().unwrap_or("").as_bytes(),
                &self.sender,
                &ts,
                &enc,
                &epoch,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub seq: i64,
    pub id: String,
    pub sender: Pub,
    pub body: Vec<u8>,
    pub encrypted: bool,
    pub epoch: i64,
}

impl Store {
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
    pub fn post_message(&mut self, now: i64, m: &SignedMessage) -> Result<i64, StoreError> {
        if !(1..=MAX_ID_LEN).contains(&m.id.len()) {
            return Err(StoreError::Invalid("message id must be 1 to 256 bytes"));
        }
        if m.channel.len() > MAX_ID_LEN {
            return Err(StoreError::Invalid("channel id too long"));
        }
        if m.thread_of.as_ref().is_some_and(|t| t.len() > MAX_ID_LEN) {
            return Err(StoreError::Invalid("thread id too long"));
        }
        if m.attachments.len() > MAX_ATTACHMENTS {
            return Err(StoreError::Invalid("too many attachments"));
        }
        let mut hashes = HashSet::new();
        for a in &m.attachments {
            if a.hash.len() != 64
                || !a
                    .hash
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(StoreError::Invalid(
                    "attachment hash must be 64 lowercase hex chars",
                ));
            }
            if a.name.len() > MAX_ATTACHMENT_FIELD {
                return Err(StoreError::Invalid("attachment name too long"));
            }
            if a.mime.len() > MAX_ATTACHMENT_FIELD {
                return Err(StoreError::Invalid("attachment mime too long"));
            }
            if !hashes.insert(a.hash.as_str()) {
                return Err(StoreError::Invalid("duplicate attachment in message"));
            }
        }
        if m.encrypted && m.body.len() < MIN_ENCRYPTED_BODY {
            return Err(StoreError::Invalid("encrypted body too short"));
        }
        if m.body.len() > MAX_BODY {
            return Err(StoreError::Invalid("body too large"));
        }
        check_clock(now, m.ts)?;
        verify(&m.sender, &m.signing_bytes(), &m.sig)?;

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        need(active_role(&tx, &m.sender)?, Role::Worker)?;
        if !is_member(&tx, &m.channel, &m.sender)? {
            return Err(StoreError::NotMember(m.channel.clone()));
        }
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?)",
            params![m.id],
            |r| r.get(0),
        )?;
        if duplicate {
            return Err(StoreError::Invalid("message id already exists"));
        }
        let epoch = current_epoch(&tx, &m.channel)?;
        if is_encrypted(&tx, &m.channel)? {
            if !m.encrypted {
                return Err(StoreError::Invalid(
                    "plaintext message in encrypted channel",
                ));
            }
            if m.epoch != epoch {
                return Err(StoreError::Invalid("stale or future key epoch"));
            }
        } else {
            if m.encrypted {
                return Err(StoreError::Invalid(
                    "encrypted message in unencrypted channel",
                ));
            }
            if m.epoch != 0 {
                return Err(StoreError::Invalid(
                    "epoch must be 0 in unencrypted channel",
                ));
            }
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
            let known: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?)",
                params![a.hash],
                |r| r.get(0),
            )?;
            if !known {
                return Err(StoreError::Invalid("attachment blob missing"));
            }
        }

        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq),0)+1 FROM messages", [], |r| {
            r.get(0)
        })?;
        tx.execute(
            "INSERT INTO messages(id,seq,channel,thread_of,sender,ts,encrypted,epoch,body,sig)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                m.id,
                seq,
                m.channel,
                m.thread_of,
                m.sender.as_slice(),
                m.ts,
                m.encrypted,
                m.epoch,
                m.body,
                m.sig
            ],
        )?;
        for a in &m.attachments {
            tx.execute(
                "INSERT INTO message_attachments(message,hash,name,mime) VALUES (?,?,?,?)",
                params![m.id, a.hash, a.name, a.mime],
            )?;
        }
        Self::append(
            &tx,
            now,
            &hex(&m.sender),
            "message.posted",
            &m.id,
            &serde_json::json!({"channel": m.channel, "seq": seq}),
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// Messages with seq > cursor in channel, oldest first, at most `limit`. Members only.
    pub fn read_channel(
        &self,
        reader: &Pub,
        channel: &str,
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<MessageRow>, StoreError> {
        active_role(&self.conn, reader)?;
        if !is_member(&self.conn, channel, reader)? {
            return Err(StoreError::NotMember(channel.into()));
        }
        let mut st = self.conn.prepare_cached(
            "SELECT seq,id,sender,body,encrypted,epoch FROM messages
             WHERE channel=? AND seq>? ORDER BY seq LIMIT ?",
        )?;
        let rows = st.query_map(params![channel, cursor, limit.clamp(0, MAX_PAGE)], |r| {
            message_row(r, 0)
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Unacked messages across reader's channels, oldest first, excluding reader's own.
    pub fn inbox(&self, reader: &Pub, limit: i64) -> Result<Vec<(String, MessageRow)>, StoreError> {
        active_role(&self.conn, reader)?;
        let mut st = self.conn.prepare_cached(
            "SELECT m.channel, m.seq, m.id, m.sender, m.body, m.encrypted, m.epoch
             FROM messages m
             JOIN channel_members cm ON cm.channel=m.channel AND cm.pubkey=?1
             LEFT JOIN cursors c ON c.channel=m.channel AND c.reader=?1
             WHERE m.seq > COALESCE(c.acked,0) AND m.sender != ?1
             ORDER BY m.seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![reader.as_slice(), limit.clamp(0, MAX_PAGE)], |r| {
            Ok((r.get::<_, String>(0)?, message_row(r, 1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Advance reader's cursor for channel; never moves backwards. Members only.
    pub fn ack(&mut self, reader: &Pub, channel: &str, seq: i64) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        active_role(&tx, reader)?;
        if !is_member(&tx, channel, reader)? {
            return Err(StoreError::NotMember(channel.into()));
        }
        let head: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE channel=?",
            params![channel],
            |r| r.get(0),
        )?;
        let seq = seq.clamp(0, head);
        tx.execute(
            "INSERT INTO cursors(reader,channel,acked) VALUES (?,?,?)
             ON CONFLICT(reader,channel) DO UPDATE SET acked=MAX(acked, excluded.acked)",
            params![reader.as_slice(), channel, seq],
        )?;
        Ok(tx.commit()?)
    }
}

/// Reads the six message columns starting at `at`: seq, id, sender, body, encrypted, epoch.
/// A sender that is not exactly 32 bytes is a conversion error, never a fabricated key.
fn message_row(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<MessageRow> {
    let sender_bytes: Vec<u8> = r.get(at + 2)?;
    let sender: Pub = sender_bytes.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            at + 2,
            rusqlite::types::Type::Blob,
            "sender must be 32 bytes".into(),
        )
    })?;
    Ok(MessageRow {
        seq: r.get(at)?,
        id: r.get(at + 1)?,
        sender,
        body: r.get(at + 3)?,
        encrypted: r.get(at + 4)?,
        epoch: r.get(at + 5)?,
    })
}
