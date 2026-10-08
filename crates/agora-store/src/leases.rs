//! Pools, path leases, pool leases and quiet windows.
//!
//! A lease is active while `released` is NULL and `expires` is NULL or later than `now`. Path and pool leases
//! expire; a quiet window has no expiry and closes with `quiet.close`, or with the reconciler after its
//! `expected_end + quiet_max`. Every read takes `now`, so expiry is judged at the caller's instant.

mod helpers;

use crate::identity::{authorize, need};
use crate::work_ops::checks::path_depth;
use crate::{SignedOp, Store, StoreError};
use agora_core::{Role, TransitionError};
use agora_sched::claims::{canonical_path, validate_name};
use agora_sched::model::{
    LeaseArgs, LeaseKind, LeaseRecord, LeaseRenewArgs, PoolArgs, PoolRecord, QuietArgs,
};
use agora_sched::pyfmt::printable;
use helpers::{
    LEASE_COLS, POOL_SQL, check_lease_room, find_pool, is_active, lease_by_id, lease_from_row,
    lease_ttl, next_lease_id, optional_json, path_holder, pool_from_row, settings,
};
use rusqlite::params;

impl Store {
    /// "pool.define" (Controller+); redefining updates the capacity.
    pub fn pool_define(&mut self, now: i64, op: &SignedOp) -> Result<PoolRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "pool.define")?;
        need(role, Role::Controller)?;
        let args: PoolArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("pool arg must be PoolArgs JSON"))?;
        validate_name("pool", &args.name).map_err(StoreError::Refused)?;
        if op.subject != args.name {
            return Err(StoreError::Invalid("pool subject must be the pool name"));
        }
        if !(args.capacity.is_finite() && args.capacity > 0.0) {
            return Err(StoreError::Invalid(
                "pool capacity must be a finite number above zero",
            ));
        }
        let me = op.actor_hex();
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pools WHERE name=?)",
            [args.name.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            tx.execute(
                "UPDATE pools SET capacity=?, unit=?, note=? WHERE name=?",
                params![args.capacity, args.unit, args.note, args.name],
            )?;
        } else {
            tx.execute(
                "INSERT INTO pools(name, capacity, unit, note, created, created_by) VALUES (?,?,?,?,?,?)",
                params![args.name, args.capacity, args.unit, args.note, now, me],
            )?;
        }
        Self::append(
            &tx,
            now,
            &me,
            "pool.defined",
            &args.name,
            &serde_json::json!({"capacity": args.capacity, "unit": args.unit}),
        )?;
        let pool = find_pool(&tx, now, &args.name)?
            .ok_or(StoreError::Invalid("pool vanished after definition"))?;
        tx.commit()?;
        Ok(pool)
    }

    /// "lease.acquire". Path: canonical, at least two components unless the actor is Supervisor+ (`Refused("<p> is too
    /// broad to claim")`), conflicts with other holders' active path leases and claimed/running task
    /// files -> `Invalid("path <p> is held by <who> (<what>) until <iso|-> ")`. Pool: used + amount <= capacity else
    /// `Invalid("pool <p> has <free> of <cap> <unit> free")`. expires = now + ttl (default settings.lease_ttl_s);
    /// a ttl above seven days -> `Invalid("a lease lasts at most 7 days")`. A holder with 64 active path and pool
    /// leases -> `Refused("<holder> already holds 64 active leases")`.
    pub fn lease_acquire(&mut self, now: i64, op: &SignedOp) -> Result<LeaseRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "lease.acquire")?;
        need(role, Role::Worker)?;
        let args: LeaseArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("lease arg must be LeaseArgs JSON"))?;
        let me = op.actor_hex();
        let (resource, amount) = match args.kind {
            LeaseKind::Quiet => return Err(StoreError::Invalid("use quiet.open")),
            LeaseKind::Path => {
                if !args.resource.starts_with('/') {
                    return Err(StoreError::Invalid("path must be absolute"));
                }
                let canon =
                    canonical_path(&args.resource, "/", None).map_err(StoreError::Refused)?;
                // S-8: "/" and "/x" are too broad for anyone below Supervisor to hold.
                if role < Role::Supervisor && path_depth(&canon) < 2 {
                    return Err(StoreError::Refused(format!(
                        "{} is too broad to claim",
                        printable(&canon)
                    )));
                }
                if let Some(message) = path_holder(&tx, now, &me, &canon)? {
                    return Err(StoreError::Refused(message));
                }
                (canon, 0.0)
            }
            LeaseKind::Pool => {
                if !(args.amount.is_finite() && args.amount > 0.0) {
                    return Err(StoreError::Invalid(
                        "lease amount must be a finite number above zero",
                    ));
                }
                let pool = find_pool(&tx, now, &args.resource)?.ok_or_else(|| {
                    StoreError::NotFound(format!("pool {}", printable(&args.resource)))
                })?;
                if pool.used + args.amount > pool.capacity {
                    let free = (pool.capacity - pool.used).max(0.0);
                    let unit = if pool.unit.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", pool.unit)
                    };
                    return Err(StoreError::Refused(format!(
                        "pool {} has {free} of {}{unit} free",
                        printable(&pool.name),
                        pool.capacity
                    )));
                }
                (args.resource.clone(), args.amount)
            }
        };
        let work = settings(&tx)?;
        let expires = now.saturating_add(lease_ttl(args.ttl_s, &work)?);
        check_lease_room(&tx, now, &me)?;
        let id = next_lease_id(&tx)?;
        tx.execute(
            "INSERT INTO leases(id, kind, resource, amount, holder, card, task, created, expires, heartbeat, \
             expected_end, released, note) VALUES (?,?,?,?,?,?,?,?,?,?,NULL,NULL,?)",
            params![
                id,
                args.kind.as_str(),
                resource,
                amount,
                me,
                args.card,
                args.task,
                now,
                expires,
                now,
                args.note
            ],
        )?;
        Self::append(
            &tx,
            now,
            &me,
            "lease.acquired",
            &id,
            &serde_json::json!({"kind": args.kind.as_str(), "resource": resource, "expires": expires}),
        )?;
        let lease = lease_by_id(&tx, &id)?;
        tx.commit()?;
        Ok(lease)
    }

    /// "lease.renew": holder only; heartbeat = now. A ttl above seven days (or a default above it) -> `Invalid`.
    /// Renewal never adds an active lease, so the per-holder cap is checked only on acquire.
    pub fn lease_renew(&mut self, now: i64, op: &SignedOp) -> Result<LeaseRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "lease.renew")?;
        need(role, Role::Worker)?;
        let args: LeaseRenewArgs = serde_json::from_str(optional_json(&op.arg))
            .map_err(|_| StoreError::Invalid("lease renew arg must be LeaseRenewArgs JSON"))?;
        let lease = lease_by_id(&tx, &op.subject)?;
        if matches!(lease.kind, LeaseKind::Quiet) {
            return Err(StoreError::Invalid("a quiet window has no expiry to renew"));
        }
        let me = op.actor_hex();
        if lease.holder != me {
            return Err(StoreError::Transition(TransitionError::Forbidden(role)));
        }
        if !is_active(&lease, now) {
            return Err(StoreError::Invalid("lease is not active"));
        }
        let expires = now.saturating_add(lease_ttl(args.ttl_s, &settings(&tx)?)?);
        tx.execute(
            "UPDATE leases SET expires=?, heartbeat=? WHERE id=?",
            params![expires, now, lease.id],
        )?;
        Self::append(
            &tx,
            now,
            &me,
            "lease.renewed",
            &lease.id,
            &serde_json::json!({"expires": expires}),
        )?;
        let renewed = lease_by_id(&tx, &lease.id)?;
        tx.commit()?;
        Ok(renewed)
    }

    /// "lease.release": holder, or Supervisor+.
    pub fn lease_release(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "lease.release")?;
        need(role, Role::Worker)?;
        let lease = lease_by_id(&tx, &op.subject)?;
        if matches!(lease.kind, LeaseKind::Quiet) {
            return Err(StoreError::Invalid("use quiet.close"));
        }
        let me = op.actor_hex();
        if lease.holder != me {
            need(role, Role::Supervisor)?;
        }
        if lease.released_ms.is_some() {
            return Err(StoreError::Invalid("lease is already released"));
        }
        tx.execute(
            "UPDATE leases SET released=? WHERE id=?",
            params![now, lease.id],
        )?;
        Self::append(
            &tx,
            now,
            &me,
            "lease.released",
            &lease.id,
            &serde_json::json!({}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// "quiet.open": Controller+; one open at a time.
    pub fn quiet_open(&mut self, now: i64, op: &SignedOp) -> Result<LeaseRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "quiet.open")?;
        need(role, Role::Controller)?;
        let args: QuietArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("quiet arg must be QuietArgs JSON"))?;
        if args.expected_end_ms <= now {
            return Err(StoreError::Invalid("quiet window must end after now"));
        }
        let open: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM leases WHERE kind='quiet' AND released IS NULL)",
            [],
            |r| r.get(0),
        )?;
        if open {
            return Err(StoreError::Invalid("a quiet window is already open"));
        }
        let me = op.actor_hex();
        let id = next_lease_id(&tx)?;
        tx.execute(
            "INSERT INTO leases(id, kind, resource, amount, holder, card, task, created, expires, heartbeat, \
             expected_end, released, note) VALUES (?,'quiet','machine',0,?,NULL,NULL,?,NULL,?,?,NULL,?)",
            params![id, me, now, now, args.expected_end_ms, args.note],
        )?;
        Self::append(
            &tx,
            now,
            &me,
            "quiet.opened",
            &id,
            &serde_json::json!({"expected_end": args.expected_end_ms}),
        )?;
        let lease = lease_by_id(&tx, &id)?;
        tx.commit()?;
        Ok(lease)
    }

    /// "quiet.close": holder or Controller+.
    pub fn quiet_close(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "quiet.close")?;
        need(role, Role::Worker)?;
        let lease = lease_by_id(&tx, &op.subject)?;
        if !matches!(lease.kind, LeaseKind::Quiet) {
            return Err(StoreError::Invalid("lease is not a quiet window"));
        }
        let me = op.actor_hex();
        if lease.holder != me {
            need(role, Role::Controller)?;
        }
        if lease.released_ms.is_some() {
            return Err(StoreError::Invalid("quiet window is already closed"));
        }
        tx.execute(
            "UPDATE leases SET released=? WHERE id=?",
            params![now, lease.id],
        )?;
        Self::append(
            &tx,
            now,
            &me,
            "quiet.closed",
            &lease.id,
            &serde_json::json!({}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn leases(
        &self,
        now: i64,
        kind: Option<LeaseKind>,
        holder: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<LeaseRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LEASE_COLS} FROM leases \
             WHERE (?1 IS NULL OR kind=?1) AND (?2 IS NULL OR holder=?2) \
             AND (?3=0 OR (released IS NULL AND (expires IS NULL OR expires > ?4))) \
             ORDER BY rowid"
        ))?;
        let kind = kind.map(LeaseKind::as_str);
        let rows = stmt.query_map(params![kind, holder, active_only, now], lease_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn pools(&self, now: i64) -> Result<Vec<PoolRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("{POOL_SQL} ORDER BY p.name"))?;
        let rows = stmt.query_map([now], pool_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests;
