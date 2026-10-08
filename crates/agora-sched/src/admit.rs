//! Admission control: budgets, pressure, memory reservation, per-job admission and backfill (ref-admission 3-9, 14).

use std::cmp::Ordering;

use crate::predict::exclusive_at;
use crate::pyfmt::{fmt_fixed, fmt_g, py_fsum};
use crate::settings::{MemBudget, MemLimit, WorkSettings};
use crate::task::VALIDITY_KINDS;
use crate::textfmt::{gib, number, span};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MemLevel {
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Signals {
    pub idle_pct: Option<f64>,
    pub memory_level: Option<MemLevel>,
    pub load1: Option<f64>,
    pub load_gate: bool,
    pub memory_used_kib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MachineSnapshot {
    pub cpus: f64,
    pub total_memory_kib: Option<u64>,
    /// free space on the filesystem holding AGORA_HOME (floor checks, reconciler).
    pub disk_free_kib: Option<u64>,
    pub signals: Option<Signals>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunningJob {
    pub id: String,
    pub weight: f64,
    pub pred_cores: Option<f64>,
    pub pred_rss_p90_kib: Option<u64>,
    pub pred_duration_p90_s: Option<f64>,
    pub rss_kib: Option<u64>,
    pub started_s: Option<f64>,
    pub quiet_until_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitingJob {
    pub id: String,
    pub kind: String,
    pub seq: i64,
    pub requested_s: f64,
    pub weight: f64,
    pub pred_cores: Option<f64>,
    pub pred_duration_s: Option<f64>,
    pub pred_duration_p90_s: Option<f64>,
    pub pred_rss_p90_kib: Option<u64>,
    pub machine_held_since_s: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    Cores,
    Capacity,
    Reserved,
    Memory,
    Idle,
    Load,
    Quiet,
    Ahead,
    Paused,
}

impl Cause {
    /// Memory | Idle | Load
    pub fn is_machine(self) -> bool {
        matches!(self, Cause::Memory | Cause::Idle | Cause::Load)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub cause: Cause,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Budgets {
    pub cpus: f64,
    pub budget: f64,
    pub capacity: Option<f64>,
    pub memory_budget_kib: Option<u64>,
    pub load_limit: f64,
}

pub const TINY_CORES: f64 = 0.5;
pub const RESERVE_KIB: u64 = 2 * 1024 * 1024;
pub const EPS: f64 = 1e-9;

/// work.py orders a job without a prediction as if it took 30 s.
const DEFAULT_DURATION_S: f64 = 30.0;

pub fn budgets(s: &WorkSettings, m: &MachineSnapshot) -> Budgets {
    let cpus = m.cpus;
    let memory_budget_kib = match s.memory_budget {
        MemBudget::Off => None,
        MemBudget::Kib(kib) => Some(kib),
        MemBudget::Auto => m
            .total_memory_kib
            .filter(|total| *total > RESERVE_KIB)
            .map(|total| total - RESERVE_KIB),
    };
    Budgets {
        cpus,
        budget: s.cpu_budget.unwrap_or(cpus),
        capacity: s.capacity,
        memory_budget_kib,
        load_limit: cpus * s.load_factor,
    }
}

pub fn cores(weight: f64, pred_cores: Option<f64>) -> f64 {
    match pred_cores {
        None => weight,
        Some(pred) => weight.min(pred),
    }
}

pub fn is_tiny(cores: f64) -> bool {
    cores <= TINY_CORES + EPS
}

pub fn is_heavy(cores: f64, small_weight: f64) -> bool {
    cores > small_weight + EPS
}

pub fn pressure(s: &WorkSettings, b: &Budgets, sig: Option<&Signals>) -> Option<Hold> {
    let sig = sig?;
    if let (Some(limit), Some(level)) = (memory_limit_level(s.memory_limit), sig.memory_level)
        && level >= limit
    {
        return Some(hold(
            Cause::Memory,
            format!(
                "memory pressure is {}; jobs start below {}",
                level_name(level),
                s.memory_limit.as_str()
            ),
        ));
    }
    if let (Some(budget), Some(used)) = (b.memory_budget_kib, sig.memory_used_kib)
        && used > budget
    {
        return Some(hold(
            Cause::Memory,
            format!(
                "used memory is {}, above the memory budget of {}",
                gib(used),
                gib(budget)
            ),
        ));
    }
    if let Some(idle) = sig.idle_pct
        && idle < s.idle_min
    {
        return Some(hold(
            Cause::Idle,
            format!(
                "the CPU is {}% idle; jobs start at {}% idle or more",
                fmt_fixed(idle, 0),
                fmt_g(s.idle_min)
            ),
        ));
    }
    if sig.load_gate
        && let Some(load) = sig.load1
        && load >= b.load_limit
    {
        return Some(hold(
            Cause::Load,
            format!(
                "the load average {} is at or above {}",
                fmt_fixed(load, 2),
                fmt_fixed(b.load_limit, 2)
            ),
        ));
    }
    None
}

pub fn memory_reservation(
    cand_rss_p90_kib: Option<u64>,
    running: &[RunningJob],
    b: &Budgets,
    sig: Option<&Signals>,
    now_s: f64,
) -> Option<Hold> {
    let sig = sig?;
    let budget = b.memory_budget_kib?;
    let used = sig.memory_used_kib?;
    if running.is_empty() || used > budget {
        return None;
    }
    // A running job grows from its sampled RSS to its p90 peak only until its p90 run time has passed.
    let growth = running
        .iter()
        .filter_map(|r| {
            let p90 = r.pred_rss_p90_kib?;
            let duration = r.pred_duration_p90_s?;
            let started = r.started_s?;
            (now_s < started + duration).then(|| p90.saturating_sub(r.rss_kib.unwrap_or(0)))
        })
        .fold(0_u64, u64::saturating_add);
    let projected = used
        .saturating_add(growth)
        .saturating_add(cand_rss_p90_kib.unwrap_or(0));
    (projected > budget).then(|| {
        hold(
            Cause::Reserved,
            format!(
                "used memory, the running jobs' predicted growth and this job's predicted peak come to {}, above the memory budget of {}",
                gib(projected),
                gib(budget)
            ),
        )
    })
}

pub fn admission(
    job: &WaitingJob,
    running: &[RunningJob],
    s: &WorkSettings,
    b: &Budgets,
    sig: Option<&Signals>,
    now_s: f64,
) -> Option<Hold> {
    let need = cores(job.weight, job.pred_cores);
    let used = running_cores(running);
    let exclusive = exclusive_at(job.weight, b.cpus);
    if over(need, used, b.budget) && !(is_tiny(need) && !exclusive) {
        return Some(taken(need, used, b));
    }
    let declared_weights: Vec<f64> = running.iter().map(|r| r.weight).collect();
    let declared = py_fsum(&declared_weights);
    if let Some(capacity) = b.capacity
        && over(job.weight, declared, capacity)
    {
        return Some(hold(
            Cause::Capacity,
            format!(
                "declared weights of {} of the capacity of {} cores are taken and this job declares {}",
                number(declared),
                number(capacity),
                number(job.weight)
            ),
        ));
    }
    memory_reservation(job.pred_rss_p90_kib, running, b, sig, now_s).or_else(|| pressure(s, b, sig))
}

/// Queue order: validity work first, then the predicted finish (predicted duration plus aging times the wait), then
/// the request time, then the sequence number. Floats are compared exactly, as Python compares them.
pub fn waiting_order_cmp(a: &WaitingJob, b: &WaitingJob, aging: f64) -> Ordering {
    tier(a)
        .cmp(&tier(b))
        .then_with(|| py_cmp(order_key(a, aging), order_key(b, aging)))
        .then_with(|| py_cmp(a.requested_s, b.requested_s))
        .then_with(|| a.seq.cmp(&b.seq))
}

pub fn head_start_estimate(head_need: f64, running: &[RunningJob], b: &Budgets, now_s: f64) -> f64 {
    let mut used = running_cores(running);
    if !over(head_need, used, b.budget) {
        return now_s;
    }
    let mut ends: Vec<(f64, f64)> = running
        .iter()
        .map(|r| {
            let end = match (r.started_s, r.pred_duration_p90_s) {
                (Some(started), Some(p90)) => now_s.max(started + p90),
                _ => now_s,
            };
            (end, cores(r.weight, r.pred_cores))
        })
        .collect();
    ends.sort_by(|x, y| py_cmp(x.0, y.0).then_with(|| py_cmp(x.1, y.1)));
    for (end, released) in &ends {
        used -= released;
        if !over(head_need, used, b.budget) {
            return *end;
        }
    }
    ends.last().map_or(now_s, |last| last.0)
}

/// `ahead` = the line (non-exclusive, non-tiny jobs) before `job`, in waiting order; ahead[0] is the head.
pub fn backfill(
    job: &WaitingJob,
    ahead: &[&WaitingJob],
    running: &[RunningJob],
    s: &WorkSettings,
    b: &Budgets,
    sig: Option<&Signals>,
    now_s: f64,
) -> Option<Hold> {
    let head = ahead.first().copied()?;
    let need = cores(job.weight, job.pred_cores);
    let head_need = cores(head.weight, head.pred_cores);
    let waited = now_s - head.requested_s;
    if waited >= s.backfill_wait {
        return Some(hold(
            Cause::Ahead,
            format!(
                "job {} has waited {}, past the backfill limit of {}, so no job starts before it",
                head.id,
                span(Some(waited)),
                span(Some(s.backfill_wait))
            ),
        ));
    }
    let used = running_cores(running);
    if !over(head_need, used, b.budget) {
        let reason = match memory_reservation(head.pred_rss_p90_kib, running, b, sig, now_s) {
            Some(held) => format!(
                "job {} is ahead of it and waits for memory: {}",
                head.id, held.reason
            ),
            None => format!("job {} is ahead of it, and there is room for it", head.id),
        };
        return Some(hold(Cause::Ahead, reason));
    }
    if used + need > b.budget + EPS {
        return Some(taken(need, used, b));
    }
    let since_weights: Vec<f64> = running
        .iter()
        .filter(|r| {
            r.started_s
                .is_some_and(|started| started >= head.requested_s)
        })
        .map(|r| cores(r.weight, r.pred_cores))
        .collect();
    let spare = b.budget - head_need - py_fsum(&since_weights);
    if need <= spare + EPS {
        return None;
    }
    let head_start = head_start_estimate(head_need, running, b, now_s);
    if job
        .pred_duration_p90_s
        .is_some_and(|p90| now_s + p90 <= head_start + EPS)
    {
        return None;
    }
    let validity = ahead.iter().copied().find(|j| is_validity(&j.kind));
    let small = need <= s.small_weight + EPS;
    if small && validity.is_none() {
        return None;
    }
    let p90_text = job
        .pred_duration_p90_s
        .map(|p90| format!("{} at p90, ", span(Some(p90))))
        .unwrap_or_default();
    let when = if head_start <= now_s {
        "any moment".to_owned()
    } else {
        format!("in {}", span(Some(head_start - now_s)))
    };
    let mut reason = format!(
        "job {} is ahead of it and needs {} of the {} cores, which leaves {} for jobs behind it; this job needs {}, {}and the head may start {}",
        head.id,
        number(head_need),
        number(b.budget),
        number(spare.max(0.0)),
        number(need),
        p90_text,
        when
    );
    if let Some(v) = validity
        && small
    {
        reason.push_str(&format!(
            ", and job {}, validity work, keeps its place",
            v.id
        ));
    }
    Some(hold(Cause::Ahead, reason))
}

fn hold(cause: Cause, reason: String) -> Hold {
    Hold { cause, reason }
}

fn level_name(level: MemLevel) -> &'static str {
    match level {
        MemLevel::Normal => "normal",
        MemLevel::Warning => "warning",
        MemLevel::Critical => "critical",
    }
}

fn memory_limit_level(limit: MemLimit) -> Option<MemLevel> {
    match limit {
        MemLimit::Warning => Some(MemLevel::Warning),
        MemLimit::Critical => Some(MemLevel::Critical),
        MemLimit::Off => None,
    }
}

fn running_cores(running: &[RunningJob]) -> f64 {
    let values: Vec<f64> = running
        .iter()
        .map(|r| cores(r.weight, r.pred_cores))
        .collect();
    py_fsum(&values)
}

/// Whether a job needing `need` does not fit beside `used` under `limit`. Alone, anything fits.
fn over(need: f64, used: f64, limit: f64) -> bool {
    used > 0.0 && used + need > limit + EPS
}

fn taken(need: f64, used: f64, b: &Budgets) -> Hold {
    hold(
        Cause::Cores,
        format!(
            "predicted {} of {} cores are taken and this job needs {}",
            number(used),
            number(b.budget),
            number(need)
        ),
    )
}

fn is_validity(kind: &str) -> bool {
    let lower = kind.to_lowercase();
    VALIDITY_KINDS.contains(&lower.as_str())
}

fn tier(job: &WaitingJob) -> u8 {
    if is_validity(&job.kind) { 0 } else { 1 }
}

fn order_key(job: &WaitingJob, aging: f64) -> f64 {
    job.pred_duration_s.unwrap_or(DEFAULT_DURATION_S) + aging * job.requested_s
}

/// Float order where equal values (including -0.0 and 0.0) compare equal and NaN has a fixed place, so sorting
/// never sees an inconsistent comparison.
fn py_cmp(a: f64, b: f64) -> Ordering {
    if a == b {
        Ordering::Equal
    } else {
        a.total_cmp(&b)
    }
}

#[cfg(test)]
mod tests;
