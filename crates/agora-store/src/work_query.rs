//! Read-only queries over the work queue.
//!
//! Every read goes through [`task_from_row`] and [`TASK_COLUMNS`], so a task is mapped the same way wherever it is
//! read. Scope filters follow work.py `in_scope`: a scope means itself and its descendants, and a task without a scope
//! is hidden by any scope filter.

mod row;

use crate::{Store, StoreError};
use agora_sched::graph::Graph;
use agora_sched::model::{
    Counts, ScopeKind, ScopeRecord, TaskFilter, TaskRecord, TaskState, WorkEvent,
};
use agora_sched::order::OrderPolicy;
use agora_sched::predict::{History, Run};
use agora_sched::pyfmt::printable;
use agora_sched::render::{Mode, render};
use agora_sched::settings::WorkSettings;
use agora_sched::signature::Signatures;
use agora_sched::task::{Queued, Task};
use row::{pseudonym_of, string_list};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, named_params, params_from_iter};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) use row::{TASK_COLUMNS, task_from_row};

const SIGNATURE_COLUMNS: [&str; 4] = ["sig_full", "sig_head", "sig_prog", "sig_tool"];
const LEVEL_LIMIT: u32 = 5;
const RECENT_LIMIT: u32 = 200;
const EXCLUSIVE_LIMIT: u32 = 20;

/// The scope rows of `work_scopes`, before the kind is parsed and the counts are attached.
type ScopeRow = (String, String, Option<String>, String, Option<String>, i64);

