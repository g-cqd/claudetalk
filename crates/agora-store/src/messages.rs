//! Blobs, signed messages, channel reads, the inbox, and read cursors.
//! Every message is verified here before it reaches the database.

use crate::channels::{current_epoch, is_encrypted, is_member, rotation_pending};
use crate::identity::{Pub, active_role, check_clock, hex, need};
use crate::{Store, StoreError};
use agora_core::Role;
use agora_crypto::{Identity, canonical, content_hash, random_bytes, verify};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Upper bound on rows returned by one `read_channel` or `inbox` call.
const MAX_PAGE: i64 = 500;

/// Largest blob accepted (clients encrypt before upload when needed).
pub const MAX_BLOB: usize = 16 * 1024 * 1024;
pub const MAX_BODY: usize = 256 * 1024;
/// Blobs strictly larger than this spill to files when the store has a blob dir.
pub const SPILL_THRESHOLD: usize = 1024 * 1024;

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

    /// Check `sig` against `sender` over the canonical bytes (no database, no clock).
    pub fn verify(&self) -> Result<(), StoreError> {
        Ok(verify(&self.sender, &self.signing_bytes(), &self.sig)?)
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
    pub channel: String,
    pub thread_of: Option<String>,
    pub ts: i64,
    /// In insertion order (rowid of message_attachments).
    pub attachments: Vec<Attachment>,
    pub sig: Vec<u8>,
}

impl MessageRow {
    /// The signed message this row was stored from, so a client can re-verify it.
    pub fn to_signed(&self) -> SignedMessage {
        SignedMessage {
            id: self.id.clone(),
            channel: self.channel.clone(),
            thread_of: self.thread_of.clone(),
            sender: self.sender,
            ts: self.ts,
            encrypted: self.encrypted,
            epoch: self.epoch,
            body: self.body.clone(),
            attachments: self.attachments.clone(),
            sig: self.sig.clone(),
        }
    }
}

impl Store {
    /// Store a blob; the hash is always recomputed, never trusted.
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<String, StoreError> {
        if bytes.len() > MAX_BLOB {
            return Err(StoreError::Invalid("blob too large"));
        }
        let hash = content_hash(bytes);
        let spilled = ensure_blob_file(&self.conn, self.blob_dir.as_deref(), &hash, bytes)?;
        insert_blob_row(&self.conn, &hash, bytes, spilled)?;
        Ok(hash)
    }

    /// Daemon upload path. `uploader` must be an active identity with role >= Worker.
    /// `bytes.len() > max_len.min(MAX_BLOB)` -> Invalid("blob too large"). If this uploader
    /// already has a blob_uploads row for the hash, nothing is charged. Otherwise
    /// blob_usage(uploader) + len > quota -> Invalid("blob quota exceeded"), else the blob
    /// is stored as by put_blob and a blob_uploads row is inserted. Returns the hash.
    pub fn put_blob_for(
        &mut self,
        now: i64,
        uploader: &Pub,
        bytes: &[u8],
        max_len: usize,
        quota: i64,
    ) -> Result<String, StoreError> {
        if bytes.len() > max_len.min(MAX_BLOB) {
            return Err(StoreError::Invalid("blob too large"));
        }
        // Plain reads first, so a refused upload hashes nothing and writes no file.
        need(active_role(&self.conn, uploader)?, Role::Worker)?;
        let hash = content_hash(bytes);
        let size = i64::try_from(bytes.len()).map_err(|_| StoreError::Invalid("blob too large"))?;
        if !charged_to(&self.conn, uploader, &hash)?
            && usage_of(&self.conn, uploader)?.saturating_add(size) > quota
        {
            return Err(StoreError::Invalid("blob quota exceeded"));
        }
        // The file is written before the transaction. It is content-addressed, so a rollback
        // below leaves at most an unreferenced file, never a row without its bytes.
        let spilled = ensure_blob_file(&self.conn, self.blob_dir.as_deref(), &hash, bytes)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        need(active_role(&tx, uploader)?, Role::Worker)?;
        if !charged_to(&tx, uploader, &hash)? {
            let used = usage_of(&tx, uploader)?;
            if used.saturating_add(size) > quota {
                return Err(StoreError::Invalid("blob quota exceeded"));
            }
            insert_blob_row(&tx, &hash, bytes, spilled)?;
            tx.execute(
                "INSERT INTO blob_uploads(uploader,hash,size,ts) VALUES (?,?,?,?)",
                params![uploader.as_slice(), hash, size, now],
            )?;
        }
        tx.commit()?;
        Ok(hash)
    }

