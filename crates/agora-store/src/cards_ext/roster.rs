//! The effective policy of a team and lane, and the identity lookups behind escalation.

use crate::StoreError;
use agora_core::Role;
use agora_sched::policy::{EffectivePolicy, TeamPolicy, effective};
use rusqlite::{Connection, OptionalExtension, params};

/// The effective policy for a team and lane: the four layers of `policies` (`*`/`*`, team/`*`,
/// `*`/lane, team/lane), most general first, combined by `agora_sched::policy::effective`.
pub(super) fn effective_for(
    conn: &Connection,
    team: &str,
    lane: &str,
    require_review_default: bool,
) -> Result<EffectivePolicy, StoreError> {
    let keys = [("*", "*"), (team, "*"), ("*", lane), (team, lane)];
    let mut layers: Vec<TeamPolicy> = Vec::with_capacity(keys.len());
    for (t, l) in keys {
        let json: Option<String> = conn
            .query_row(
                "SELECT json FROM policies WHERE team=? AND lane=?",
                params![t, l],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = json {
            layers.push(
                serde_json::from_str(&json)
                    .map_err(|_| StoreError::Invalid("corrupt policy in database"))?,
            );
        }
    }
    let refs: Vec<&TeamPolicy> = layers.iter().collect();
    Ok(effective(&refs, require_review_default))
}

/// True when `pk` is a registered identity that is not revoked and not the system identity.
pub(super) fn is_active_person(conn: &Connection, pk: &[u8]) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM identities WHERE pubkey=? AND revoked=0 AND system=0)",
        [pk],
        |r| r.get(0),
    )?)
}

/// Hex keys of the active, non-system identities with role Supervisor or higher, sorted.
pub(super) fn supervisors(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut st = conn.prepare(
        "SELECT lower(hex(pubkey)), role FROM identities WHERE revoked=0 AND system=0 ORDER BY 1",
    )?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (pk, role) = row?;
        if Role::parse(&role).is_some_and(|r| r >= Role::Supervisor) {
            out.push(pk);
        }
    }
    Ok(out)
}
