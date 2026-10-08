//! Channels, membership, and key epochs. Owns the crate-internal channel helpers
//! that messages.rs and social.rs call.

use crate::identity::{Pub, active_role, authorize, hex, need, unhex32};
use crate::{SignedOp, Store, StoreError};
use agora_core::{Role, TransitionError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};

const CHANNEL_KINDS: [&str; 5] = ["dm", "group", "thread", "squad", "team"];

/// Exact length of one wrapped channel key: a 24-byte XChaCha nonce, a 32-byte key and a
/// 16-byte Poly1305 tag.
pub const WRAPPED_KEY_LEN: usize = 72;

impl Store {
    /// kind "channel.create", subject = channel id; need Worker.
    pub fn create_channel(
        &mut self,
        now: i64,
        op: &SignedOp,
        keys: &[(Pub, Vec<u8>)],
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "channel.create")?;
        need(role, Role::Worker)?;
        if op.subject.is_empty() {
            return Err(StoreError::Invalid("channel id must not be empty"));
        }
        let arg = parse_arg(&op.arg)?;
        let kind = arg
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .filter(|k| CHANNEL_KINDS.contains(k))
            .ok_or(StoreError::Invalid(
                "channel kind must be dm, group, thread, squad or team",
            ))?;
        let name = match arg.get("name") {
            None => "",
            Some(v) => v
                .as_str()
                .ok_or(StoreError::Invalid("channel name must be a string"))?,
        };
        let parent = match arg.get("parent") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(p)) => {
                current_epoch(&tx, p)?;
                if role < Role::Supervisor && !is_member(&tx, p, &op.actor)? {
                    return Err(StoreError::NotMember(p.clone()));
                }
                Some(p.as_str())
            }
            Some(_) => return Err(StoreError::Invalid("parent must be null or a channel id")),
        };

        let mut members: BTreeMap<Pub, &'static str> = BTreeMap::new();
        match arg.get("members") {
            None => {}
            Some(serde_json::Value::Array(list)) => {
                for m in list {
                    let (pk, member_role) = parse_member(m)?;
                    active_role(&tx, &pk)?;
                    if members.insert(pk, member_role).is_some() {
                        return Err(StoreError::Invalid("duplicate member"));
                    }
                }
            }
            Some(_) => return Err(StoreError::Invalid("members must be an array")),
        }
        // The creator is always a member and always an admin, listed or not.
        members.insert(op.actor, "admin");

        let encrypted = !keys.is_empty();
        let member_pks: BTreeSet<Pub> = members.keys().copied().collect();
        if encrypted && key_set(keys)? != member_pks {
            return Err(StoreError::Invalid(
                "keys must cover exactly the member set",
            ));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM channels WHERE id=?)",
            [&op.subject],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid("channel already exists"));
        }

        tx.execute(
            "INSERT INTO channels(id,kind,name,parent,epoch,created) VALUES (?,?,?,?,0,?)",
            params![op.subject, kind, name, parent, now],
        )?;
        for (pk, member_role) in &members {
            tx.execute(
                "INSERT INTO channel_members(channel,pubkey,role) VALUES (?,?,?)",
                params![op.subject, pk.as_slice(), member_role],
            )?;
        }
        for (pk, wrapped) in keys {
            insert_key(&tx, &op.subject, 0, pk, wrapped)?;
        }
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "channel.created",
            &op.subject,
            &serde_json::json!({
                "kind": kind,
                "name": name,
                "parent": parent,
                "members": members.len(),
                "encrypted": encrypted,
            }),
        )?;
        Ok(tx.commit()?)
    }

    /// kind "channel.add", subject = channel id; need Worker plus channel admin or Supervisor.
    pub fn add_member(
        &mut self,
        now: i64,
        op: &SignedOp,
        wrapped: Option<Vec<u8>>,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "channel.add")?;
        need(role, Role::Worker)?;
        let channel = op.subject.as_str();
        let arg = parse_arg(&op.arg)?;
        let (target, member_role) = parse_member(&arg)?;
        // Authorize before probing the channel, so an outsider cannot learn which ids exist.
        if role < Role::Supervisor && !is_channel_admin(&tx, channel, &op.actor)? {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        let epoch = current_epoch(&tx, channel)?;
        active_role(&tx, &target)?;
        if is_member(&tx, channel, &target)? {
            return Err(StoreError::Invalid("already a member"));
        }
        match (is_encrypted(&tx, channel)?, wrapped) {
            (true, Some(w)) if w.len() == WRAPPED_KEY_LEN => {
                insert_key(&tx, channel, epoch, &target, &w)?
            }
            (true, _) => {
                return Err(StoreError::Invalid(
                    "encrypted channel needs a 72-byte wrapped key",
                ));
            }
            (false, None) => {}
            (false, Some(_)) => {
                return Err(StoreError::Invalid(
                    "unencrypted channel takes no wrapped key",
                ));
            }
        }
        tx.execute(
            "INSERT INTO channel_members(channel,pubkey,role) VALUES (?,?,?)",
            params![channel, target.as_slice(), member_role],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "channel.member_added",
            &op.subject,
            &serde_json::json!({"member": hex(&target), "role": member_role, "epoch": epoch}),
        )?;
        Ok(tx.commit()?)
    }

    /// kind "channel.remove", subject = channel id, arg = hex64 of the member.
    pub fn remove_member(
        &mut self,
        now: i64,
        op: &SignedOp,
        keys: &[(Pub, Vec<u8>)],
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "channel.remove")?;
        let channel = op.subject.as_str();
        let target =
            unhex32(&op.arg).ok_or(StoreError::Invalid("member must be 64 lowercase hex"))?;
        // Self-leave is open to every active role; removing someone else needs admin rights.
        // Authorize before probing the channel, so an outsider cannot learn which ids exist.
        let allowed = target == op.actor
            || role >= Role::Supervisor
            || (role >= Role::Worker && is_channel_admin(&tx, channel, &op.actor)?);
        if !allowed {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        current_epoch(&tx, channel)?;
        if !is_member(&tx, channel, &target)? {
            return Err(StoreError::NotMember(channel.into()));
        }
        tx.execute(
            "DELETE FROM channel_members WHERE channel=? AND pubkey=?",
            params![channel, target.as_slice()],
        )?;
        let new_epoch = if is_encrypted(&tx, channel)? {
            // Validate against the members that remain, after the delete above.
            let remaining = member_set(&tx, channel)?;
            if key_set(keys)? != remaining {
                return Err(StoreError::Invalid(
                    "keys must cover exactly the remaining members",
                ));
            }
            tx.execute(
                "UPDATE channels SET epoch = epoch + 1 WHERE id=?",
                [channel],
            )?;
            let new_epoch = current_epoch(&tx, channel)?;
            for (pk, wrapped) in keys {
                insert_key(&tx, channel, new_epoch, pk, wrapped)?;
            }
            new_epoch
        } else {
            if !keys.is_empty() {
                return Err(StoreError::Invalid("unencrypted channel takes no keys"));
            }
            current_epoch(&tx, channel)?
        };
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "channel.member_removed",
            &op.subject,
            &serde_json::json!({"member": hex(&target), "epoch": new_epoch}),
        )?;
        Ok(tx.commit()?)
    }

    /// Wrapped key of `member` for `epoch`, or None.
    pub fn wrapped_key(
        &self,
        channel: &str,
        member: &Pub,
        epoch: i64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let wrapped = self
            .conn
            .query_row(
                "SELECT wrapped FROM channel_keys WHERE channel=? AND epoch=? AND member=?",
                params![channel, epoch, member.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(wrapped)
    }

    /// Current epoch (NotFound if channel missing).
    pub fn channel_epoch(&self, channel: &str) -> Result<i64, StoreError> {
        current_epoch(&self.conn, channel)
    }

    /// Members as (pubkey, role) sorted by pubkey bytes (NotFound if channel missing).
    pub fn channel_members(&self, channel: &str) -> Result<Vec<(Pub, String)>, StoreError> {
        current_epoch(&self.conn, channel)?;
        let mut stmt = self
            .conn
            .prepare("SELECT pubkey, role FROM channel_members WHERE channel=? ORDER BY pubkey")?;
        let rows = stmt.query_map([channel], |r| Ok((read_pub(r, 0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Reads `{"pk": hex64, "role": "admin" | "member"}`.
fn parse_member(v: &serde_json::Value) -> Result<(Pub, &'static str), StoreError> {
    let pk = v
        .get("pk")
        .and_then(serde_json::Value::as_str)
        .and_then(unhex32)
        .ok_or(StoreError::Invalid("member pk must be 64 lowercase hex"))?;
    let role = match v.get("role").and_then(serde_json::Value::as_str) {
        Some("admin") => "admin",
        Some("member") => "member",
        _ => return Err(StoreError::Invalid("member role must be admin or member")),
    };
    Ok((pk, role))
}

fn parse_arg(arg: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(arg).map_err(|_| StoreError::Invalid("arg must be JSON"))
}

/// The pubkeys a key set covers. Rejects a key listed twice and a wrapped key of the wrong length.
fn key_set(keys: &[(Pub, Vec<u8>)]) -> Result<BTreeSet<Pub>, StoreError> {
    let mut set = BTreeSet::new();
    for (pk, wrapped) in keys {
        if wrapped.len() != WRAPPED_KEY_LEN {
            return Err(StoreError::Invalid("wrapped key must be 72 bytes"));
        }
        if !set.insert(*pk) {
            return Err(StoreError::Invalid("duplicate key pubkey"));
        }
    }
    Ok(set)
}

fn member_set(conn: &Connection, channel: &str) -> Result<BTreeSet<Pub>, StoreError> {
    let mut stmt = conn.prepare("SELECT pubkey FROM channel_members WHERE channel=?")?;
    let pks = stmt.query_map([channel], |r| read_pub(r, 0))?;
    Ok(pks.collect::<rusqlite::Result<BTreeSet<Pub>>>()?)
}

fn insert_key(
    conn: &Connection,
    channel: &str,
    epoch: i64,
    member: &Pub,
    wrapped: &[u8],
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO channel_keys(channel,epoch,member,wrapped) VALUES (?,?,?,?)",
        params![channel, epoch, member.as_slice(), wrapped],
    )?;
    Ok(())
}

/// A pubkey column must hold exactly 32 bytes; anything else is a corrupt row, not a zero key.
fn read_pub(row: &rusqlite::Row, idx: usize) -> rusqlite::Result<Pub> {
    let bytes: Vec<u8> = row.get(idx)?;
    Pub::try_from(bytes.as_slice()).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "pubkey is not 32 bytes".into(),
        )
    })
}

pub(crate) fn current_epoch(conn: &Connection, channel: &str) -> Result<i64, StoreError> {
    conn.query_row("SELECT epoch FROM channels WHERE id=?", [channel], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| StoreError::NotFound(channel.into()))
}

pub(crate) fn is_encrypted(conn: &Connection, channel: &str) -> Result<bool, StoreError> {
    let found: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_keys WHERE channel=?)",
        [channel],
        |r| r.get(0),
    )?;
    Ok(found)
}

pub(crate) fn is_member(conn: &Connection, channel: &str, pk: &Pub) -> Result<bool, StoreError> {
    let found: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_members WHERE channel=? AND pubkey=?)",
        params![channel, pk.as_slice()],
        |r| r.get(0),
    )?;
    Ok(found)
}

pub(crate) fn is_channel_admin(
    conn: &Connection,
    channel: &str,
    pk: &Pub,
) -> Result<bool, StoreError> {
    let found: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_members WHERE channel=? AND pubkey=? AND role='admin')",
        params![channel, pk.as_slice()],
        |r| r.get(0),
    )?;
    Ok(found)
}