    /// Sum of sizes in blob_uploads for `uploader` (0 when none).
    pub fn blob_usage(&self, uploader: &Pub) -> Result<i64, StoreError> {
        usage_of(&self.conn, uploader)
    }

    /// The bytes stored under `hash`, re-hashed before they are returned. A spilled blob is
    /// read from its file; a missing file is `Invalid("blob file missing")`, and bytes that
    /// do not hash to `hash` are `Invalid("blob file corrupt")`, never returned.
    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let row: Option<(Vec<u8>, bool)> = self
            .conn
            .query_row(
                "SELECT bytes, spilled FROM blobs WHERE hash=?",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((inline, spilled)) = row else {
            return Ok(None);
        };
        let bytes = if spilled {
            let dir = self
                .blob_dir
                .as_deref()
                .ok_or(StoreError::Invalid("blob file missing"))?;
            match std::fs::read(spill_dir(dir, hash)?.join(hash)) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(StoreError::Invalid("blob file missing"));
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            inline
        };
        if content_hash(&bytes) != hash {
            return Err(StoreError::Invalid("blob file corrupt"));
        }
        Ok(Some(bytes))
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
        if rotation_pending(&tx, &m.channel)? {
            return Err(StoreError::Invalid("channel key rotation pending"));
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
            "SELECT seq,id,sender,body,encrypted,epoch,channel,thread_of,ts,sig FROM messages
             WHERE channel=? AND seq>? ORDER BY seq LIMIT ?",
        )?;
        let rows = st.query_map(params![channel, cursor, limit.clamp(0, MAX_PAGE)], |r| {
            message_row(r, 0)
        })?;
        let mut out: Vec<MessageRow> = rows.collect::<Result<_, _>>()?;
        for row in &mut out {
            row.attachments = attachments_of(&self.conn, &row.id)?;
        }
        Ok(out)
    }

