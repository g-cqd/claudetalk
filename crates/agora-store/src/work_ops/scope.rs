//! `work.scope`: create or update a scope (ref-queue-cli section 1.2).

use super::checks::{TEXT_MAX, check_text_field, detail_text, refusal, refuse};
use crate::{Store, StoreError};
use agora_sched::claims;
use agora_sched::model_ops::ScopeArgs;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;
use std::collections::HashSet;

/// Creates or updates the scope named by `args`. Omitted fields keep their stored value; `parent: ""` clears it.
pub(super) fn set_in(
    tx: &Transaction,
    now: i64,
    actor: &str,
    args: &ScopeArgs,
) -> Result<(), StoreError> {
    refuse(claims::validate_name("scope", &args.id))?;
    let existing: Option<(String, Option<String>, String)> = tx
        .query_row(
            "SELECT kind, parent, task FROM work_scopes WHERE id=?",
            [&args.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let parent = match args.parent.as_deref() {
        None => existing.as_ref().and_then(|e| e.1.clone()),
        Some("") => None,
        Some(parent) => {
            refuse(claims::validate_name("parent scope", parent))?;
            check_parent(tx, &args.id, parent)?;
            Some(parent.to_string())
        }
    };
    let kind = match (args.kind, &existing) {
        (Some(kind), _) => kind.as_str().to_string(),
        (None, Some(stored)) => stored.0.clone(),
        (None, None) if parent.is_some() => "agent".to_string(),
        (None, None) => "project".to_string(),
    };
    let task = match args.task.as_deref() {
        Some(text) => {
            check_text_field("task", text, TEXT_MAX)?;
            text.to_string()
        }
        None => existing.as_ref().map(|e| e.2.clone()).unwrap_or_default(),
    };
    // The scope keeps the agent that first set it up; an update by someone else does not take it over.
    if existing.is_some() {
        tx.execute(
            "UPDATE work_scopes SET kind=?1, parent=?2, task=?3, agent=COALESCE(agent, ?4), updated=?5 WHERE id=?6",
            params![kind, parent, task, actor, now, args.id],
        )?;
    } else {
        tx.execute(
            "INSERT INTO work_scopes(id, kind, parent, task, agent, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![args.id, kind, parent, task, actor, now],
        )?;
    }
    let detail = detail_text(&task, &format!("kind {kind}"));
    Store::append(
        tx,
        now,
        actor,
        "work.scoped",
        &args.id,
        &json!({"kind": kind, "parent": parent, "scope": args.id, "detail": detail}),
    )?;
    Ok(())
}

/// Refuses an unknown parent, or one that is `id` or under it (a cycle). Walks up the chain once per scope.
fn check_parent(tx: &Transaction, id: &str, parent: &str) -> Result<(), StoreError> {
    let mut current = Some(parent.to_string());
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(scope) = current.take() {
        if scope == id {
            return Err(refusal(format!(
                "scope {id} cannot be under {parent}, which is {id} or under it"
            )));
        }
        if !seen.insert(scope.clone()) {
            break;
        }
        let row: Option<Option<String>> = tx
            .query_row("SELECT parent FROM work_scopes WHERE id=?", [&scope], |r| {
                r.get(0)
            })
            .optional()?;
        match row {
            None => return Err(refusal(format!("unknown parent scope {scope}"))),
            Some(next) => current = next,
        }
    }
    Ok(())
}
