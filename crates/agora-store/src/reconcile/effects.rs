//! The effects of reconcile actions on cards and admission: returning a claim to the pool, and opening,
//! refreshing and closing the blocker card, and pausing admission.

use super::{iso_utc, state_value};
use crate::{Store, StoreError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

fn card_exists(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM cards WHERE id = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

/// True when the card exists and is neither done nor cancelled.
pub(super) fn card_is_open(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    let state: Option<String> = conn
        .query_row("SELECT state FROM cards WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    Ok(matches!(state.as_deref(), Some(s) if s != "done" && s != "cancelled"))
}

/// Return a claim to the pool, but only while `owner` still holds it: a card that was re-claimed or
/// moved since the action was planned is left alone. Mirrors `reclaim_stale`'s effect.
pub(super) fn reclaim_card(
    tx: &Transaction<'_>,
    now: i64,
    actor: &str,
    card: &str,
    owner: &str,
) -> Result<(), StoreError> {
    let from: Option<String> = tx
        .query_row(
            "SELECT state FROM cards WHERE id = ?1 AND owner = ?2 AND state IN ('claimed','doing')",
            params![card, owner],
            |r| r.get(0),
        )
        .optional()?;
    let Some(from) = from else {
        return Ok(());
    };
    tx.execute(
        "UPDATE cards SET state = 'ready', owner = NULL, updated = ?1 WHERE id = ?2",
        params![now, card],
    )?;
    tx.execute("DELETE FROM approvals WHERE card = ?1", [card])?;
    Store::append(
        tx,
        now,
        actor,
        "card.reclaimed",
        card,
        &serde_json::json!({}),
    )?;
    Store::append(
        tx,
        now,
        actor,
        "card.moved",
        card,
        &serde_json::json!({"from": from, "to": "ready"}),
    )?;
    Ok(())
}

/// Create the open blocker card, or refresh the one already open (remembered in `work_state`). Returns its id.
pub(super) fn open_blocker(
    tx: &Transaction<'_>,
    now: i64,
    actor: &str,
    title: &str,
    owner_action: &str,
) -> Result<String, StoreError> {
    if let Some(id) = state_value(tx, "blocker_card")?
        && card_is_open(tx, &id)?
    {
        tx.execute(
            "UPDATE cards SET title = ?1, owner_action = ?2, needs_owner = 1, updated = ?3 WHERE id = ?4",
            params![title, owner_action, now, id],
        )?;
        return Ok(id);
    }
    let id = next_blocker_id(tx)?;
    tx.execute(
        "INSERT INTO cards(id, title, lane, state, owner, created, updated, team, needs_owner, owner_action) \
         VALUES (?1, ?2, 'ops', 'blocked', NULL, ?3, ?3, '', 1, ?4)",
        params![id, title, now, owner_action],
    )?;
    Store::append(
        tx,
        now,
        actor,
        "card.created",
        &id,
        &serde_json::json!({"title": title, "lane": "ops"}),
    )?;
    tx.execute(
        "INSERT INTO work_state(key, value) VALUES ('blocker_card', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [id.as_str()],
    )?;
    Ok(id)
}

/// `blocker-<n>` with n = 1 + the number of existing `blocker-%` cards, skipping any id already taken.
fn next_blocker_id(conn: &Connection) -> Result<String, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cards WHERE id LIKE 'blocker-%'",
        [],
        |r| r.get(0),
    )?;
    for n in count.saturating_add(1)..=i64::MAX {
        let id = format!("blocker-{n}");
        if !card_exists(conn, &id)? {
            return Ok(id);
        }
    }
    Err(StoreError::Invalid("no free blocker card id"))
}

/// Cancel the open blocker card with a resolution note, and forget it. Returns the closed card's id, if there was one.
pub(super) fn close_blocker(
    tx: &Transaction<'_>,
    now: i64,
    actor: &str,
) -> Result<Option<String>, StoreError> {
    let Some(id) = state_value(tx, "blocker_card")? else {
        tx.execute("DELETE FROM work_state WHERE key = 'blocker_card'", [])?;
        return Ok(None);
    };
    tx.execute(
        "UPDATE cards SET needs_owner = 0 WHERE id = ?1",
        [id.as_str()],
    )?;
    let from: Option<String> = tx
        .query_row(
            "SELECT state FROM cards WHERE id = ?1 AND state NOT IN ('done','cancelled')",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(from) = from {
        tx.execute(
            "UPDATE cards SET state = 'cancelled', updated = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO evidence(card, ref, by) VALUES (?1, ?2, ?3)",
            params![id, format!("resolved at {}", iso_utc(now)), actor],
        )?;
        Store::append(
            tx,
            now,
            actor,
            "card.moved",
            &id,
            &serde_json::json!({"from": from, "to": "cancelled"}),
        )?;
    }
    tx.execute("DELETE FROM work_state WHERE key = 'blocker_card'", [])?;
    Ok(Some(id))
}

/// Set (`Some(reason)`) or clear (`None`) the admission pause.
pub(super) fn set_admission(tx: &Transaction<'_>, paused: Option<&str>) -> Result<(), StoreError> {
    match paused {
        Some(reason) => tx.execute(
            "INSERT INTO work_state(key, value) VALUES ('admission_paused', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [reason],
        )?,
        None => tx.execute("DELETE FROM work_state WHERE key = 'admission_paused'", [])?,
    };
    Ok(())
}
