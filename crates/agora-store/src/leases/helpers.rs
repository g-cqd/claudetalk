//! Private helpers of the lease store: work settings, TTLs, lease ids, path conflicts and row mapping.

use crate::StoreError;
use crate::identity::unhex32;
use agora_crypto::pseudonym;
use agora_sched::cron::civil_from_ms;
use agora_sched::graph::overlap;
use agora_sched::model::{LeaseKind, LeaseRecord, PoolRecord};
use agora_sched::pyfmt::printable;
use agora_sched::settings::WorkSettings;
use rusqlite::{Connection, OptionalExtension, params};

/// Longest accepted lease TTL in seconds: seven days. Applies to explicit, renewed and default TTLs alike.
const MAX_TTL_S: f64 = 604_800.0;

/// Most active path and pool leases one holder may have at once. Quiet windows do not count.
const MAX_ACTIVE_LEASES: i64 = 64;

pub(super) const LEASE_COLS: &str = "id, kind, resource, amount, holder, card, task, created, expires, heartbeat, \
     expected_end, released, note";

/// Pool rows with their used amount: the sum of the pool leases that are active at the bound `now` (?1).
pub(super) const POOL_SQL: &str = "SELECT p.name, p.capacity, p.unit, p.note, p.created, p.created_by, \
     COALESCE((SELECT SUM(l.amount) FROM leases l WHERE l.kind='pool' AND l.resource=p.name \
     AND l.released IS NULL AND (l.expires IS NULL OR l.expires > ?1)), 0) \
     FROM pools p";

/// The stored work settings, or the defaults when no row exists.
pub(super) fn settings(conn: &Connection) -> Result<WorkSettings, StoreError> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM work_settings WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    match json {
        None => Ok(WorkSettings::default()),
        Some(text) => serde_json::from_str(&text)
            .map_err(|_| StoreError::Invalid("corrupt work settings in database")),
    }
}

/// Milliseconds for a TTL in seconds: finite, above zero, at most seven days, at least one millisecond.
fn ttl_ms(ttl_s: f64) -> Result<i64, StoreError> {
    if !ttl_s.is_finite() || ttl_s <= 0.0 {
        return Err(StoreError::Invalid(
            "lease ttl must be a finite number above zero",
        ));
    }
    if ttl_s > MAX_TTL_S {
        return Err(StoreError::Invalid("a lease lasts at most 7 days"));
    }
    let ms = (ttl_s * 1000.0).round();
    if ms < 1.0 {
        return Err(StoreError::Invalid(
            "lease ttl must be at least one millisecond",
        ));
    }
    // `ms` is whole and within [1, MAX_TTL_S * 1000], so the conversion is exact.
    Ok(ms as i64)
}

/// The TTL to use: the requested seconds, else the workspace default.
pub(super) fn lease_ttl(ttl_s: Option<f64>, settings: &WorkSettings) -> Result<i64, StoreError> {
    ttl_ms(ttl_s.unwrap_or(settings.lease_ttl_s))
}

/// An arg whose fields are all optional may be empty, which means `{}`.
pub(super) fn optional_json(arg: &str) -> &str {
    if arg.trim().is_empty() { "{}" } else { arg }
}

/// Refuses a new path or pool lease for `holder` when it already has `MAX_ACTIVE_LEASES` of them active at `now`.
pub(super) fn check_lease_room(
    conn: &Connection,
    now: i64,
    holder: &str,
) -> Result<(), StoreError> {
    let active: i64 = conn.query_row(
        "SELECT COUNT(*) FROM leases WHERE holder=?1 AND kind IN ('path','pool') \
         AND released IS NULL AND (expires IS NULL OR expires > ?2)",
        params![holder, now],
        |r| r.get(0),
    )?;
    if active >= MAX_ACTIVE_LEASES {
        return Err(StoreError::Refused(format!(
            "{} already holds {MAX_ACTIVE_LEASES} active leases",
            name_of_hex(holder)
        )));
    }
    Ok(())
}

/// `lease-<n>` with n = existing leases + 1, skipping any id already taken.
pub(super) fn next_lease_id(conn: &Connection) -> Result<String, StoreError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM leases", [], |r| r.get(0))?;
    let mut n = count.saturating_add(1);
    loop {
        let id = format!("lease-{n}");
        let taken: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM leases WHERE id=?)",
            [id.as_str()],
            |r| r.get(0),
        )?;
        if !taken {
            return Ok(id);
        }
        n = n.saturating_add(1);
    }
}

