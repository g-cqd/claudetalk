//! Card extensions: priority/value/size/team, dependencies, links, escalation, WIP limits,
//! and the card side of job completion (D7). Submodules: `fields` (create-arg validation),
//! `job_sync` (D7) and `board` (the board summary).

mod board;
mod fields;
mod job_sync;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_sync;

pub(crate) use fields::NewCardFields;
pub(crate) use job_sync::card_job_sync_tx;

use crate::identity::{authorize, need};
use crate::policy::post_notice;
use crate::{SignedOp, Store, StoreError, require_card, unhex32};
use agora_core::{CardState, Role};
use agora_sched::graph::Graph;
use agora_sched::model::{
    BoardExtras, CardDepArgs, CardExt, CardLink, CardLinkArgs, CardUpdateArgs, EscalateArgs,
    LinkRel,
};
use agora_sched::policy::{EffectivePolicy, TeamPolicy, effective};
use agora_sched::task::{Task, tier_of};
use board::{count_u32, count_u32_at, needs_owner_cards, quiet_view, wip_views};
use fields::{check_positive, check_team, unique};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::BTreeMap;

const CYCLE: &str = "card dependencies would form a cycle";
const VALUE_MSG: &str = "card value must be a positive number";
const SIZE_MSG: &str = "card size must be a positive number";
const AFTER_MSG: &str = "card after must be a list of card ids";
const TARGET_MSG: &str = "escalation target must be a 64-character lowercase hex public key";
/// Longest owner action, in characters.
const OWNER_ACTION_MAX: usize = 1000;
/// Most explicit escalation targets one `card.escalate` names.
const ESCALATE_TARGETS_MAX: usize = 16;

