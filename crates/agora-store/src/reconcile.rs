//! Reconciler support: input snapshot, idempotent application of system actions, watchdog report.

mod claims;
mod effects;
#[cfg(test)]
mod tests;

use self::claims::{expire_claim, read_claims};
use self::effects::{card_is_open, close_blocker, open_blocker, reclaim_card, set_admission};
use crate::system::authorize_system;
use crate::{SignedOp, Store, StoreError, hex, unhex32};
use agora_crypto::pseudonym;
use agora_sched::admit::MachineSnapshot;
use agora_sched::cron::civil_from_ms;
use agora_sched::model::{
    LeaseKind, LeaseRecord, NeedsOwner, NoticeRecord, StaleCard, WatchdogReport, WipView,
};
use agora_sched::policy::EffectivePolicy;
use agora_sched::reconcile::{CardSnap, Mark, ReconcileAction, ReconcileInput, SprintSnap};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

/// Leases expiring within this window (or already expired and unreleased) are watchdog findings.
const EXPIRY_WINDOW_MS: i64 = 10 * 60 * 1000;
/// How many of the caller's own notices the watchdog returns, oldest first.
const WATCHDOG_NOTICES: i64 = 50;
/// Substring of the note the daemon writes on a running job whose supervisor died ("its supervisor, process N, is
/// gone; ..."). The watchdog finds those jobs by this substring, so the daemon's wording (agora-daemon `jobs.rs`) and
/// this constant must change together; the snapshot test pins the whole note.
const ORPHAN_NOTE: &str = "its supervisor";

/// What applying one reconcile action did. `applied` is false when its key was already marked; `notice_posted` is
/// true only when the action posted a notice in the same transaction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub applied: bool,
    pub notice_posted: bool,
}

impl Store {
    /// Snapshot of everything `agora_sched::reconcile::plan` reads.
    pub fn reconcile_input(
        &self,
        now: i64,
        machine: Option<MachineSnapshot>,
        require_review_default: bool,
    ) -> Result<ReconcileInput, StoreError> {
        let conn = &self.conn;
        let blocker_card = match state_value(conn, "blocker_card")? {
            Some(id) if card_is_open(conn, &id)? => Some(id),
            _ => None,
        };
        Ok(ReconcileInput {
            now_ms: now,
            settings: self.work_settings()?,
            machine,
            cards: in_progress_cards(conn)?,
            leases: read_leases(
                conn,
                "SELECT id, kind, resource, amount, holder, card, task, created, expires, heartbeat, \
                 expected_end, released, note FROM leases WHERE released IS NULL ORDER BY id",
                [],
            )?,
            policies: self.policies()?,
            require_review_default,
            marks: read_marks(conn)?,
            supervisors: supervisor_keys(conn)?,
            admission_paused: state_value(conn, "admission_paused")?,
            blocker_card,
            sprints: read_sprints(conn)?,
            claims: read_claims(conn)?,
        })
    }

    /// Apply one action signed by the system (op.kind == action.op_kind(), arg = action JSON). Idempotent: if
    /// reconcile_marks has the key -> Ok(false) and nothing changes; else apply + insert the mark -> Ok(true).
    pub fn apply_reconcile(&mut self, now: i64, op: &SignedOp) -> Result<bool, StoreError> {
        Ok(self.apply_reconcile_outcome(now, op)?.applied)
    }