pub(super) fn is_active(lease: &LeaseRecord, now: i64) -> bool {
    lease.released_ms.is_none() && lease.expires_ms.is_none_or(|expires| expires > now)
}

/// Display name of a hex key; a malformed key shows as an empty name.
fn name_of_hex(key: &str) -> String {
    unhex32(key).map(|pk| pseudonym(&pk)).unwrap_or_default()
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" for an instant in milliseconds.
pub(super) fn iso(ms: i64) -> String {
    let (year, month, day, hour, minute, _) = civil_from_ms(ms);
    let second = ms.div_euclid(1_000).rem_euclid(60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// "path <p> is held by <who> (<what>) until <iso|->".
fn held_by(path: &str, holder: &str, what: &str, until: Option<i64>) -> String {
    let until = until.map_or_else(|| "-".to_string(), iso);
    format!(
        "path {} is held by {} ({what}) until {until}",
        printable(path),
        name_of_hex(holder)
    )
}

/// The first conflict for a canonical `path` requested by `me`: an active path lease of another holder, or a
/// claimed/running task of another identity, whose files overlap it. `None` when the path is free.
pub(super) fn path_holder(
    conn: &Connection,
    now: i64,
    me: &str,
    path: &str,
) -> Result<Option<String>, StoreError> {
    let mut lease_rows = conn.prepare(
        "SELECT id, resource, holder, expires FROM leases \
         WHERE kind='path' AND released IS NULL AND (expires IS NULL OR expires > ?1) AND holder <> ?2 \
         ORDER BY rowid",
    )?;
    let leases = lease_rows.query_map(params![now, me], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    for lease in leases {
        let (id, resource, holder, expires) = lease?;
        if overlap(&resource, path) {
            return Ok(Some(held_by(
                path,
                &holder,
                &format!("lease {id}"),
                expires,
            )));
        }
    }
    let mut task_rows = conn.prepare(
        "SELECT id, agent, files FROM work_tasks \
         WHERE state IN ('claimed','running') AND agent IS NOT ?1 ORDER BY seq",
    )?;
    let tasks = task_rows.query_map([me], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for task in tasks {
        let (id, agent, files) = task?;
        let files: Vec<String> = serde_json::from_str(&files)
            .map_err(|_| StoreError::Invalid("corrupt task files in database"))?;
        if files.iter().any(|file| overlap(file, path)) {
            return Ok(Some(held_by(
                path,
                agent.as_deref().unwrap_or_default(),
                &format!("task {id}"),
                None,
            )));
        }
    }
    Ok(None)
}

pub(super) fn lease_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRecord> {
    let kind: String = r.get(1)?;
    let kind = LeaseKind::parse(&kind).ok_or(rusqlite::Error::InvalidColumnType(
        1,
        "kind".into(),
        rusqlite::types::Type::Text,
    ))?;
    let holder: String = r.get(4)?;
    Ok(LeaseRecord {
        id: r.get(0)?,
        kind,
        resource: r.get(2)?,
        amount: r.get(3)?,
        holder_name: name_of_hex(&holder),
        holder,
        card: r.get(5)?,
        task: r.get(6)?,
        created_ms: r.get(7)?,
        expires_ms: r.get(8)?,
        heartbeat_ms: r.get(9)?,
        expected_end_ms: r.get(10)?,
        released_ms: r.get(11)?,
        note: r.get(12)?,
    })
}

pub(super) fn lease_by_id(conn: &Connection, id: &str) -> Result<LeaseRecord, StoreError> {
    conn.query_row(
        &format!("SELECT {LEASE_COLS} FROM leases WHERE id=?1"),
        [id],
        lease_from_row,
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound(format!("lease {}", printable(id))))
}

pub(super) fn pool_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PoolRecord> {
    Ok(PoolRecord {
        name: r.get(0)?,
        capacity: r.get(1)?,
        unit: r.get(2)?,
        note: r.get(3)?,
        created_ms: r.get(4)?,
        created_by: r.get(5)?,
        used: r.get(6)?,
    })
}

/// One pool with its used amount at `now`, or `None` when it is not defined.
pub(super) fn find_pool(
    conn: &Connection,
    now: i64,
    name: &str,
) -> Result<Option<PoolRecord>, StoreError> {
    Ok(conn
        .query_row(
            &format!("{POOL_SQL} WHERE p.name=?2"),
            params![now, name],
            pool_from_row,
        )
        .optional()?)
}
