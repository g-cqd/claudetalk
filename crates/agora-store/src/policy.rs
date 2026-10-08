//! Team/lane policies and notices.

use crate::identity::{authorize, need};
use crate::{SignedOp, Store, StoreError};
use agora_core::Role;
use agora_sched::claims::validate_name;
use agora_sched::model::{NoticeRecord, PolicyRecord};
use agora_sched::model_ops::PolicyArgs;
use agora_sched::policy::{EffectivePolicy, TeamPolicy, effective, validate};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Longest notice text kept, in characters. Longer text is cut, never refused.
const NOTICE_TEXT_MAX_CHARS: usize = 2000;
/// Most notices one `notices` call returns.
const NOTICE_PAGE_MAX: u32 = 500;
/// The subject part, and the stored team or lane, that matches every team or lane.
const WILDCARD: &str = "*";

impl Store {
    /// "policy.set": subject "team/lane"; Supervisor+; an `escalation_chain` needs Controller+.
    /// The row is replaced whole, so a field the arg leaves out becomes unset.
    pub fn policy_set(&mut self, now: i64, op: &SignedOp) -> Result<PolicyRecord, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "policy.set")?;
        need(role, Role::Supervisor)?;
        let (team, lane) = parse_subject(&op.subject)?;
        let args: PolicyArgs = serde_json::from_str(&op.arg).map_err(|_| {
            StoreError::Invalid("policy arg must be a JSON object with team, lane and policy")
        })?;
        if args.team != team || args.lane != lane {
            return Err(StoreError::Invalid(
                "policy arg team and lane must match the subject",
            ));
        }
        // Setting a chain and clearing a stored one both change who is notified at escalation.
        let stored = stored_policy(&tx, team, lane)?;
        let changes_chain = args.policy.escalation_chain.is_some()
            || stored.is_some_and(|p| p.escalation_chain.is_some());
        if changes_chain {
            need(role, Role::Controller)?;
        }
        validate(&args.policy).map_err(StoreError::Refused)?;
        let value = serde_json::to_value(&args.policy)
            .map_err(|_| StoreError::Invalid("policy cannot be encoded"))?;
        let by = op.actor_hex();
        tx.execute(
            "INSERT INTO policies(team, lane, json, updated, by) VALUES (?,?,?,?,?)
             ON CONFLICT(team, lane) DO UPDATE SET
                 json=excluded.json, updated=excluded.updated, by=excluded.by",
            params![team, lane, value.to_string(), now, by],
        )?;
        Self::append(&tx, now, &by, "policy.set", &op.subject, &value)?;
        let record = PolicyRecord {
            team: team.to_string(),
            lane: lane.to_string(),
            policy: args.policy,
            updated_ms: now,
            by,
        };
        tx.commit()?;
        Ok(record)
    }

    /// Every stored policy, ordered by team, then lane.
    pub fn policies(&self) -> Result<Vec<PolicyRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT team, lane, json, updated, by FROM policies ORDER BY team, lane")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| -> Result<PolicyRecord, StoreError> {
            let (team, lane, json, updated_ms, by) = row?;
            Ok(PolicyRecord {
                team,
                lane,
                policy: decode(&json)?,
                updated_ms,
                by,
            })
        })
        .collect()
    }

    /// The layered policy for `team`/`lane`: the stored rows for ("*","*"), (team,"*"),
    /// ("*",lane) and (team,lane), most general first; missing rows are skipped.
    pub fn effective_policy(
        &self,
        team: &str,
        lane: &str,
        require_review_default: bool,
    ) -> Result<EffectivePolicy, StoreError> {
        let mut keys: Vec<(&str, &str)> = Vec::with_capacity(4);
        let mut layers: Vec<TeamPolicy> = Vec::with_capacity(4);
        for key in [
            (WILDCARD, WILDCARD),
            (team, WILDCARD),
            (WILDCARD, lane),
            (team, lane),
        ] {
            // A wildcard subject repeats an earlier layer; it adds nothing, so it is read once.
            if keys.contains(&key) {
                continue;
            }
            keys.push(key);
            if let Some(p) = stored_policy(&self.conn, key.0, key.1)? {
                layers.push(p);
            }
        }
        let refs: Vec<&TeamPolicy> = layers.iter().collect();
        Ok(effective(&refs, require_review_default))
    }

    /// Notices for `target` (hex key) with seq > `after`, oldest first. `limit` is capped at 500.
    pub fn notices(
        &self,
        target: &str,
        after: i64,
        limit: u32,
    ) -> Result<Vec<NoticeRecord>, StoreError> {
        let limit = i64::from(limit.min(NOTICE_PAGE_MAX));
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, target, kind, subject, text FROM notices
             WHERE target = ? AND seq > ? ORDER BY seq LIMIT ?",
        )?;
        let rows = stmt.query_map(params![target, after, limit], |r| {
            Ok(NoticeRecord {
                seq: r.get(0)?,
                ts_ms: r.get(1)?,
                target: r.get(2)?,
                kind: r.get(3)?,
                subject: r.get(4)?,
                text: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// Appends a notice inside the caller's transaction and returns its seq. The text is cut to
/// `NOTICE_TEXT_MAX_CHARS` characters.
pub(crate) fn post_notice(
    tx: &Transaction,
    now: i64,
    target: &str,
    kind: &str,
    subject: &str,
    text: &str,
) -> Result<i64, StoreError> {
    let text: String = text.chars().take(NOTICE_TEXT_MAX_CHARS).collect();
    tx.execute(
        "INSERT INTO notices(ts, target, kind, subject, text) VALUES (?,?,?,?,?)",
        params![now, target, kind, subject, text],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Splits "team/lane". Each part is `*` or a NAME-valid string, so `/` cannot appear in a part.
fn parse_subject(subject: &str) -> Result<(&str, &str), StoreError> {
    let (team, lane) = subject
        .split_once('/')
        .ok_or(StoreError::Invalid("policy subject must be team/lane"))?;
    if team != WILDCARD {
        validate_name("team", team).map_err(StoreError::Refused)?;
    }
    if lane != WILDCARD {
        validate_name("lane", lane).map_err(StoreError::Refused)?;
    }
    Ok((team, lane))
}

/// The stored policy row for exactly `team`/`lane`, if there is one.
fn stored_policy(
    conn: &Connection,
    team: &str,
    lane: &str,
) -> Result<Option<TeamPolicy>, StoreError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT json FROM policies WHERE team=? AND lane=?",
            params![team, lane],
            |r| r.get(0),
        )
        .optional()?;
    json.as_deref().map(decode).transpose()
}

fn decode(json: &str) -> Result<TeamPolicy, StoreError> {
    serde_json::from_str(json).map_err(|_| StoreError::Invalid("stored policy is corrupt"))
}

#[cfg(test)]
mod tests;
