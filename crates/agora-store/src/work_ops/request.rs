//! Run requests: placement signatures from the real filesystem, the run history and the prediction, and the
//! reservation that marks a claimed task as waiting for capacity (ref-admission section 2, ref-queue-cli section 4).

use super::checks::event;
use crate::StoreError;
use agora_sched::model::TaskRecord;
use agora_sched::predict::{History, Run, predict};
use agora_sched::signature::{PlaceProbe, Signatures, place, signatures};
use agora_sched::textfmt::number;
use rusqlite::{Transaction, params};
use serde_json::json;
use std::io::Read;
use std::path::Path;

/// The real filesystem as the placement probe sees it. Reads only the first `n` bytes of a regular file.
struct FsProbe;

impl PlaceProbe for FsProbe {
    fn is_dir(&self, p: &str) -> bool {
        Path::new(p).is_dir()
    }

    fn is_file(&self, p: &str) -> bool {
        Path::new(p).is_file()
    }

    fn read_head(&self, p: &str, n: usize) -> Option<String> {
        // Only regular files: opening a FIFO or a device could block.
        if !Path::new(p).is_file() {
            return None;
        }
        let file = std::fs::File::open(p).ok()?;
        let mut head = Vec::new();
        Read::take(file, u64::try_from(n).unwrap_or(u64::MAX))
            .read_to_end(&mut head)
            .ok()?;
        Some(String::from_utf8_lossy(&head).into_owned())
    }
}

/// Scratch roots of work.py `place`, plus the canonical temporary directory of this process.
fn scratch_roots() -> Vec<String> {
    let mut roots: Vec<String> = [
        "/tmp",
        "/private/tmp",
        "/var/folders",
        "/private/var/folders",
    ]
    .iter()
    .map(|root| root.to_string())
    .collect();
    if let Ok(real) = std::fs::canonicalize(std::env::temp_dir())
        && let Some(text) = real.to_str()
    {
        roots.push(text.to_string());
    }
    roots
}

/// The four signatures of a command run in `cwd`.
pub(super) fn signatures_for(argv: &[String], cwd: Option<&str>) -> Signatures {
    let placed = place(cwd, &FsProbe, &scratch_roots());
    signatures(argv, &placed)
}

/// Columns of `job_history` for the four signature levels, in `History::by_level` order.
const LEVELS: [&str; 4] = ["sig_full", "sig_head", "sig_prog", "sig_tool"];

/// A run that taught something: it reached a real exit status and its CPU time was recorded.
const RAN: &str = "status IS NOT NULL AND status NOT IN (126,127) AND cpu_time IS NOT NULL";

fn runs(
    tx: &Transaction,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Run>, StoreError> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok(Run {
            started_ms: r.get(0)?,
            ended_ms: r.get(1)?,
            cpu_seconds: r.get(2)?,
            rss_peak_kib: r
                .get::<_, Option<i64>>(3)?
                .and_then(|kib| u64::try_from(kib).ok()),
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// History for the prediction, read inside the caller's transaction (ref-admission section 2): up to 5 newest RAN
/// runs per signature level, the 200 newest RAN durations of any command, and the 20 newest RAN runs at least
/// `cpus` heavy.
pub(super) fn history_in(
    tx: &Transaction,
    sigs: &Signatures,
    cpus: f64,
) -> Result<History, StoreError> {
    let keys = [&sigs.full, &sigs.head, &sigs.prog, &sigs.tool];
    let mut history = History::default();
    for (slot, (column, key)) in LEVELS.iter().zip(keys).enumerate() {
        let sql = format!(
            "SELECT started, ended, cpu_time, rss_peak FROM job_history WHERE {column}=?1 AND {RAN} \
             ORDER BY ended DESC, seq DESC LIMIT 5"
        );
        let found = runs(tx, &sql, [key.as_str()])?;
        if let Some(bucket) = history.by_level.get_mut(slot) {
            *bucket = found;
        }
    }
    let mut durations = tx.prepare(&format!(
        "SELECT started, ended FROM job_history WHERE {RAN} ORDER BY ended DESC, seq DESC LIMIT 200"
    ))?;
    let rows = durations.query_map([], |r| {
        let started: i64 = r.get(0)?;
        let ended: i64 = r.get(1)?;
        Ok(ended.saturating_sub(started) as f64 / 1000.0)
    })?;
    history.recent_durations_s = rows.collect::<Result<Vec<_>, _>>()?;
    history.exclusive_runs = runs(
        tx,
        &format!(
            "SELECT started, ended, cpu_time, rss_peak FROM job_history WHERE {RAN} AND weight >= ?1 \
             ORDER BY ended DESC, seq DESC LIMIT 20"
        ),
        [cpus],
    )?;
    Ok(history)
}

/// Marks a claimed, accepted task as requested: computes its signatures and prediction, stores the log path,
/// clears the note and records the event. The daemon starts the job from there.
pub(super) fn reserve(
    tx: &Transaction,
    now: i64,
    task: &TaskRecord,
    actor: &str,
    cpus: f64,
    log_dir: &str,
) -> Result<(), StoreError> {
    let argv = task
        .command
        .as_ref()
        .ok_or(StoreError::Invalid("a run needs a command"))?;
    let sigs = signatures_for(argv, task.cwd.as_deref());
    let history = history_in(tx, &sigs, cpus)?;
    let prediction = predict(&history, task.weight, cpus);
    let prediction_json = serde_json::to_string(&prediction)
        .map_err(|_| StoreError::Invalid("prediction could not be stored"))?;
    let log = Path::new(log_dir)
        .join(format!("{}.log", task.id))
        .to_string_lossy()
        .into_owned();
    tx.execute(
        "UPDATE work_tasks SET requested=?1, log=?2, note='', sig_full=?3, sig_head=?4, sig_prog=?5, sig_tool=?6, \
         prediction=?7 WHERE id=?8",
        params![
            now,
            log,
            sigs.full,
            sigs.head,
            sigs.prog,
            sigs.tool,
            prediction_json,
            task.id
        ],
    )?;
    event(
        tx,
        now,
        actor,
        "work.requested",
        &task.id,
        json!({
            "weight": task.weight,
            "scope": task.scope,
            "detail": format!("weight {}", number(task.weight)),
        }),
    )
}