    /// Supervisor or above: messages of any channel, member or not. Encrypted
    /// bodies are returned as stored (ciphertext); the caller decrypts only when
    /// it holds the channel key. Same ordering and page cap as `read_channel`.
    pub fn read_channel_any(
        &self,
        reader: &Pub,
        channel: &str,
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<MessageRow>, StoreError> {
        let role = active_role(&self.conn, reader)?;
        need(role, Role::Supervisor)?;
        let mut st = self.conn.prepare_cached(
            "SELECT seq,id,sender,body,encrypted,epoch,channel,thread_of,ts,sig FROM messages
             WHERE channel=? AND seq>? ORDER BY seq LIMIT ?",
        )?;
        let rows = st.query_map(params![channel, cursor, limit.clamp(0, MAX_PAGE)], |r| {
            message_row(r, 0)
        })?;
        let mut out: Vec<MessageRow> = rows.collect::<Result<_, _>>()?;
        for row in &mut out {
            row.attachments = attachments_of(&self.conn, &row.id)?;
        }
        Ok(out)
    }

    /// Unacked messages across reader's channels, oldest first, excluding reader's own.
    pub fn inbox(&self, reader: &Pub, limit: i64) -> Result<Vec<(String, MessageRow)>, StoreError> {
        // Every stored seq is at least 1, so `seq > 0` leaves the inbox unchanged.
        self.inbox_after(reader, 0, limit)
    }

    /// Same rows and filters as `inbox`, restricted to seq > `after` (seq is global,
    /// so one cursor spans all channels). Oldest first, limit clamped to 0..=MAX_PAGE.
    pub fn inbox_after(
        &self,
        reader: &Pub,
        after: i64,
        limit: i64,
    ) -> Result<Vec<(String, MessageRow)>, StoreError> {
        active_role(&self.conn, reader)?;
        let mut st = self.conn.prepare_cached(
            "SELECT m.seq, m.id, m.sender, m.body, m.encrypted, m.epoch, m.channel, m.thread_of, m.ts, m.sig
             FROM messages m
             JOIN channel_members cm ON cm.channel=m.channel AND cm.pubkey=?1
             LEFT JOIN cursors c ON c.channel=m.channel AND c.reader=?1
             WHERE m.seq > COALESCE(c.acked,0) AND m.seq > ?2 AND m.sender != ?1
             ORDER BY m.seq LIMIT ?3",
        )?;
        let rows = st.query_map(
            params![reader.as_slice(), after, limit.clamp(0, MAX_PAGE)],
            |r| {
                let row = message_row(r, 0)?;
                Ok((row.channel.clone(), row))
            },
        )?;
        let mut out: Vec<(String, MessageRow)> = rows.collect::<Result<_, _>>()?;
        for (_, row) in &mut out {
            row.attachments = attachments_of(&self.conn, &row.id)?;
        }
        Ok(out)
    }

    /// Advance reader's cursor for channel; never moves backwards. Members only. Returns the
    /// stored cursor after the advance, which is above `seq` when reader had acked further.
    pub fn ack(&mut self, reader: &Pub, channel: &str, seq: i64) -> Result<i64, StoreError> {
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
        let effective: i64 = tx.query_row(
            "SELECT acked FROM cursors WHERE reader=? AND channel=?",
            params![reader.as_slice(), channel],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(effective)
    }
}

/// Reads the ten message columns starting at `at`: seq, id, sender, body, encrypted, epoch,
/// channel, thread_of, ts, sig. Attachments are not in the row; callers fill them with
/// `attachments_of`. A sender that is not exactly 32 bytes is a conversion error, never a
/// fabricated key.
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
        channel: r.get(at + 6)?,
        thread_of: r.get(at + 7)?,
        ts: r.get(at + 8)?,
        attachments: vec![],
        sig: r.get(at + 9)?,
    })
}

/// The attachments of one message, in insertion (rowid) order.
fn attachments_of(conn: &Connection, message: &str) -> Result<Vec<Attachment>, StoreError> {
    let mut st = conn.prepare_cached(
        "SELECT hash,name,mime FROM message_attachments WHERE message=? ORDER BY rowid",
    )?;
    let rows = st.query_map([message], |r| {
        Ok(Attachment {
            hash: r.get(0)?,
            name: r.get(1)?,
            mime: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Sum of sizes charged to `uploader` in blob_uploads (0 when none).
fn usage_of(conn: &Connection, uploader: &Pub) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(size),0) FROM blob_uploads WHERE uploader=?",
        [uploader.as_slice()],
        |r| r.get(0),
    )?)
}

/// True when `uploader` already has a blob_uploads row for `hash`, so nothing is charged again.
fn charged_to(conn: &Connection, uploader: &Pub, hash: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM blob_uploads WHERE uploader=? AND hash=?)",
        params![uploader.as_slice(), hash],
        |r| r.get(0),
    )?)
}