impl Store {
    /// "card.update": the owner (or any Worker for an unowned card) changes value, size, team,
    /// needs_owner, owner_action and priority; anyone else needs a Supervisor. Does not touch
    /// `updated`, so it does not reset the stale-claim clock.
    pub fn card_update(&mut self, now: i64, op: &SignedOp) -> Result<CardExt, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.update")?;
        let args: CardUpdateArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("card update arg must be JSON"))?;
        let actor = op.actor_hex();
        let c = load_card(&tx, &op.subject)?;
        check_edit(role, c.owner.as_deref(), &actor)?;
        let priority = args.priority.unwrap_or(c.priority);
        let value = match args.value {
            Some(v) => check_positive(v, VALUE_MSG)?,
            None => c.value,
        };
        let size = match args.size {
            Some(v) => check_positive(v, SIZE_MSG)?,
            None => c.size,
        };
        let team = match args.team {
            Some(t) => {
                check_team(&t)?;
                t
            }
            None => c.team,
        };
        let needs_owner = args.needs_owner.unwrap_or(c.needs_owner);
        let owner_action = match args.owner_action {
            Some(action) => {
                if action.chars().count() > OWNER_ACTION_MAX {
                    return Err(StoreError::Invalid(
                        "owner action is longer than 1000 characters",
                    ));
                }
                action
            }
            None => c.owner_action,
        };
        tx.execute(
            "UPDATE cards SET priority=?, value=?, size=?, team=?, needs_owner=?, owner_action=? WHERE id=?",
            params![priority, value, size, team, needs_owner, owner_action, op.subject],
        )?;
        Self::append(
            &tx,
            now,
            &actor,
            "card.updated",
            &op.subject,
            &serde_json::json!({
                "priority": priority, "value": value, "size": size, "team": team,
                "needs_owner": needs_owner, "owner_action": owner_action
            }),
        )?;
        let ext = card_ext_of(&tx, &op.subject)?;
        tx.commit()?;
        Ok(ext)
    }

    /// "card.dep": adds or removes `after` as a prerequisite of the subject card. Refuses a
    /// new edge that closes a cycle; the transaction then rolls back.
    pub fn card_dep(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.dep")?;
        let args: CardDepArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("card dep arg must be JSON"))?;
        let actor = op.actor_hex();
        let c = load_card(&tx, &op.subject)?;
        check_edit(role, c.owner.as_deref(), &actor)?;
        if args.after == op.subject {
            return Err(StoreError::Invalid("a card cannot depend on itself"));
        }
        require_card(&tx, &args.after)?;
        if args.remove {
            let n = tx.execute(
                "DELETE FROM card_deps WHERE card=? AND after=?",
                params![op.subject, args.after],
            )?;
            if n == 0 {
                return Err(StoreError::Invalid("card has no such dependency"));
            }
            Self::append(
                &tx,
                now,
                &actor,
                "card.dep_removed",
                &op.subject,
                &serde_json::json!({"after": args.after}),
            )?;
        } else {
            let n = tx.execute(
                "INSERT OR IGNORE INTO card_deps(card, after) VALUES (?,?)",
                params![op.subject, args.after],
            )?;
            if n == 0 {
                return Err(StoreError::Invalid("card already depends on that card"));
            }
            check_acyclic(&tx)?;
            Self::append(
                &tx,
                now,
                &actor,
                "card.dep_added",
                &op.subject,
                &serde_json::json!({"after": args.after}),
            )?;
        }
        Ok(tx.commit()?)
    }

    /// "card.link": adds or removes a typed link from the subject card to `target`.
    pub fn card_link(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.link")?;
        let args: CardLinkArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("card link arg must be JSON"))?;
        let actor = op.actor_hex();
        let c = load_card(&tx, &op.subject)?;
        check_edit(role, c.owner.as_deref(), &actor)?;
        if args.target.is_empty() || args.target.len() > 256 {
            return Err(StoreError::Invalid("link target must be 1 to 256 bytes"));
        }
        let rel = args.rel.as_str();
        let data = serde_json::json!({"rel": rel, "target": args.target});
        if args.remove {
            let n = tx.execute(
                "DELETE FROM card_links WHERE card=? AND rel=? AND target=?",
                params![op.subject, rel, args.target],
            )?;
            if n == 0 {
                return Err(StoreError::Invalid("card has no such link"));
            }
            Self::append(&tx, now, &actor, "card.unlinked", &op.subject, &data)?;
        } else {
            let n = tx.execute(
                "INSERT OR IGNORE INTO card_links(card, rel, target, created, by) VALUES (?,?,?,?,?)",
                params![op.subject, rel, args.target, now, actor],
            )?;
            if n == 0 {
                return Err(StoreError::Invalid("card already has that link"));
            }
            Self::append(&tx, now, &actor, "card.linked", &op.subject, &data)?;
        }
        Ok(tx.commit()?)
    }

    /// "card.escalate": the owner (Worker+) or a Supervisor+ marks the card as needing its
    /// owner and notifies `to`; with no `to`, the effective escalation chain; with neither,
    /// every supervisor. Returns the hex keys notified. The owner action is at most 1000
    /// characters; `to` names at most 16 active, non-system identities.
    pub fn card_escalate(
        &mut self,
        now: i64,
        op: &SignedOp,
        require_review_default: bool,
    ) -> Result<Vec<String>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "card.escalate")?;
        let args: EscalateArgs = serde_json::from_str(&op.arg)
            .map_err(|_| StoreError::Invalid("card escalate arg must be JSON"))?;
        let actor = op.actor_hex();
        let c = load_card(&tx, &op.subject)?;
        match c.owner.as_deref() {
            Some(owner) if owner == actor => need(role, Role::Worker)?,
            _ => need(role, Role::Supervisor)?,
        }
        let action = args.owner_action.trim().to_owned();
        if action.is_empty() {
            return Err(StoreError::Invalid("owner action must not be empty"));
        }
        if action.chars().count() > OWNER_ACTION_MAX {
            return Err(StoreError::Invalid(
                "owner action is longer than 1000 characters",
            ));
        }
        let targets = if args.to.is_empty() {
            let chain =
                effective_for(&tx, &c.team, &c.lane, require_review_default)?.escalation_chain;
            if chain.is_empty() {
                supervisors(&tx)?
            } else {
                unique(chain)
            }
        } else {
            if args.to.len() > ESCALATE_TARGETS_MAX {
                return Err(StoreError::Invalid("too many escalation targets"));
            }
            for key in &args.to {
                let pk = unhex32(key).ok_or(StoreError::Invalid(TARGET_MSG))?;
                if !is_active_person(&tx, &pk)? {
                    return Err(StoreError::Invalid(
                        "escalation target is not an active identity",
                    ));
                }
            }
            unique(args.to)
        };
        tx.execute(
            "UPDATE cards SET needs_owner=1, owner_action=? WHERE id=?",
            params![action, op.subject],
        )?;
        let text = format!(
            "card {} ({}) needs its owner: {}",
            op.subject, c.title, action
        );
        for target in &targets {
            post_notice(&tx, now, target, "escalation", &op.subject, &text)?;
        }
        Self::append(
            &tx,
            now,
            &actor,
            "card.escalated",
            &op.subject,
            &serde_json::json!({"owner_action": action, "notified": targets}),
        )?;
        tx.commit()?;
        Ok(targets)
    }

    /// The extension fields of each card, in the order given. An unknown id is `NotFound`.
    pub fn card_ext(&self, ids: &[String]) -> Result<Vec<CardExt>, StoreError> {
        ids.iter().map(|id| card_ext_of(&self.conn, id)).collect()
    }

    /// Board summary: cards needing an owner, WIP per (team, lane), job and lease counts,
    /// the open manual quiet window, and the admission pause.
    pub fn board_extras(&self, now: i64) -> Result<BoardExtras, StoreError> {
        let conn = &self.conn;
        Ok(BoardExtras {
            needs_owner: needs_owner_cards(conn)?,
            wip: wip_views(conn)?,
            running_jobs: count_u32(
                conn,
                "SELECT COUNT(*) FROM work_tasks WHERE state='running'",
            )?,
            waiting_jobs: count_u32(
                conn,
                "SELECT COUNT(*) FROM work_tasks WHERE state='claimed' AND requested IS NOT NULL AND started IS NULL",
            )?,
            queued_tasks: count_u32(conn, "SELECT COUNT(*) FROM work_tasks WHERE state='queued'")?,
            open_leases: count_u32_at(
                conn,
                "SELECT COUNT(*) FROM leases WHERE released IS NULL AND (expires IS NULL OR expires > ?)",
                now,
            )?,
            quiet: quiet_view(conn, now)?,
            paused: conn
                .query_row(
                    "SELECT value FROM work_state WHERE key='admission_paused'",
                    [],
                    |r| r.get(0),
                )
                .optional()?,
        })
    }
}