impl Store {
    /// One task; `NotFound("task <id>")`.
    pub fn work_task(&self, id: &str) -> Result<TaskRecord, StoreError> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM work_tasks WHERE id = ?1");
        self.conn
            .query_row(&sql, [id], task_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("task {id}")))
    }

    /// Tasks matching `f`, in arrival order (seq). `scope` covers the scope and its descendants and hides tasks
    /// without a scope; empty `states` or `ids` do not filter; `limit` 0 means no limit.
    pub fn work_tasks(&self, f: &TaskFilter) -> Result<Vec<TaskRecord>, StoreError> {
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        if let Some(scope) = &f.scope {
            clauses.push(format!("scope IN ({})", subtree("?")));
            values.push(Value::Text(scope.clone()));
        }
        if !f.states.is_empty() {
            clauses.push(format!("state IN ({})", placeholders(f.states.len())));
            values.extend(
                f.states
                    .iter()
                    .map(|state| Value::Text(state.as_str().to_string())),
            );
        }
        if !f.ids.is_empty() {
            clauses.push(format!("id IN ({})", placeholders(f.ids.len())));
            values.extend(f.ids.iter().map(|id| Value::Text(id.clone())));
        }
        let mut sql = format!("SELECT {TASK_COLUMNS} FROM work_tasks");
        if !clauses.is_empty() {
            sql = format!("{sql} WHERE {}", clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY seq");
        if f.limit > 0 {
            sql.push_str(" LIMIT ?");
            values.push(Value::Integer(i64::from(f.limit)));
        }
        select_tasks(&self.conn, &sql, values)
    }

    /// Scopes under `root` (all when `None`), oldest first. `counts` holds the scope's own non-final tasks per state;
    /// an unknown root gives an empty list.
    pub fn work_scopes(&self, root: Option<&str>) -> Result<Vec<ScopeRecord>, StoreError> {
        let sql = format!(
            "SELECT id, kind, parent, task, agent, updated FROM work_scopes \
             WHERE :root IS NULL OR id IN ({}) ORDER BY created, id",
            subtree(":root")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(named_params! {":root": root}, scope_row)?;
        let raw = rows.collect::<rusqlite::Result<Vec<ScopeRow>>>()?;
        let mut counts = scope_counts(&self.conn)?;
        raw.into_iter()
            .map(|(id, kind, parent, task, agent, updated_ms)| {
                let kind =
                    ScopeKind::parse(&kind).ok_or_else(|| StoreError::BadState(kind.clone()))?;
                let own_counts = counts.remove(&id).unwrap_or_default();
                Ok(ScopeRecord {
                    id,
                    kind,
                    parent,
                    task,
                    agent,
                    updated_ms,
                    counts: own_counts,
                })
            })
            .collect()
    }

    /// Task counts per state in `scope` (all when `None`). `ready` counts the queued tasks whose prerequisites are all
    /// done; a prerequisite that does not exist is not done.
    pub fn work_counts(&self, scope: Option<&str>) -> Result<Counts, StoreError> {
        let mut counts = Counts::default();
        let by_state = format!(
            "SELECT state, COUNT(*) FROM work_tasks WHERE {} GROUP BY state",
            in_scope("scope")
        );
        let mut stmt = self.conn.prepare(&by_state)?;
        let rows = stmt.query_map(named_params! {":scope": scope}, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (state, n) = row?;
            match TaskState::parse(&state) {
                Some(TaskState::Queued) => counts.queued = n,
                Some(TaskState::Claimed) => counts.claimed = n,
                Some(TaskState::Running) => counts.running = n,
                Some(TaskState::Done) => counts.done = n,
                Some(TaskState::Failed) => counts.failed = n,
                Some(TaskState::Cancelled) => counts.cancelled = n,
                None => return Err(StoreError::BadState(state)),
            }
        }
        let ready = format!(
            "SELECT COUNT(*) FROM work_tasks t WHERE t.state = 'queued' AND {} \
             AND NOT EXISTS (SELECT 1 FROM json_each(t.after) p \
                 WHERE p.value NOT IN (SELECT d.id FROM work_tasks d WHERE d.state = 'done'))",
            in_scope("t.scope")
        );
        counts.ready = self
            .conn
            .query_row(&ready, named_params! {":scope": scope}, |row| {
                row.get::<_, u64>(0)
            })?;
        Ok(counts)
    }

    /// The last `limit` work and job events (kinds `work.%` and `job.%`), oldest first. `task` matches the subject,
    /// `actor` the hex public key exactly, `scope` the event's scope (its `scope` datum, else its task's or scope's).
    pub fn work_log(
        &self,
        task: Option<&str>,
        actor: Option<&str>,
        scope: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkEvent>, StoreError> {
        let sql = format!(
            "SELECT seq, ts, kind, actor, task, scope, detail FROM (\
                SELECT e.seq, e.ts, e.kind, e.actor, \
                    CASE WHEN t.id IS NOT NULL THEN e.subject END AS task, \
                    COALESCE(CAST(json_extract(e.data, '$.scope') AS TEXT), t.scope, w.id) AS scope, \
                    COALESCE(CAST(json_extract(e.data, '$.detail') AS TEXT), '') AS detail \
                FROM events e \
                LEFT JOIN work_tasks t ON t.id = e.subject \
                LEFT JOIN work_scopes w ON w.id = e.subject \
                WHERE (e.kind LIKE 'work.%' OR e.kind LIKE 'job.%') \
                    AND (:task IS NULL OR e.subject = :task) \
                    AND (:actor IS NULL OR e.actor = :actor)\
             ) WHERE {} ORDER BY seq DESC LIMIT {limit}",
            in_scope("scope")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            named_params! {":task": task, ":actor": actor, ":scope": scope},
            |row| {
                let actor: String = row.get(3)?;
                Ok(WorkEvent {
                    seq: row.get(0)?,
                    ts_ms: row.get(1)?,
                    kind: row.get(2)?,
                    actor_name: pseudonym_of(&actor),
                    actor,
                    task: row.get(4)?,
                    scope: row.get(5)?,
                    detail: row.get(6)?,
                })
            },
        )?;
        let mut events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// The stored settings (`work_settings` row 1), or the defaults when no row exists.
    pub fn work_settings(&self) -> Result<WorkSettings, StoreError> {
        crate::settings_read::settings_in(&self.conn)
    }

    /// History for prediction (ref-admission section 2; tie-break ended DESC, seq DESC).
    pub fn work_history(&self, sigs: &Signatures, cpus: f64) -> Result<History, StoreError> {
        let keys = [&sigs.full, &sigs.head, &sigs.prog, &sigs.tool];
        let mut by_level: [Vec<Run>; 4] = Default::default();
        for ((column, key), runs) in SIGNATURE_COLUMNS.iter().zip(keys).zip(by_level.iter_mut()) {
            *runs = ran_runs(
                &self.conn,
                &format!("{column} = ?"),
                vec![Value::Text(key.clone())],
                LEVEL_LIMIT,
            )?;
        }
        let recent_durations_s = ran_runs(&self.conn, "1 = 1", Vec::new(), RECENT_LIMIT)?
            .iter()
            .map(run_seconds)
            .collect();
        let exclusive_runs = ran_runs(
            &self.conn,
            "weight >= ?",
            vec![Value::Real(cpus)],
            EXCLUSIVE_LIMIT,
        )?;
        Ok(History {
            by_level,
            recent_durations_s,
            exclusive_runs,
        })
    }

    /// Queued tasks of the scope as an agora_sched Graph (queued_ms as `Queued::Int`) -> (json, text) via render.
    /// Prerequisites that are not queued are dropped from the ordering, as work.py does: they are either done (no
    /// wait) or blocking, which `next` reports. A cycle among the queued tasks is `Refused`, with the cycle path in the message.
    pub fn work_plan(
        &self,
        scope: Option<&str>,
        policy: OrderPolicy,
        mode: &Mode,
    ) -> Result<(String, String), StoreError> {
        let queued = self.work_tasks(&TaskFilter {
            scope: scope.map(str::to_string),
            states: vec![TaskState::Queued],
            ids: Vec::new(),
            limit: 0,
        })?;
        let queued_ids: HashSet<&str> = queued.iter().map(|task| task.id.as_str()).collect();
        let tasks: Vec<Task> = queued
            .iter()
            .enumerate()
            .map(|(index, task)| Task {
                index,
                id: task.id.clone(),
                title: printable(&task.title),
                kind: task.kind.clone(),
                tier: task.tier,
                queued: Some(Queued::Int(task.queued_ms)),
                after: task
                    .after
                    .iter()
                    .filter(|id| queued_ids.contains(id.as_str()))
                    .cloned()
                    .collect(),
                size: task.size,
                value: task.value,
                files: task.files.clone(),
            })
            .collect();
        let graph = Graph::new(tasks)
            .map_err(|e| StoreError::Refused(format!("the queue cannot be ordered: {}", e.0)))?;
        Ok((
            render(&graph, policy, mode, true),
            render(&graph, policy, mode, false),
        ))
    }

    /// (task id, files) of claimed+running tasks with files, then the active path leases as ("lease:<id>", [path]).
    /// A lease is active when unreleased and unexpired at `now`; `exclude_holder` drops that holder's leases only.
    pub fn work_holders(
        &self,
        now: i64,
        exclude_holder: Option<&str>,
    ) -> Result<Vec<(String, Vec<String>)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, files FROM work_tasks WHERE state IN ('claimed', 'running') ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut holders = Vec::new();
        for row in rows {
            let (id, files_text) = row?;
            let files = string_list(1, &files_text)?;
            if !files.is_empty() {
                holders.push((id, files));
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, resource FROM leases \
             WHERE kind = 'path' AND released IS NULL AND (expires IS NULL OR expires > :now) \
               AND (:exclude IS NULL OR holder <> :exclude) \
             ORDER BY created, id",
        )?;
        let leases = stmt.query_map(
            named_params! {":now": now, ":exclude": exclude_holder},
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for lease in leases {
            let (id, resource) = lease?;
            holders.push((format!("lease:{id}"), vec![resource]));
        }
        Ok(holders)
    }

    /// Running tasks, by start time then seq.
    pub fn work_running(&self) -> Result<Vec<TaskRecord>, StoreError> {
        select_tasks(
            &self.conn,
            &format!(
                "SELECT {TASK_COLUMNS} FROM work_tasks WHERE state = 'running' ORDER BY started, seq"
            ),
            Vec::new(),
        )
    }

    /// Claimed AND requested AND not started, by requested, seq.
    pub fn work_waiting(&self) -> Result<Vec<TaskRecord>, StoreError> {
        select_tasks(
            &self.conn,
            &format!(
                "SELECT {TASK_COLUMNS} FROM work_tasks \
                 WHERE state = 'claimed' AND requested IS NOT NULL AND started IS NULL ORDER BY requested, seq"
            ),
            Vec::new(),
        )
    }

    /// Every (task id, close time ms) with quiet_until set: min(quiet_until, coalesce(ended, quiet_until)).
    pub fn work_window_closes(&self) -> Result<Vec<(String, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, MIN(quiet_until, COALESCE(ended, quiet_until)) FROM work_tasks \
             WHERE quiet_until IS NOT NULL ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let closes = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(closes)
    }
}

/// The scope `param` and its descendants, following `parent` links. UNION (not UNION ALL) ends a cycle.
fn subtree(param: &str) -> String {
    format!(
        "WITH RECURSIVE sub(id) AS (SELECT id FROM work_scopes WHERE id = {param} \
         UNION SELECT s.id FROM work_scopes s JOIN sub ON s.parent = sub.id) SELECT id FROM sub"
    )
}

/// work.py `in_scope` over the named parameter `:scope`: no scope keeps every row, a scope keeps its subtree. A NULL
/// column is not in any subtree, so a scope filter hides tasks without a scope.
fn in_scope(column: &str) -> String {
    format!("(:scope IS NULL OR {column} IN ({}))", subtree(":scope"))
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

fn select_tasks(
    conn: &Connection,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<TaskRecord>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(values), task_from_row)?;
    let tasks = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tasks)
}

fn scope_row(row: &Row) -> rusqlite::Result<ScopeRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

/// Non-final task counts per scope and state, for the scopes that have any.
fn scope_counts(conn: &Connection) -> Result<HashMap<String, BTreeMap<String, u64>>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT scope, state, COUNT(*) FROM work_tasks \
         WHERE scope IS NOT NULL AND state IN ('queued', 'claimed', 'running') GROUP BY scope, state",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
        ))
    })?;
    let mut by_scope: HashMap<String, BTreeMap<String, u64>> = HashMap::new();
    for row in rows {
        let (scope, state, n) = row?;
        by_scope.entry(scope).or_default().insert(state, n);
    }
    Ok(by_scope)
}

/// RAN rows of `job_history` (ref-admission section 2): a real exit status other than 126 or 127, a start, an end and
/// a CPU time. `filter` is an SQL condition whose `?` placeholders take `values`.
fn ran_runs(
    conn: &Connection,
    filter: &str,
    values: Vec<Value>,
    limit: u32,
) -> Result<Vec<Run>, StoreError> {
    let sql = format!(
        "SELECT started, ended, cpu_time, rss_peak FROM job_history \
         WHERE status IS NOT NULL AND status NOT IN (126, 127) AND started IS NOT NULL \
           AND ended IS NOT NULL AND cpu_time IS NOT NULL AND ({filter}) \
         ORDER BY ended DESC, seq DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(Run {
            started_ms: row.get(0)?,
            ended_ms: row.get(1)?,
            cpu_seconds: row.get(2)?,
            rss_peak_kib: row.get(3)?,
        })
    })?;
    let runs = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(runs)
}

fn run_seconds(run: &Run) -> f64 {
    run.ended_ms.saturating_sub(run.started_ms) as f64 / 1000.0
}

#[cfg(test)]
mod tests;