/// The file half of storing the blob `hash` (computed from `bytes` by the caller). Runs
/// before any row is written, and returns whether the blob is stored spilled.
/// - A row that already exists inline is left alone: no file is written.
/// - A row that already exists spilled gets its file back only when the file is missing or
///   does not hash to `hash` (`spill_bytes` checks that).
/// - A new blob over `SPILL_THRESHOLD` is spilled when the store has a blob dir.
/// - Anything else is stored inline.
fn ensure_blob_file(
    conn: &Connection,
    blob_dir: Option<&Path>,
    hash: &str,
    bytes: &[u8],
) -> Result<bool, StoreError> {
    let existing: Option<bool> = conn
        .query_row("SELECT spilled FROM blobs WHERE hash=?", [hash], |r| {
            r.get(0)
        })
        .optional()?;
    match (existing, blob_dir) {
        (Some(false), _) => Ok(false),
        // Spilled into another store's blob dir, which this store cannot reach. The row
        // stands, and `get_blob` reports a missing file if it is gone.
        (Some(true), None) => Ok(true),
        (Some(true), Some(dir)) => {
            spill_bytes(dir, hash, bytes)?;
            Ok(true)
        }
        (None, Some(dir)) if bytes.len() > SPILL_THRESHOLD => {
            spill_bytes(dir, hash, bytes)?;
            Ok(true)
        }
        (None, _) => Ok(false),
    }
}

/// The row half of storing a blob: inserts it unless the hash already has a row. The bytes
/// are inline unless `spilled`, in which case the file written by `ensure_blob_file` holds them.
fn insert_blob_row(
    conn: &Connection,
    hash: &str,
    bytes: &[u8],
    spilled: bool,
) -> Result<(), StoreError> {
    let size = i64::try_from(bytes.len()).map_err(|_| StoreError::Invalid("blob too large"))?;
    if spilled {
        conn.execute(
            "INSERT OR IGNORE INTO blobs(hash,size,bytes,spilled) VALUES (?,?,x'',1)",
            params![hash, size],
        )?;
    } else {
        conn.execute(
            "INSERT OR IGNORE INTO blobs(hash,size,bytes) VALUES (?,?,?)",
            params![hash, size, bytes],
        )?;
    }
    Ok(())
}

/// `<dir>/<hash[0..2]>`, the directory that holds a spilled blob. Created with mode 0700
/// by `spill_bytes`. Rejects anything but a 64-char lowercase hex hash, so a hash taken
/// from a client can never name a path outside `dir`.
fn spill_dir(dir: &Path, hash: &str) -> Result<PathBuf, StoreError> {
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(StoreError::Invalid("blob hash malformed"));
    }
    let prefix = hash
        .get(..2)
        .ok_or(StoreError::Invalid("blob hash malformed"))?;
    Ok(dir.join(prefix))
}