/// WIP check used by `Store::transition` when moving INTO claimed or doing from outside them:
/// the cards of the same team and lane in claimed or doing, excluding this one, must be below
/// the effective wip limit. Refused with a `Refused` message naming the lane, its count and its
/// limit.
pub(crate) fn check_wip(
    tx: &Transaction,
    card: &str,
    require_review_default: bool,
) -> Result<(), StoreError> {
    let (team, lane): (String, String) = tx
        .query_row("SELECT team, lane FROM cards WHERE id=?", [card], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
        .ok_or_else(|| StoreError::NotFound(card.into()))?;
    let Some(limit) = effective_for(tx, &team, &lane, require_review_default)?.wip_limit else {
        return Ok(());
    };
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM cards WHERE team=? AND lane=? AND state IN ('claimed','doing') AND id<>?",
        params![team, lane, card],
        |r| r.get(0),
    )?;
    if count >= i64::from(limit) {
        return Err(StoreError::Refused(format!(
            "wip limit reached: lane {team}/{lane} has {count} of {limit}"
        )));
    }
    Ok(())
}

/// The columns of one card, as stored.
struct CardFields {
    title: String,
    lane: String,
    state: String,
    owner: Option<String>,
    priority: i64,
    value: f64,
    size: f64,
    team: String,
    needs_owner: bool,
    owner_action: String,
}

