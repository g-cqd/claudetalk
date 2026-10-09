//! R7 claim expiry in the store: the claims the reconciler sees, and returning one claimed task to the queue.

use crate::{Store, StoreError};
use agora_sched::reconcile::ClaimSnap;
use agora_sched::settings::WorkSettings;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

/// The note a task carries after its claim expired, and the `note` and `detail` of its `work.reclaimed` event.
const CLAIM_EXPIRED: &str = "claim expired";

/// Largest claim TTL the store compares, in milliseconds: half of `i64::MAX`, so a huge setting cannot wrap when it is
/// added to a claim time. Real TTLs are far below it.
const MAX_CLAIM_TTL_MS: i64 = i64::MAX / 2;

/// Work tasks in state `claimed` with no run request yet and a command, oldest first. A task without a command is
/// worked by its claimant and closed with `work.finish`, so its claim never expires. A row without an agent or a
/// claim time has no claim to expire, so it is not listed either.
pub(super) fn read_claims(conn: &Connection) -> Result<Vec<ClaimSnap>, StoreError> {
    let mut st = conn.prepare_cached(
        "SELECT id, agent, claimed, title FROM work_tasks \
         WHERE state='claimed' AND requested IS NULL AND command IS NOT NULL \
         AND claimed IS NOT NULL AND agent IS NOT NULL ORDER BY seq",
    )?;
    let rows = st.query_map([], |r| {
        Ok(ClaimSnap {
            task: r.get(0)?,
            agent: r.get(1)?,
            claimed_ms: r.get(2)?,
            title: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The claim TTL in whole milliseconds, read from the stored work settings (the defaults when no row exists). `None`
/// when the TTL is not finite and above zero, which turns expiry off, as it does for the planner.
fn claim_ttl_ms(conn: &Connection) -> Result<Option<i64>, StoreError> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM work_settings WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let settings = match json {
        None => WorkSettings::default(),
        Some(text) => serde_json::from_str(&text)
            .map_err(|_| StoreError::Invalid("corrupt work settings in database"))?,
    };
    let ttl_s = settings.claim_ttl_s;
    if !ttl_s.is_finite() || ttl_s <= 0.0 {
        return Ok(None);
    }
    // The float-to-integer cast saturates, so a huge TTL stays huge instead of wrapping.
    Ok(Some(((ttl_s * 1000.0) as i64).min(MAX_CLAIM_TTL_MS)))
}

/// Return `task` to the queue, but only while `agent` still holds the claim made at `claimed_ms`, the task has a
/// command, no run has been requested for it, and the claim is at least the claim TTL old: a claim that was re-made,
/// requested or is still fresh is left alone, whatever the action says. When the task moves, one `work.reclaimed`
/// event is written in the shape `work.reclaim` writes (note, scope, detail).
pub(super) fn expire_claim(
    tx: &Transaction<'_>,
    now: i64,
    actor: &str,
    task: &str,
    agent: &str,
    claimed_ms: i64,
) -> Result<(), StoreError> {
    let Some(ttl_ms) = claim_ttl_ms(tx)? else {
        return Ok(());
    };
    // `claimed` equals `claimed_ms` here, and `ttl_ms` is at most `MAX_CLAIM_TTL_MS`; should the sum still exceed
    // the integer range, SQLite promotes it to a real rather than wrapping, so the comparison stays correct.
    let changed = tx.execute(
        "UPDATE work_tasks SET state='queued', agent=NULL, claimed=NULL, accepted=NULL, note=?1 \
         WHERE id=?2 AND state='claimed' AND agent=?3 AND claimed=?4 AND requested IS NULL \
         AND command IS NOT NULL AND claimed + ?5 <= ?6",
        params![CLAIM_EXPIRED, task, agent, claimed_ms, ttl_ms, now],
    )?;
    if changed != 1 {
        return Ok(());
    }
    let scope: Option<String> =
        tx.query_row("SELECT scope FROM work_tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })?;
    Store::append(
        tx,
        now,
        actor,
        "work.reclaimed",
        task,
        &json!({
            "note": CLAIM_EXPIRED,
            "scope": scope,
            "detail": CLAIM_EXPIRED,
        }),
    )?;
    Ok(())
}
