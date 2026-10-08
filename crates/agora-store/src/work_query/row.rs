//! The shared row mapping of `work_tasks`: [`TASK_COLUMNS`] lists the columns and [`task_from_row`] maps a row
//! selected with them. Other store modules read tasks through these two items, so a task is always mapped the same way.

use agora_sched::claims;
use agora_sched::model::{TaskRecord, TaskState};
use agora_sched::predict::Prediction;
use agora_sched::task::tier_of;
use rusqlite::Row;
use rusqlite::types::Type;
use std::error::Error;

/// The `work_tasks` columns, in the order [`task_from_row`] reads them. A SELECT that feeds `task_from_row` lists
/// exactly these, in this order.
pub(crate) const TASK_COLUMNS: &str = "seq, id, title, kind, scope, author, queued, after, size, value, files, \
     command, cwd, weight, state, agent, accepted, claimed, requested, started, ended, status, pid, supervisor_pid, \
     log, qos, cpu, rss, rss_peak, cpu_time, sampled, note, prediction, card, origin, exclusive, quiet_until, \
     cancel_requested";

/// Maps one row selected with [`TASK_COLUMNS`] to a [`TaskRecord`]. Derived fields: `tier` from the kind, the
/// pseudonyms of the author and the claimant, `needs_acceptance`, and the D5 `digest` of a command. JSON columns that
/// do not parse are a conversion error, never a panic.
pub(crate) fn task_from_row(row: &Row) -> rusqlite::Result<TaskRecord> {
    let kind: String = row.get(3)?;
    let author: String = row.get(5)?;
    let after = string_list(7, &row.get::<_, String>(7)?)?;
    let files = string_list(10, &row.get::<_, String>(10)?)?;
    let command = row
        .get::<_, Option<String>>(11)?
        .map(|text| string_list(11, &text))
        .transpose()?;
    let state_text: String = row.get(14)?;
    let state = TaskState::parse(&state_text)
        .ok_or_else(|| column_error(14, format!("unknown task state {state_text:?}")))?;
    let prediction = row
        .get::<_, Option<String>>(32)?
        .map(|text| serde_json::from_str::<Prediction>(&text).map_err(|e| column_error(32, e)))
        .transpose()?;
    let agent: Option<String> = row.get(15)?;
    let accepted: Option<String> = row.get(16)?;
    let cwd: Option<String> = row.get(12)?;
    let digest = command
        .as_ref()
        .map(|argv| claims::digest(argv, cwd.as_deref().unwrap_or(""), &files));
    let needs_acceptance = command.is_some() && agent.is_some() && accepted != agent;
    Ok(TaskRecord {
        seq: row.get(0)?,
        id: row.get(1)?,
        title: row.get(2)?,
        tier: tier_of(&kind),
        scope: row.get(4)?,
        author_name: pseudonym_of(&author),
        author,
        state,
        agent_name: agent.as_deref().map(pseudonym_of),
        agent,
        accepted_by: accepted,
        needs_acceptance,
        after,
        size: row.get(8)?,
        value: row.get(9)?,
        files,
        weight: row.get(13)?,
        command,
        cwd,
        digest,
        queued_ms: row.get(6)?,
        claimed_ms: row.get(17)?,
        requested_ms: row.get(18)?,
        started_ms: row.get(19)?,
        ended_ms: row.get(20)?,
        status: row.get(21)?,
        pid: row.get(22)?,
        supervisor_pid: row.get(23)?,
        log: row.get(24)?,
        qos: row.get(25)?,
        prediction,
        cpu_percent: row.get(26)?,
        rss_kib: row.get(27)?,
        rss_peak_kib: row.get(28)?,
        cpu_seconds: row.get(29)?,
        sampled_ms: row.get(30)?,
        note: row.get(31)?,
        card: row.get(33)?,
        origin: row.get(34)?,
        exclusive: row.get(35)?,
        quiet_until_ms: row.get(36)?,
        cancel_requested: row.get(37)?,
        kind,
    })
}

/// The display pseudonym of a stored hex public key. A value that is not a key is shown unchanged.
pub(super) fn pseudonym_of(public_hex: &str) -> String {
    crate::unhex32(public_hex).map_or_else(
        || public_hex.to_string(),
        |key| agora_crypto::pseudonym(&key),
    )
}

pub(super) fn column_error(
    column: usize,
    e: impl Into<Box<dyn Error + Send + Sync>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, e.into())
}

pub(super) fn string_list(column: usize, text: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(text).map_err(|e| column_error(column, e))
}
