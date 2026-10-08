//! Sprints: planned, active and closed groupings of cards.

use crate::identity::{authorize, need};
use crate::{SignedOp, Store, StoreError};
use agora_core::Role;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub struct SprintSummary {
    pub total: i64,
    pub done: i64,
    pub in_flight: i64,
    pub blocked: i64,
}

impl Store {
    /// kind "sprint.create", subject = sprint id; need Supervisor.
    pub fn create_sprint(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "sprint.create")?;
        need(role, Role::Supervisor)?;
        require_sprint_id(op)?;
        let (name, goal, starts, ends) = parse_create_arg(&op.arg)?;
        if ends <= starts {
            return Err(StoreError::Invalid("sprint must end after it starts"));
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sprints WHERE id=?)",
            [op.subject.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            return Err(StoreError::Invalid("sprint already exists"));
        }
        tx.execute(
            "INSERT INTO sprints(id,name,goal,starts,ends) VALUES (?,?,?,?,?)",
            params![op.subject, name, goal, starts, ends],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "sprint.created",
            &op.subject,
            &serde_json::json!({"name": name, "goal": goal, "starts": starts, "ends": ends}),
        )?;
        Ok(tx.commit()?)
    }

    /// kind "sprint.add", subject = sprint id, arg = card id; need Supervisor.
    pub fn add_to_sprint(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "sprint.add")?;
        need(role, Role::Supervisor)?;
        require_sprint_id(op)?;
        if op.arg.is_empty() {
            return Err(StoreError::Invalid("card id must not be empty"));
        }
        ensure_card(&tx, &op.arg)?;
        if sprint_state(&tx, &op.subject)? == "closed" {
            return Err(StoreError::Invalid("sprint is closed"));
        }
        // A card already in the sprint is a no-op: the nonce is still consumed
        // (the commit below), but no event is appended.
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO sprint_cards(sprint,card) VALUES (?,?)",
            params![op.subject, op.arg],
        )?;
        if inserted > 0 {
            Self::append(
                &tx,
                now,
                &op.actor_hex(),
                "sprint.card_added",
                &op.subject,
                &serde_json::json!({"card": op.arg}),
            )?;
        }
        Ok(tx.commit()?)
    }

    /// kind "sprint.start", subject = sprint id; need Supervisor.
    /// Only one sprint may be active at a time.
    pub fn start_sprint(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "sprint.start")?;
        need(role, Role::Supervisor)?;
        require_sprint_id(op)?;
        if sprint_state(&tx, &op.subject)? != "planned" {
            return Err(StoreError::Invalid("sprint is not planned"));
        }
        let another_active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sprints WHERE state='active')",
            [],
            |r| r.get(0),
        )?;
        if another_active {
            return Err(StoreError::Invalid("another sprint is already active"));
        }
        tx.execute(
            "UPDATE sprints SET state='active' WHERE id=?",
            [op.subject.as_str()],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "sprint.started",
            &op.subject,
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    /// kind "sprint.close", subject = sprint id; need Supervisor.
    pub fn close_sprint(&mut self, now: i64, op: &SignedOp) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role = authorize(&tx, now, op, "sprint.close")?;
        need(role, Role::Supervisor)?;
        require_sprint_id(op)?;
        if sprint_state(&tx, &op.subject)? != "active" {
            return Err(StoreError::Invalid("sprint is not active"));
        }
        tx.execute(
            "UPDATE sprints SET state='closed' WHERE id=?",
            [op.subject.as_str()],
        )?;
        Self::append(
            &tx,
            now,
            &op.actor_hex(),
            "sprint.closed",
            &op.subject,
            &serde_json::json!({}),
        )?;
        Ok(tx.commit()?)
    }

    pub fn sprint_summary(&self, sprint: &str) -> Result<SprintSummary, StoreError> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.state, COUNT(*) FROM sprint_cards s JOIN cards c ON c.id=s.card WHERE s.sprint=? GROUP BY c.state",
        )?;
        let mut out = SprintSummary::default();
        let rows = st.query_map([sprint], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, n) = row?;
            out.total += n;
            match state.as_str() {
                "done" => out.done += n,
                "blocked" => out.blocked += n,
                "claimed" | "doing" | "review" => out.in_flight += n,
                _ => {}
            }
        }
        Ok(out)
    }
}

fn require_sprint_id(op: &SignedOp) -> Result<(), StoreError> {
    if op.subject.is_empty() {
        return Err(StoreError::Invalid("sprint id must not be empty"));
    }
    Ok(())
}

/// Parses `{"name": str, "goal"?: str, "starts": i64, "ends": i64}`.
fn parse_create_arg(arg: &str) -> Result<(String, String, i64, i64), StoreError> {
    let v: serde_json::Value =
        serde_json::from_str(arg).map_err(|_| StoreError::Invalid("sprint arg must be JSON"))?;
    let name = v
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(StoreError::Invalid(
            "sprint name must be a non-empty string",
        ))?;
    let goal = match v.get("goal") {
        None => "",
        Some(g) => g
            .as_str()
            .ok_or(StoreError::Invalid("sprint goal must be a string"))?,
    };
    let starts = v
        .get("starts")
        .and_then(serde_json::Value::as_i64)
        .ok_or(StoreError::Invalid("sprint starts must be an integer"))?;
    let ends = v
        .get("ends")
        .and_then(serde_json::Value::as_i64)
        .ok_or(StoreError::Invalid("sprint ends must be an integer"))?;
    Ok((name.to_owned(), goal.to_owned(), starts, ends))
}

/// State of an existing sprint. Runs inside the caller's transaction.
fn sprint_state(conn: &Connection, id: &str) -> Result<String, StoreError> {
    let state: Option<String> = conn
        .query_row("SELECT state FROM sprints WHERE id=?", [id], |r| r.get(0))
        .optional()?;
    state.ok_or_else(|| StoreError::NotFound(id.to_owned()))
}

/// Fails with `NotFound` unless the card exists. Runs inside the caller's transaction.
fn ensure_card(conn: &Connection, id: &str) -> Result<(), StoreError> {
    let exists: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM cards WHERE id=?)", [id], |r| {
            r.get(0)
        })?;
    if !exists {
        return Err(StoreError::NotFound(id.to_owned()));
    }
    Ok(())
}