impl CardFields {
    fn state(&self) -> Result<CardState, StoreError> {
        CardState::parse(&self.state).ok_or_else(|| StoreError::BadState(self.state.clone()))
    }
}

fn load_card(conn: &Connection, id: &str) -> Result<CardFields, StoreError> {
    conn.query_row(
        "SELECT title, lane, state, owner, priority, value, size, team, needs_owner, owner_action \
         FROM cards WHERE id=?",
        [id],
        |r| {
            Ok(CardFields {
                title: r.get(0)?,
                lane: r.get(1)?,
                state: r.get(2)?,
                owner: r.get(3)?,
                priority: r.get(4)?,
                value: r.get(5)?,
                size: r.get(6)?,
                team: r.get(7)?,
                needs_owner: r.get::<_, i64>(8)? != 0,
                owner_action: r.get(9)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| StoreError::NotFound(id.into()))
}

/// Who may edit a card's fields, dependencies and links: its owner, any Worker for an
/// unowned card, and a Supervisor for a card someone else owns.
fn check_edit(role: Role, owner: Option<&str>, actor: &str) -> Result<(), StoreError> {
    match owner {
        Some(o) if o != actor => need(role, Role::Supervisor),
        _ => need(role, Role::Worker),
    }
}

fn card_ext_of(conn: &Connection, id: &str) -> Result<CardExt, StoreError> {
    let c = load_card(conn, id)?;
    let after = column_strings(
        conn,
        "SELECT after FROM card_deps WHERE card=? ORDER BY after",
        id,
    )?;
    let jobs = column_strings(
        conn,
        "SELECT id FROM work_tasks WHERE card=? ORDER BY seq",
        id,
    )?;
    let mut st =
        conn.prepare("SELECT rel, target FROM card_links WHERE card=? ORDER BY rel, target")?;
    let rows = st.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut links = Vec::new();
    for row in rows {
        let (rel, target) = row?;
        let rel =
            LinkRel::parse(&rel).ok_or(StoreError::Invalid("corrupt card link in database"))?;
        links.push(CardLink { rel, target });
    }
    Ok(CardExt {
        id: id.into(),
        priority: c.priority,
        value: c.value,
        size: c.size,
        team: c.team,
        needs_owner: c.needs_owner,
        owner_action: c.owner_action,
        after,
        links,
        jobs,
    })
}

fn column_strings(conn: &Connection, sql: &str, id: &str) -> Result<Vec<String>, StoreError> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([id], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<String>, _>>()?)
}

/// The effective policy for a team and lane: the four layers of `policies` (`*`/`*`, team/`*`,
/// `*`/lane, team/lane), most general first, combined by `agora_sched::policy::effective`.
fn effective_for(
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
fn is_active_person(conn: &Connection, pk: &[u8]) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM identities WHERE pubkey=? AND revoked=0 AND system=0)",
        [pk],
        |r| r.get(0),
    )?)
}

/// Hex keys of the active, non-system identities with role Supervisor or higher, sorted.
fn supervisors(conn: &Connection) -> Result<Vec<String>, StoreError> {
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

/// Fails with the cycle message unless the card dependency graph is acyclic. Self and unknown
/// prerequisites cannot be stored (checked on insert, and by the foreign key), so any error
/// from the graph is a cycle.
fn check_acyclic(conn: &Connection) -> Result<(), StoreError> {
    let mut st = conn.prepare("SELECT card, after FROM card_deps")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut after: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (card, prerequisite) = row?;
        after.entry(prerequisite.clone()).or_default();
        after.entry(card).or_default().push(prerequisite);
    }
    let tasks: Vec<Task> = after
        .into_iter()
        .enumerate()
        .map(|(index, (id, after))| Task {
            index,
            id,
            title: String::new(),
            kind: "other".into(),
            tier: tier_of("other"),
            queued: None,
            after,
            size: 1.0,
            value: 1.0,
            files: Vec::new(),
        })
        .collect();
    Graph::new(tasks)
        .map(|_| ())
        .map_err(|_| StoreError::Invalid(CYCLE))
}