/// Writes `bytes` to `<dir>/<hash[0..2]>/<hash>`. Skipped when that file already holds
/// bytes that hash to `hash`; otherwise written to a temp file in the same directory,
/// synced, and renamed over the final name, so readers never see a partial file.
fn spill_bytes(dir: &Path, hash: &str, bytes: &[u8]) -> Result<(), StoreError> {
    let sub = spill_dir(dir, hash)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&sub)?;
    let path = sub.join(hash);
    if std::fs::read(&path).is_ok_and(|existing| content_hash(&existing) == hash) {
        return Ok(());
    }
    let tmp = sub.join(format!(".{hash}.{}.tmp", hex(&random_bytes::<8>()?)));
    // The directory fsync makes the rename itself durable, not just the file's bytes.
    let written = write_synced(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .and_then(|()| std::fs::File::open(&sub)?.sync_all());
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Creates `path` exclusively with mode 0600, writes `bytes` and syncs them to disk.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the system temp dir, removed when the test ends.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("agora-messages-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self(path)
        }

        fn open_store(&self) -> Store {
            Store::open_with_blob_dir(&self.0.join("db.sqlite"), &self.0.join("blobs"))
                .expect("open store")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn signed(id: &Identity) -> SignedMessage {
        SignedMessage {
            id: "m1".into(),
            channel: "general".into(),
            thread_of: None,
            sender: [0; 32],
            ts: 1_000,
            encrypted: false,
            epoch: 0,
            body: b"hello".to_vec(),
            attachments: vec![Attachment {
                hash: "a".repeat(64),
                name: "log.txt".into(),
                mime: "text/plain".into(),
            }],
            sig: vec![],
        }
        .sign(id)
    }

    #[test]
    fn signed_message_verifies_after_a_row_round_trip() {
        let id = Identity::from_seed([7; 32]);
        let msg = signed(&id);
        msg.verify().expect("fresh signature verifies");
        let row = MessageRow {
            seq: 1,
            id: msg.id.clone(),
            sender: msg.sender,
            body: msg.body.clone(),
            encrypted: msg.encrypted,
            epoch: msg.epoch,
            channel: msg.channel.clone(),
            thread_of: msg.thread_of.clone(),
            ts: msg.ts,
            attachments: msg.attachments.clone(),
            sig: msg.sig.clone(),
        };
        let back = row.to_signed();
        back.verify().expect("round-tripped signature verifies");
        assert_eq!(back.signing_bytes(), msg.signing_bytes());
        assert_eq!(back.sender, id.public());
    }

    #[test]
    fn tampered_message_fails_verification() {
        let id = Identity::from_seed([7; 32]);
        let mut msg = signed(&id);
        msg.body = b"goodbye".to_vec();
        assert!(matches!(msg.verify(), Err(StoreError::Crypto(_))));
    }

    #[test]
    fn spilled_blob_is_written_under_its_hash_prefix_and_reads_back() {
        let dir = ScratchDir::new("spill");
        let mut store = dir.open_store();
        let bytes = vec![0xab; SPILL_THRESHOLD + 1];
        let hash = store.put_blob(&bytes).expect("put blob");
        let path = dir.0.join("blobs").join(&hash[..2]).join(&hash);
        assert_eq!(std::fs::read(&path).expect("spill file"), bytes);
        let (inline_len, spilled): (i64, bool) = store
            .conn
            .query_row(
                "SELECT length(bytes), spilled FROM blobs WHERE hash=?",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("blob row");
        assert_eq!((inline_len, spilled), (0, true));
        assert_eq!(store.get_blob(&hash).expect("get blob"), Some(bytes));
    }

    #[test]
    fn corrupt_spill_file_is_refused() {
        let dir = ScratchDir::new("corrupt");
        let mut store = dir.open_store();
        let hash = store
            .put_blob(&vec![0x11; SPILL_THRESHOLD + 1])
            .expect("put blob");
        let path = dir.0.join("blobs").join(&hash[..2]).join(&hash);
        std::fs::write(&path, b"tampered").expect("overwrite spill file");
        assert!(matches!(
            store.get_blob(&hash),
            Err(StoreError::Invalid("blob file corrupt"))
        ));
    }

    #[test]
    fn put_blob_for_charges_each_blob_once_and_enforces_the_quota() {
        let dir = ScratchDir::new("quota");
        let mut store = dir.open_store();
        let uploader: Pub = [9; 32];
        store
            .conn
            .execute(
                "INSERT INTO identities(pubkey, first_seen) VALUES (?, 0)",
                [uploader.as_slice()],
            )
            .expect("register uploader");
        let ten = b"0123456789";
        let first = store
            .put_blob_for(1_000, &uploader, ten, 100, 10)
            .expect("first upload fits the quota");
        assert_eq!(store.blob_usage(&uploader).expect("usage"), 10);
        // The same bytes again are already charged to this uploader.
        assert_eq!(
            store
                .put_blob_for(1_000, &uploader, ten, 100, 10)
                .expect("dedupe"),
            first
        );
        assert_eq!(store.blob_usage(&uploader).expect("usage"), 10);
        // A new blob would take the uploader past the quota.
        assert!(matches!(
            store.put_blob_for(1_000, &uploader, b"x", 100, 10),
            Err(StoreError::Invalid("blob quota exceeded"))
        ));
        assert_eq!(store.blob_usage(&uploader).expect("usage"), 10);
        // Over the per-upload cap is refused before anything is charged.
        assert!(matches!(
            store.put_blob_for(1_000, &uploader, ten, 5, 100),
            Err(StoreError::Invalid("blob too large"))
        ));
    }

    #[test]
    fn re_putting_an_inline_blob_writes_no_file_even_with_a_blob_dir() {
        let dir = ScratchDir::new("inline-no-file");
        let mut store = Store::open_in_memory().expect("open store");
        let bytes = vec![0x22; SPILL_THRESHOLD + 1];
        let hash = store.put_blob(&bytes).expect("inline put");
        store.blob_dir = Some(dir.0.join("blobs"));
        assert_eq!(store.put_blob(&bytes).expect("second put"), hash);
        assert!(
            !dir.0.join("blobs").exists(),
            "no spill dir for an inline row"
        );
        assert_eq!(store.get_blob(&hash).expect("get blob"), Some(bytes));
    }

    #[test]
    fn re_putting_a_spilled_blob_repairs_a_missing_or_corrupt_file() {
        let dir = ScratchDir::new("repair");
        let mut store = dir.open_store();
        let bytes = vec![0x33; SPILL_THRESHOLD + 1];
        let hash = store.put_blob(&bytes).expect("put blob");
        let path = dir.0.join("blobs").join(&hash[..2]).join(&hash);

        std::fs::remove_file(&path).expect("remove spill file");
        assert_eq!(store.put_blob(&bytes).expect("re-put"), hash);
        assert_eq!(
            store.get_blob(&hash).expect("get after repair"),
            Some(bytes.clone())
        );

        std::fs::write(&path, b"tampered").expect("corrupt spill file");
        assert_eq!(store.put_blob(&bytes).expect("re-put"), hash);
        assert_eq!(
            store.get_blob(&hash).expect("get after repair"),
            Some(bytes)
        );
    }

    #[test]
    fn spill_dir_accepts_only_64_char_lowercase_hex() {
        let dir = Path::new("/blobs");
        let good = "ab".repeat(32);
        assert_eq!(spill_dir(dir, &good).expect("valid hash"), dir.join("ab"));
        let traversal = format!("../{}", "a".repeat(61));
        for bad in [
            String::new(),
            "ab".into(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            traversal,
        ] {
            assert!(
                matches!(
                    spill_dir(dir, &bad),
                    Err(StoreError::Invalid("blob hash malformed"))
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn ack_returns_the_stored_cursor_after_the_merge() {
        let mut store = Store::open_in_memory().expect("open store");
        let reader: Pub = [5; 32];
        store
            .conn
            .execute(
                "INSERT INTO identities(pubkey, first_seen) VALUES (?, 0)",
                [reader.as_slice()],
            )
            .expect("register reader");
        store
            .conn
            .execute_batch(
                "INSERT INTO channels(id, kind, name, epoch, created) VALUES ('c', 'group', '', 0, 0);",
            )
            .expect("channel");
        store
            .conn
            .execute(
                "INSERT INTO channel_members(channel, pubkey, role) VALUES ('c', ?, 'member')",
                [reader.as_slice()],
            )
            .expect("membership");
        for seq in 1..=5_i64 {
            store
                .conn
                .execute(
                    "INSERT INTO messages(id, seq, channel, sender, ts, encrypted, body, sig)
                     VALUES (?, ?, 'c', ?, 0, 0, x'', x'')",
                    params![format!("m{seq}"), seq, [9_u8; 32].as_slice()],
                )
                .expect("message");
        }
        assert_eq!(store.ack(&reader, "c", 3).expect("ack 3"), 3);
        assert_eq!(store.ack(&reader, "c", 1).expect("ack 1 after 3"), 3);
        assert_eq!(store.ack(&reader, "c", 99).expect("ack past head"), 5);
    }
}
