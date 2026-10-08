//! Channels, membership, and key epochs. Owns the crate-internal channel helpers
//! that messages.rs and social.rs call.

use crate::identity::{Pub, active_role, authorize, hex, need, unhex32};
use crate::{SignedOp, Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_crypto::{canonical, content_hash};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};

const CHANNEL_KINDS: [&str; 5] = ["dm", "group", "thread", "squad", "team"];

/// Exact length of one wrapped channel key: a 24-byte XChaCha nonce, a 32-byte key and a
/// 16-byte Poly1305 tag.
pub const WRAPPED_KEY_LEN: usize = 72;

/// A channel as seen by one reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    /// "dm" | "group" | "thread" | "squad" | "team".
    pub kind: String,
    pub name: String,
    pub parent: Option<String>,
    /// Current key epoch (channels.epoch).
    pub epoch: i64,
    /// True iff any channel_keys row exists (sticky).
    pub encrypted: bool,
    /// True iff a channel_rotation row exists: posting is refused until an admin rotates.
    pub rotation_pending: bool,
    /// The reader's channel role ("admin" | "member"), None when the reader is a
    /// supervisor looking at a channel it is not in.
    pub my_role: Option<String>,
}

/// Digest that binds wrapped key material into a signed op's `arg` (field "keys").
/// Sort `keys` by public key bytes, then
/// `content_hash(&canonical("keys/v1", [epoch.to_le_bytes(), pk_0, wrapped_0, pk_1, wrapped_1, ...]))`
/// (64 lowercase hex). Order of the input slice does not matter.
pub fn keys_digest(epoch: i64, keys: &[(Pub, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(Pub, Vec<u8>)> = keys.iter().collect();
    sorted.sort();
    let epoch = epoch.to_le_bytes();
    let mut fields: Vec<&[u8]> = Vec::with_capacity(1 + 2 * sorted.len());
    fields.push(&epoch);
    for (pk, wrapped) in sorted {
        fields.push(pk);
        fields.push(wrapped);
    }
    content_hash(&canonical("keys/v1", &fields))
}

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
                // Membership first: an outsider gets the same answer for a missing parent
                // as for an existing one.
                if role < Role::Supervisor && !is_member(&tx, p, &op.actor)? {
                    return Err(StoreError::NotMember(p.clone()));
                }
                current_epoch(&tx, p)?;
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
        check_keys_field(&arg, encrypted.then(|| keys_digest(0, keys)))?;
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
            insert_key(&tx, &op.subject, 0, pk, wrapped, &op.actor)?;
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
                check_keys_field(&arg, Some(keys_digest(epoch, &[(target, w.clone())])))?;
                insert_key(&tx, channel, epoch, &target, &w, &op.actor)?
            }
            (true, _) => {
                return Err(StoreError::Invalid(
                    "encrypted channel needs a 72-byte wrapped key",
                ));
            }
            (false, None) => check_keys_field(&arg, None)?,
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

    /// kind "channel.remove", subject = channel id, arg JSON {"pk": hex64, "keys"?: digest}.
    /// Self-leave is open to every active role; removing someone else needs channel admin or
    /// Supervisor. A non-member gets NotMember(channel) without any existence probe, except
    /// Supervisor+, who get NotFound for a missing channel. Removing the last admin while
    /// other members remain is Invalid("cannot remove the last channel admin").
    /// Encrypted channel, removing someone else: arg.keys == keys_digest(current + 1, keys),
    /// keys covering the remaining members, epoch bumped, any pending rotation cleared.
    /// Encrypted channel, self-leave: no keys and no "keys" field (Invalid("self-leave takes
    /// no keys")), no epoch bump; a channel_rotation row ('left <hex>') is recorded, so
    /// posting is refused until a member rotates the key.
    /// Plain channel: no "keys" field and no keys.
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
        let arg = parse_arg(&op.arg)?;
        let target = parse_pk(&arg)?;
        // Authorize before probing the channel, so an outsider cannot learn which ids exist.
        let allowed = target == op.actor
            || role >= Role::Supervisor
            || (role >= Role::Worker && is_channel_admin(&tx, channel, &op.actor)?);
        if !allowed {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        if !is_member(&tx, channel, &target)? {
            if role >= Role::Supervisor {
                current_epoch(&tx, channel)?;
            }
            return Err(StoreError::NotMember(channel.into()));
        }
        let epoch = current_epoch(&tx, channel)?;
        if is_channel_admin(&tx, channel, &target)? {
            let (other_members, other_admins) = others_of(&tx, channel, &target)?;
            if other_admins == 0 && other_members > 0 {
                return Err(StoreError::Invalid("cannot remove the last channel admin"));
            }
        }
        let encrypted = is_encrypted(&tx, channel)?;
        let self_leave = target == op.actor;
        // A leaver must not author the next epoch's key, so a self-leave carries no keys.
        if encrypted && self_leave && (!keys.is_empty() || arg.get("keys").is_some()) {
            return Err(StoreError::Invalid("self-leave takes no keys"));
        }
        tx.execute(
            "DELETE FROM channel_members WHERE channel=? AND pubkey=?",
            params![channel, target.as_slice()],
        )?;
        let new_epoch = if !encrypted {
            if !keys.is_empty() {
                return Err(StoreError::Invalid("unencrypted channel takes no keys"));
            }
            check_keys_field(&arg, None)?;
            epoch
        } else if self_leave {
            // No epoch bump: the remaining members must channel.rotate before anyone posts.
            let reason = format!("left {}", hex(&target));
            tx.execute(
                "INSERT OR IGNORE INTO channel_rotation(channel, since, reason) VALUES (?,?,?)",
                params![channel, now, reason],
            )?;
            epoch
        } else {
            // Validate against the members that remain, after the delete above.
            let remaining = member_set(&tx, channel)?;
            if key_set(keys)? != remaining {
                return Err(StoreError::Invalid(
                    "keys must cover exactly the remaining members",
                ));
            }
            let new_epoch = epoch + 1;
            check_keys_field(&arg, Some(keys_digest(new_epoch, keys)))?;
            tx.execute(
                "UPDATE channels SET epoch=? WHERE id=?",
                params![new_epoch, channel],
            )?;
            for (pk, wrapped) in keys {
                insert_key(&tx, channel, new_epoch, pk, wrapped, &op.actor)?;
            }
            // A full rotation settles any rotation that was pending.
            tx.execute("DELETE FROM channel_rotation WHERE channel=?", [channel])?;
            new_epoch
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

    /// kind "channel.role", subject = channel id, arg JSON {"pk": hex64, "role": "admin"|"member"}.
    /// Need Worker plus channel admin, or Supervisor (else Forbidden; check before probing
    /// existence). Target must be a member (else NotMember(channel)). Demoting the last
    /// admin -> Invalid("cannot demote the last channel admin"). Unchanged role -> Ok
    /// with no event (nonce still consumed). Event "channel.role_changed" {"member","role"}.
    pub fn set_channel_role(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "channel.role")?;
        need(role, Role::Worker)?;
        let channel = op.subject.as_str();
        let arg = parse_arg(&op.arg)?;
        let (target, new_role) = parse_member(&arg)?;
        // Authorize before probing the channel, so an outsider cannot learn which ids exist.
        if role < Role::Supervisor && !is_channel_admin(&tx, channel, &op.actor)? {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        current_epoch(&tx, channel)?;
        let old_role: Option<String> = tx
            .query_row(
                "SELECT role FROM channel_members WHERE channel=? AND pubkey=?",
                params![channel, target.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        let old_role = old_role.ok_or_else(|| StoreError::NotMember(channel.into()))?;
        if old_role == new_role {
            return Ok(tx.commit()?);
        }
        if old_role == "admin" && others_of(&tx, channel, &target)?.1 == 0 {
            return Err(StoreError::Invalid("cannot demote the last channel admin"));
        }
        tx.execute(
            "UPDATE channel_members SET role=? WHERE channel=? AND pubkey=?",
            params![new_role, channel, target.as_slice()],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "channel.role_changed",
            &op.subject,
            &serde_json::json!({"member": hex(&target), "role": new_role}),
        )?;
        Ok(tx.commit()?)
    }

    /// kind "channel.rotate", subject = channel id, arg JSON {"keys": digest}.
    /// Need Worker plus channel admin, or Supervisor (Forbidden first). Channel must be
    /// encrypted (Invalid("channel is not encrypted")). New epoch = current + 1; `keys`
    /// must cover exactly the current members (key_set rules) and arg.keys must equal
    /// keys_digest(new_epoch, keys) (Invalid("keys digest mismatch")). Inserts the keys
    /// (wrapper = op.actor), bumps channels.epoch, deletes the channel_rotation row.
    /// Event "channel.rotated" {"epoch": new}. Returns the new epoch.
    pub fn rotate_channel_key(
        &mut self,
        now: i64,
        op: &SignedOp,
        keys: &[(Pub, Vec<u8>)],
    ) -> Result<i64, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "channel.rotate")?;
        need(role, Role::Worker)?;
        let channel = op.subject.as_str();
        let arg = parse_arg(&op.arg)?;
        // Authorize before probing the channel, so an outsider cannot learn which ids exist.
        if role < Role::Supervisor && !is_channel_admin(&tx, channel, &op.actor)? {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        let epoch = current_epoch(&tx, channel)?;
        if !is_encrypted(&tx, channel)? {
            return Err(StoreError::Invalid("channel is not encrypted"));
        }
        let new_epoch = epoch + 1;
        let members = member_set(&tx, channel)?;
        if key_set(keys)? != members {
            return Err(StoreError::Invalid(
                "keys must cover exactly the member set",
            ));
        }
        check_keys_field(&arg, Some(keys_digest(new_epoch, keys)))?;
        tx.execute(
            "UPDATE channels SET epoch=? WHERE id=?",
            params![new_epoch, channel],
        )?;
        for (pk, wrapped) in keys {
            insert_key(&tx, channel, new_epoch, pk, wrapped, &op.actor)?;
        }
        tx.execute("DELETE FROM channel_rotation WHERE channel=?", [channel])?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "channel.rotated",
            &op.subject,
            &serde_json::json!({"epoch": new_epoch}),
        )?;
        tx.commit()?;
        Ok(new_epoch)
    }

    /// Channels `reader` is a member of, ordered by id. Reader must be active
    /// (active_role), else that error.
    pub fn list_channels(&self, reader: &Pub) -> Result<Vec<ChannelInfo>, StoreError> {
        active_role(&self.conn, reader)?;
        let mut stmt = self.conn.prepare(&format!(
            "{INFO_SELECT} WHERE c.id IN (SELECT channel FROM channel_members WHERE pubkey=?1) ORDER BY c.id"
        ))?;
        let rows = stmt.query_map([reader.as_slice()], info_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Info plus members (pubkey, role) sorted by pubkey. Reader must be active.
    /// Members see their channel. A Supervisor+ that is not a member sees it with
    /// my_role None (NotFound if the channel is missing). Anyone else gets
    /// NotMember(channel) whether or not the channel exists (no existence leak).
    pub fn channel_info(
        &self,
        reader: &Pub,
        channel: &str,
    ) -> Result<(ChannelInfo, Vec<(Pub, String)>), StoreError> {
        // One read snapshot, so the membership check and the reads agree.
        let tx = self.conn.unchecked_transaction()?;
        let reader_role = active_role(&tx, reader)?;
        if !is_member(&tx, channel, reader)? {
            if reader_role < Role::Supervisor {
                return Err(StoreError::NotMember(channel.into()));
            }
            current_epoch(&tx, channel)?;
        }
        let info = tx.query_row(
            &format!("{INFO_SELECT} WHERE c.id=?2"),
            params![reader.as_slice(), channel],
            info_row,
        )?;
        let members = self.channel_members(channel)?;
        Ok((info, members))
    }

    /// The reader's own wrapped key: (epoch, wrapper pubkey, wrapped bytes) for `epoch`
    /// (None = current epoch). Reader must be active and a current member
    /// (NotMember(channel) otherwise, also for a missing channel). No row, or a pre-M2
    /// row with NULL wrapper -> NotFound("channel key").
    pub fn member_key(
        &self,
        reader: &Pub,
        channel: &str,
        epoch: Option<i64>,
    ) -> Result<(i64, Pub, Vec<u8>), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        active_role(&tx, reader)?;
        if !is_member(&tx, channel, reader)? {
            return Err(StoreError::NotMember(channel.into()));
        }
        let epoch = match epoch {
            Some(e) => e,
            None => current_epoch(&tx, channel)?,
        };
        let row = tx
            .query_row(
                "SELECT wrapper, wrapped FROM channel_keys WHERE channel=? AND epoch=? AND member=?",
                params![channel, epoch, reader.as_slice()],
                |r| Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let (wrapper, wrapped) = match row {
            Some((Some(wrapper), wrapped)) => (wrapper, wrapped),
            // A pre-M2 row has no recorded wrapper, so it cannot be unwrapped.
            _ => return Err(StoreError::NotFound("channel key".into())),
        };
        let wrapper = Pub::try_from(wrapper.as_slice()).map_err(|_| {
            StoreError::Sql(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                "wrapper is not 32 bytes".into(),
            ))
        })?;
        Ok((epoch, wrapper, wrapped))
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
    let pk = parse_pk(v)?;
    let role = match v.get("role").and_then(serde_json::Value::as_str) {
        Some("admin") => "admin",
        Some("member") => "member",
        _ => return Err(StoreError::Invalid("member role must be admin or member")),
    };
    Ok((pk, role))
}

/// Reads the `"pk"` field: 64 lowercase hex.
fn parse_pk(v: &serde_json::Value) -> Result<Pub, StoreError> {
    v.get("pk")
        .and_then(serde_json::Value::as_str)
        .and_then(unhex32)
        .ok_or(StoreError::Invalid("member pk must be 64 lowercase hex"))
}

fn parse_arg(arg: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(arg).map_err(|_| StoreError::Invalid("arg must be JSON"))
}

/// Enforces the signed `"keys"` field. `expected` Some(digest): the arg must carry exactly
/// that digest. None: the arg must not carry the field at all.
fn check_keys_field(arg: &serde_json::Value, expected: Option<String>) -> Result<(), StoreError> {
    match (expected, arg.get("keys")) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::Invalid("keys digest without keys")),
        (Some(digest), Some(v)) if v.as_str() == Some(digest.as_str()) => Ok(()),
        (Some(_), _) => Err(StoreError::Invalid("keys digest mismatch")),
    }
}

/// (other members, other admins) of `channel`, not counting `pk`.
fn others_of(conn: &Connection, channel: &str, pk: &Pub) -> Result<(i64, i64), StoreError> {
    let (members, admins): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(role='admin'), 0) FROM channel_members \
         WHERE channel=? AND pubkey<>?",
        params![channel, pk.as_slice()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((members, admins))
}

/// Shared projection for `ChannelInfo` reads. Parameter ?1 is the reader's pubkey; the
/// callers add their own WHERE clause (?2 is the channel id where one is needed).
const INFO_SELECT: &str = "SELECT c.id, c.kind, c.name, c.parent, c.epoch,
    EXISTS(SELECT 1 FROM channel_keys k WHERE k.channel=c.id),
    (SELECT m.role FROM channel_members m WHERE m.channel=c.id AND m.pubkey=?1),
    EXISTS(SELECT 1 FROM channel_rotation r WHERE r.channel=c.id)
    FROM channels c";

fn info_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelInfo> {
    Ok(ChannelInfo {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        parent: r.get(3)?,
        epoch: r.get(4)?,
        encrypted: r.get(5)?,
        my_role: r.get(6)?,
        rotation_pending: r.get(7)?,
    })
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

/// Records `wrapper` (the op actor that sealed the key) alongside the wrapped bytes.
fn insert_key(
    conn: &Connection,
    channel: &str,
    epoch: i64,
    member: &Pub,
    wrapped: &[u8],
    wrapper: &Pub,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO channel_keys(channel,epoch,member,wrapped,wrapper) VALUES (?,?,?,?,?)",
        params![
            channel,
            epoch,
            member.as_slice(),
            wrapped,
            wrapper.as_slice()
        ],
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

/// True iff the channel must rotate its key before anyone may post (channel_rotation row).
pub(crate) fn rotation_pending(conn: &Connection, channel: &str) -> Result<bool, StoreError> {
    let found: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_rotation WHERE channel=?)",
        [channel],
        |r| r.get(0),
    )?;
    Ok(found)
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

#[cfg(test)]
mod tests {
    use super::*;
    use agora_crypto::Identity;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    const NOW: i64 = 1_700_000_000_000;
    static NONCES: AtomicU64 = AtomicU64::new(0);

    fn ident(seed: u8) -> Identity {
        Identity::from_seed([seed; 32])
    }

    fn pk(seed: u8) -> Pub {
        ident(seed).public()
    }

    /// An operation signed at NOW with a nonce no earlier call in this process used.
    fn op(id: &Identity, kind: &str, subject: &str, arg: &str) -> SignedOp {
        let nonce = format!("n{}", NONCES.fetch_add(1, Ordering::Relaxed));
        SignedOp::new(kind, subject, arg, NOW, &nonce).sign(id)
    }

    /// Owner (seed 1) bootstrapped; supervisor (2) granted by the owner; alice (3),
    /// bob (4) and carol (7) registered with the default worker role.
    fn fixture() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.bootstrap_owner(NOW, &pk(1)).unwrap();
        for seed in [2, 3, 4, 7] {
            s.register_identity(NOW, &pk(seed), "").unwrap();
        }
        s.grant_role(
            NOW,
            &op(&ident(1), "role.grant", &hex(&pk(2)), "supervisor"),
        )
        .unwrap();
        s
    }

    /// Creates group `id` as `actor`, with `others` as plain members. An encrypted channel
    /// wraps a key for every member, the creator included; each wrapped key is `[seed; 72]`.
    fn create(s: &mut Store, actor: u8, id: &str, others: &[u8], encrypt: bool) {
        let members: Vec<_> = others
            .iter()
            .map(|m| json!({"pk": hex(&pk(*m)), "role": "member"}))
            .collect();
        let keys: Vec<(Pub, Vec<u8>)> = if encrypt {
            std::iter::once(actor)
                .chain(others.iter().copied())
                .map(|m| (pk(m), vec![m; WRAPPED_KEY_LEN]))
                .collect()
        } else {
            vec![]
        };
        let mut arg = json!({"kind": "group", "members": members});
        if encrypt {
            arg["keys"] = json!(keys_digest(0, &keys));
        }
        s.create_channel(
            NOW,
            &op(&ident(actor), "channel.create", id, &arg.to_string()),
            &keys,
        )
        .unwrap();
    }

    #[test]
    fn keys_digest_ignores_the_order_of_the_keys() {
        let a = (pk(3), vec![1u8; WRAPPED_KEY_LEN]);
        let b = (pk(4), vec![2u8; WRAPPED_KEY_LEN]);
        assert_eq!(
            keys_digest(0, &[a.clone(), b.clone()]),
            keys_digest(0, &[b, a])
        );
    }

    #[test]
    fn keys_digest_depends_on_the_epoch() {
        let keys = [(pk(3), vec![1u8; WRAPPED_KEY_LEN])];
        assert_ne!(keys_digest(0, &keys), keys_digest(1, &keys));
    }

    #[test]
    fn keys_digest_changes_when_any_wrapped_byte_changes() {
        let keys = vec![
            (pk(3), vec![1u8; WRAPPED_KEY_LEN]),
            (pk(4), vec![2u8; WRAPPED_KEY_LEN]),
        ];
        let base = keys_digest(0, &keys);
        for entry in 0..keys.len() {
            for byte in 0..WRAPPED_KEY_LEN {
                let mut changed = keys.clone();
                changed[entry].1[byte] ^= 0x01;
                assert_ne!(keys_digest(0, &changed), base, "entry {entry} byte {byte}");
            }
        }
    }

    #[test]
    fn set_channel_role_promotes_a_member() {
        let mut s = fixture();
        create(&mut s, 3, "c", &[4], false);
        let arg = json!({"pk": hex(&pk(4)), "role": "admin"}).to_string();
        s.set_channel_role(NOW, &op(&ident(3), "channel.role", "c", &arg))
            .unwrap();
        assert!(
            s.channel_members("c")
                .unwrap()
                .contains(&(pk(4), "admin".to_string()))
        );
    }

    #[test]
    fn set_channel_role_refuses_to_demote_the_last_admin() {
        let mut s = fixture();
        create(&mut s, 3, "c", &[4], false);
        let arg = json!({"pk": hex(&pk(3)), "role": "member"}).to_string();
        let res = s.set_channel_role(NOW, &op(&ident(3), "channel.role", "c", &arg));
        assert!(matches!(res, Err(StoreError::Invalid(_))));
        assert!(
            s.channel_members("c")
                .unwrap()
                .contains(&(pk(3), "admin".to_string()))
        );
    }

    #[test]
    fn rotate_channel_key_bumps_the_epoch_and_clears_a_pending_rotation() {
        let mut s = fixture();
        create(&mut s, 3, "e", &[4], true);
        s.conn
            .execute(
                "INSERT INTO channel_rotation(channel, since, reason) VALUES ('e', ?, 'test')",
                [NOW],
            )
            .unwrap();
        assert!(rotation_pending(&s.conn, "e").unwrap());
        let keys = vec![
            (pk(3), vec![5u8; WRAPPED_KEY_LEN]),
            (pk(4), vec![6u8; WRAPPED_KEY_LEN]),
        ];
        let arg = json!({"keys": keys_digest(1, &keys)}).to_string();
        let epoch = s
            .rotate_channel_key(NOW, &op(&ident(3), "channel.rotate", "e", &arg), &keys)
            .unwrap();
        assert_eq!(epoch, 1);
        assert_eq!(s.channel_epoch("e").unwrap(), 1);
        assert_eq!(
            s.wrapped_key("e", &pk(4), 1).unwrap(),
            Some(vec![6u8; WRAPPED_KEY_LEN])
        );
        assert!(!rotation_pending(&s.conn, "e").unwrap());
    }

    #[test]
    fn list_channels_returns_only_my_channels_in_id_order() {
        let mut s = fixture();
        create(&mut s, 3, "b", &[], false);
        create(&mut s, 3, "a", &[4], false);
        let mine = s.list_channels(&pk(3)).unwrap();
        let ids: Vec<&str> = mine.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(mine.iter().all(|c| c.my_role.as_deref() == Some("admin")));
        let bobs = s.list_channels(&pk(4)).unwrap();
        assert_eq!(bobs.len(), 1);
        assert_eq!(bobs[0].id, "a");
        assert_eq!(bobs[0].my_role.as_deref(), Some("member"));
        assert!(s.list_channels(&pk(7)).unwrap().is_empty());
    }

    #[test]
    fn list_channels_rejects_an_unknown_reader() {
        let s = fixture();
        assert!(matches!(
            s.list_channels(&pk(9)),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn channel_info_gives_outsiders_the_same_error_for_missing_and_existing_channels() {
        let mut s = fixture();
        create(&mut s, 3, "p", &[], false);
        assert!(matches!(
            s.channel_info(&pk(4), "p"),
            Err(StoreError::NotMember(_))
        ));
        assert!(matches!(
            s.channel_info(&pk(4), "missing"),
            Err(StoreError::NotMember(_))
        ));
    }

    #[test]
    fn channel_info_shows_a_supervisor_a_channel_it_is_not_in() {
        let mut s = fixture();
        create(&mut s, 3, "p", &[4], false);
        let (info, members) = s.channel_info(&pk(2), "p").unwrap();
        assert_eq!(info.my_role, None);
        assert_eq!(members.len(), 2);
        assert!(matches!(
            s.channel_info(&pk(2), "missing"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn member_key_returns_my_wrapped_key_and_its_wrapper() {
        let mut s = fixture();
        create(&mut s, 3, "e", &[4], true);
        assert_eq!(
            s.member_key(&pk(4), "e", None).unwrap(),
            (0, pk(3), vec![4u8; WRAPPED_KEY_LEN])
        );
        assert!(matches!(
            s.member_key(&pk(7), "e", None),
            Err(StoreError::NotMember(_))
        ));
    }

    #[test]
    fn member_key_reports_a_pre_m2_row_without_a_wrapper_as_not_found() {
        let mut s = fixture();
        create(&mut s, 3, "e", &[4], true);
        s.conn
            .execute("UPDATE channel_keys SET wrapper=NULL", [])
            .unwrap();
        assert!(matches!(
            s.member_key(&pk(4), "e", None),
            Err(StoreError::NotFound(_))
        ));
    }
}
