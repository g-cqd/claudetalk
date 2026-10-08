//! The board summary: cards needing an owner, WIP per lane, job and lease counts, and the
//! open manual quiet window.

use super::effective_for;
use crate::StoreError;
use agora_sched::model::{NeedsOwner, QuietView, WipView};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn needs_owner_cards(conn: &Connection) -> Result<Vec<NeedsOwner>, StoreError> {
    let mut st =
        conn.prepare("SELECT id, title, owner_action FROM cards WHERE needs_owner=1 ORDER BY id")?;
    let rows = st.query_map([], |r| {
        Ok(NeedsOwner {
            card: r.get(0)?,
            title: r.get(1)?,
            owner_action: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<NeedsOwner>, _>>()?)
}

/// WIP per (team, lane) of any card, shown when it has cards in progress or a limit.
pub(super) fn wip_views(conn: &Connection) -> Result<Vec<WipView>, StoreError> {
    let mut st = conn.prepare("SELECT DISTINCT team, lane FROM cards ORDER BY team, lane")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let pairs = rows.collect::<Result<Vec<(String, String)>, _>>()?;
    let mut out = Vec::new();
    for (team, lane) in pairs {
        let count = count_u32_params(
            conn,
            "SELECT COUNT(*) FROM cards WHERE team=? AND lane=? AND state IN ('claimed','doing')",
            params![team, lane],
        )?;
        let limit = effective_for(conn, &team, &lane, false)?.wip_limit;
        if count > 0 || limit.is_some() {
            out.push(WipView {
                team,
                lane,
                count,
                limit,
            });
        }
    }
    Ok(out)
}

/// The open quiet lease, shown as a manual window; `seconds_left` is 0 once its end has passed.
pub(super) fn quiet_view(conn: &Connection, now: i64) -> Result<Option<QuietView>, StoreError> {
    let open: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT id, expected_end FROM leases WHERE kind='quiet' AND released IS NULL \
             ORDER BY created, id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(open.map(|(lease, end)| {
        let seconds_left = end.map_or(0.0, |end| {
            (end.saturating_sub(now) as f64 / 1000.0).max(0.0)
        });
        QuietView {
            state: "manual".into(),
            job: None,
            text: format!("manual quiet window {lease}"),
            lease: Some(lease),
            seconds_left,
        }
    }))
}

pub(super) fn count_u32(conn: &Connection, sql: &str) -> Result<u32, StoreError> {
    count_u32_params(conn, sql, [])
}

pub(super) fn count_u32_at(conn: &Connection, sql: &str, now: i64) -> Result<u32, StoreError> {
    count_u32_params(conn, sql, params![now])
}

fn count_u32_params(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<u32, StoreError> {
    let n: i64 = conn.query_row(sql, p, |r| r.get(0))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}