    /// As `apply_reconcile`, and also reports whether the action posted a notice. Only a claim expiry posts one, and
    /// only when it releases the claim.
    pub fn apply_reconcile_outcome(
        &mut self,
        now: i64,
        op: &SignedOp,
    ) -> Result<ReconcileOutcome, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        authorize_system(&tx, now, op, &op.kind)?;
        let action: ReconcileAction = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("reconcile arg must be a reconcile action"))?;
        if action.op_kind() != op.kind.as_str() {
            return Err(StoreError::Invalid("wrong operation kind"));
        }
        let key = action.key();
        let marked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reconcile_marks WHERE key = ?1)",
            [key],
            |r| r.get(0),
        )?;
        if marked {
            // Nothing is committed: the transaction rolls back when it is dropped.
            return Ok(ReconcileOutcome::default());
        }
        let actor = op.actor_hex();
        let mut notice_posted = false;
        // The audit event's subject is the entity the action touched; its data carries the idempotency key.
        let subject: String = match &action {
            ReconcileAction::ExpireLease { lease, .. } => {
                // Only a path or pool lease whose expiry has passed; a quiet window is closed by CloseQuiet.
                tx.execute(
                    "UPDATE leases SET released = ?1 WHERE id = ?2 AND released IS NULL \
                     AND kind IN ('path','pool') AND expires IS NOT NULL AND expires <= ?1",
                    params![now, lease],
                )?;
                lease.clone()
            }
            ReconcileAction::ExpireClaim {
                task,
                agent,
                claimed_ms,
                notice_key,
                text,
                ..
            } => {
                // The release and its notice share this transaction: a claim that was not released gets no notice.
                if expire_claim(&tx, now, &actor, task, agent, *claimed_ms)? {
                    // Old-shape actions (pre-0.2.0 planner) carry no notice; the store has no task title to rebuild
                    // it, so it posts none.
                    if !notice_key.is_empty() && !text.is_empty() {
                        crate::policy::post_notice(&tx, now, agent, "claim_expired", task, text)?;
                        // The notice-key mark is kept for continuity with the pre-0.2.0 mark set (pruned with the rest).
                        tx.execute(
                            "INSERT OR IGNORE INTO reconcile_marks(key, last, count) VALUES (?1, ?2, 1)",
                            params![notice_key, now],
                        )?;
                        notice_posted = true;
                    }
                }
                task.clone()
            }
            ReconcileAction::CloseQuiet { lease, .. } => {
                tx.execute(
                    "UPDATE leases SET released = ?1 WHERE id = ?2 AND released IS NULL AND kind = 'quiet'",
                    params![now, lease],
                )?;
                lease.clone()
            }
            ReconcileAction::Notice {
                target,
                kind,
                subject,
                text,
                ..
            } => {
                crate::policy::post_notice(&tx, now, target, kind, subject, text)?;
                target.clone()
            }
            ReconcileAction::ReclaimCard { card, owner, .. } => {
                reclaim_card(&tx, now, &actor, card, owner)?;
                card.clone()
            }
            ReconcileAction::Blocker {
                open: true,
                title,
                owner_action,
                ..
            } => open_blocker(&tx, now, &actor, title, owner_action)?,
            ReconcileAction::Blocker { open: false, .. } => {
                close_blocker(&tx, now, &actor)?.unwrap_or_else(|| "blocker".into())
            }
            ReconcileAction::Admission { paused, .. } => {
                set_admission(&tx, paused.as_deref())?;
                "admission".into()
            }
        };
        tx.execute(
            "INSERT INTO reconcile_marks(key, last, count) VALUES (?1, ?2, 1)",
            params![key, now],
        )?;
        Store::append(
            &tx,
            now,
            &actor,
            &op.kind,
            &subject,
            &serde_json::json!({"key": key}),
        )?;
        tx.commit()?;
        Ok(ReconcileOutcome {
            applied: true,
            notice_posted,
        })
    }

    /// The reason admission is paused, if it is.
    pub fn admission_paused(&self) -> Result<Option<String>, StoreError> {
        state_value(&self.conn, "admission_paused")
    }

    /// Findings for a supervisor: stale claims, leases about to expire, the caller's last notices, cards that
    /// need an owner, WIP per lane, the pause reason and jobs whose supervisor died.
    pub fn watchdog_report(
        &self,
        now: i64,
        me: &str,
        require_review_default: bool,
    ) -> Result<WatchdogReport, StoreError> {
        let conn = &self.conn;
        let mut stale = Vec::new();
        let mut in_progress: BTreeMap<(String, String), u32> = BTreeMap::new();
        for card in in_progress_cards(conn)? {
            let eff = self.effective_policy(&card.team, &card.lane, require_review_default)?;
            let idle_s = secs(now.saturating_sub(card.updated_ms));
            if let Some(stage) = stage_of(&eff, idle_s) {
                stale.push(StaleCard {
                    id: card.id,
                    title: card.title,
                    owner: card.owner,
                    state: card.state,
                    idle_s,
                    stage: stage.into(),
                });
            }
            let count = in_progress.entry((card.team, card.lane)).or_insert(0);
            *count = count.saturating_add(1);
        }
        let mut wip = Vec::with_capacity(in_progress.len());
        for ((team, lane), count) in in_progress {
            let limit = self
                .effective_policy(&team, &lane, require_review_default)?
                .wip_limit;
            wip.push(WipView {
                team,
                lane,
                count,
                limit,
            });
        }
        let expiring_leases = read_leases(
            conn,
            "SELECT id, kind, resource, amount, holder, card, task, created, expires, heartbeat, \
             expected_end, released, note FROM leases \
             WHERE released IS NULL AND expires IS NOT NULL AND expires <= ?1 ORDER BY expires, id",
            params![now.saturating_add(EXPIRY_WINDOW_MS)],
        )?;
        let mut notices = {
            let mut st = conn.prepare_cached(
                "SELECT seq, ts, target, kind, subject, text FROM notices \
                 WHERE target = ?1 ORDER BY seq DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![me, WATCHDOG_NOTICES], |r| {
                Ok(NoticeRecord {
                    seq: r.get(0)?,
                    ts_ms: r.get(1)?,
                    target: r.get(2)?,
                    kind: r.get(3)?,
                    subject: r.get(4)?,
                    text: r.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        notices.reverse();
        let needs_owner = {
            let mut st = conn.prepare_cached(
                "SELECT id, title, owner_action FROM cards WHERE needs_owner = 1 ORDER BY id",
            )?;
            let rows = st.query_map([], |r| {
                Ok(NeedsOwner {
                    card: r.get(0)?,
                    title: r.get(1)?,
                    owner_action: r.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let orphaned_jobs = {
            let mut st = conn.prepare_cached(
                "SELECT id FROM work_tasks WHERE state = 'running' AND instr(note, ?1) > 0 ORDER BY seq",
            )?;
            let rows = st.query_map([ORPHAN_NOTE], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        Ok(WatchdogReport {
            now_ms: now,
            stale,
            expiring_leases,
            notices,
            needs_owner,
            wip,
            paused: self.admission_paused()?,
            orphaned_jobs,
        })
    }
}

/// Claimed and doing cards, least recently updated first.
fn in_progress_cards(conn: &Connection) -> Result<Vec<CardSnap>, StoreError> {
    let mut st = conn.prepare_cached(
        "SELECT id, title, team, lane, state, owner, updated FROM cards \
         WHERE state IN ('claimed','doing') ORDER BY updated, id",
    )?;
    let rows = st.query_map([], |r| {
        Ok(CardSnap {
            id: r.get(0)?,
            title: r.get(1)?,
            team: r.get(2)?,
            lane: r.get(3)?,
            state: r.get(4)?,
            owner: r.get(5)?,
            updated_ms: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Reads lease rows selected by `sql`, which must select the 13 lease columns in the order used below.
fn read_leases<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    p: P,
) -> Result<Vec<LeaseRecord>, StoreError> {
    let mut st = conn.prepare_cached(sql)?;
    let rows = st.query_map(p, |r| {
        let kind_raw: String = r.get(1)?;
        let kind = LeaseKind::parse(&kind_raw).ok_or(rusqlite::Error::InvalidColumnType(
            1,
            "kind".into(),
            rusqlite::types::Type::Text,
        ))?;
        let holder: String = r.get(4)?;
        let holder_name = unhex32(&holder)
            .map(|pk| pseudonym(&pk))
            .unwrap_or_default();
        Ok(LeaseRecord {
            id: r.get(0)?,
            kind,
            resource: r.get(2)?,
            amount: r.get(3)?,
            holder,
            holder_name,
            card: r.get(5)?,
            task: r.get(6)?,
            created_ms: r.get(7)?,
            expires_ms: r.get(8)?,
            heartbeat_ms: r.get(9)?,
            expected_end_ms: r.get(10)?,
            released_ms: r.get(11)?,
            note: r.get(12)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn read_marks(conn: &Connection) -> Result<Vec<Mark>, StoreError> {
    let mut st =
        conn.prepare_cached("SELECT key, last, count FROM reconcile_marks ORDER BY key")?;
    let rows = st.query_map([], |r| {
        Ok(Mark {
            key: r.get(0)?,
            last_ms: r.get(1)?,
            count: count_u32(r.get(2)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Hex keys of active, non-system identities with role supervisor, controller or owner, sorted.
fn supervisor_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut st = conn.prepare_cached(
        "SELECT pubkey FROM identities \
         WHERE role IN ('supervisor','controller','owner') AND revoked = 0 AND system = 0",
    )?;
    let rows = st.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    let mut keys = Vec::new();
    for raw in rows {
        keys.push(hex(&raw?));
    }
    keys.sort();
    Ok(keys)
}

/// Active sprints with the number of their cards that are neither done nor cancelled.
fn read_sprints(conn: &Connection) -> Result<Vec<SprintSnap>, StoreError> {
    let mut st = conn.prepare_cached(
        "SELECT s.id, s.name, s.ends, s.state, \
            (SELECT COUNT(*) FROM sprint_cards sc JOIN cards c ON c.id = sc.card \
             WHERE sc.sprint = s.id AND c.state NOT IN ('done','cancelled')) \
         FROM sprints s WHERE s.state = 'active' ORDER BY s.id",
    )?;
    let rows = st.query_map([], |r| {
        Ok(SprintSnap {
            id: r.get(0)?,
            name: r.get(1)?,
            ends_ms: r.get(2)?,
            state: r.get(3)?,
            open_cards: count_u32(r.get(4)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The value stored under `key` in `work_state`, if any.
fn state_value(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row("SELECT value FROM work_state WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

/// `2026-10-08T17:33:00Z` for a UTC instant in milliseconds since the epoch; the sub-second part is dropped.
fn iso_utc(ms: i64) -> String {
    let (year, month, day, hour, minute, _weekday) = civil_from_ms(ms);
    let second = ms.rem_euclid(60_000) / 1000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Stage of a claim idle for `idle_s` seconds under `eff`, or None while it is fresh.
fn stage_of(eff: &EffectivePolicy, idle_s: f64) -> Option<&'static str> {
    if idle_s < eff.stale_after_s {
        None
    } else if eff.reclaim && idle_s >= eff.reclaim_after_s {
        Some("reclaim due")
    } else if idle_s >= eff.stale_after_s + eff.escalate_after_s {
        Some("escalated")
    } else {
        Some("stale")
    }
}

/// Milliseconds as seconds, for comparison with the policy thresholds (which are in seconds).
fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

fn count_u32(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
