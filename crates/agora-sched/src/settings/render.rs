//! The settings document and text (`settings --json`, `settings` text): ref-admission section 11 rendering.

use crate::admit::MachineSnapshot;
use crate::pyfmt::{fmt_fixed, fmt_g};
use crate::textfmt::{gib, number, span};

use super::{MemBudget, MemLimit, WorkSettings};

/// Memory kept back from the total for the default memory budget: total minus 2 GiB (work.py RESERVE_KIB).
const RESERVE_KIB: u64 = 2 * 1024 * 1024;
/// The memory budget in KiB as work.py's `memory_budget` computes it: auto is the total minus 2 GiB when the total is
/// above 2 GiB, off is None.
fn memory_budget_kib(s: &WorkSettings, m: &MachineSnapshot) -> Option<u64> {
    match s.memory_budget {
        MemBudget::Auto => m
            .total_memory_kib
            .filter(|total| *total > RESERVE_KIB)
            .map(|total| total - RESERVE_KIB),
        MemBudget::Off => None,
        MemBudget::Kib(kib) => Some(kib),
    }
}

/// A whole number is written as an integer (work.py prints 8 as 8), anything else as a float.
fn whole_or_float(x: f64) -> serde_json::Value {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        serde_json::Value::from(x as i64)
    } else {
        serde_json::json!(x)
    }
}

/// settings --json document (work.py key set + agora keys; serde_json::Value, keys sorted is acceptable).
pub fn settings_json(s: &WorkSettings, m: &MachineSnapshot) -> serde_json::Value {
    serde_json::json!({
        "cpu_budget": s.cpu_budget.unwrap_or(m.cpus),
        "cpu_budget_is_default": s.cpu_budget.is_none(),
        "capacity": s.capacity,
        "capacity_is_default": s.capacity.is_none(),
        "cpus": whole_or_float(m.cpus),
        "idle_min": s.idle_min,
        "memory_limit": s.memory_limit.as_str(),
        "memory_budget_kib": memory_budget_kib(s, m),
        "memory_budget_is_default": s.memory_budget == MemBudget::Auto,
        "load_factor": s.load_factor,
        "load_limit": m.cpus * s.load_factor,
        "load_wait": s.load_wait,
        "log_limit": s.log_limit,
        "keep_days": s.keep_days,
        "backfill_wait": s.backfill_wait,
        "small_weight": s.small_weight,
        "aging": s.aging,
        "job_qos": s.job_qos.as_str(),
        "quiet_drain": s.quiet_drain,
        "quiet_max": s.quiet_max,
        "self_accept": s.self_accept,
        "timer_quota": s.timer_quota,
        "disk_floor_kib": s.disk_floor_kib,
        "memory_floor_kib": s.memory_floor_kib,
        "idle_floor_pct": s.idle_floor_pct,
        "reconcile_interval_s": s.reconcile_interval_s,
        "lease_ttl_s": s.lease_ttl_s,
        "run_role": s.run_role,
        "max_waiting": s.max_waiting,
    })
}

/// work.py `settings` text (ref-admission section 11 templates) + one line per agora key.
pub fn settings_text(s: &WorkSettings, m: &MachineSnapshot, macos: bool) -> String {
    let cpus = m.cpus;
    let budget = s.cpu_budget.unwrap_or(cpus);
    let load_limit = cpus * s.load_factor;
    let budget_default = if s.cpu_budget.is_none() {
        " (the default: the effective CPU capacity)"
    } else {
        ""
    };
    let capacity = match s.capacity {
        None => "off (the default): no cap on declared weights".to_owned(),
        Some(cap) => format!(
            "{} cores; the running jobs' declared weights fit it",
            fmt_g(cap)
        ),
    };
    let memory = match (s.memory_budget, memory_budget_kib(s, m)) {
        (MemBudget::Off, _) => "off; used memory is not a gate".to_owned(),
        (_, None) => {
            "unknown here: the machine's memory cannot be read, so used memory is not a gate"
                .to_owned()
        }
        (mode, Some(kib)) => {
            let default = if mode == MemBudget::Auto {
                " (the default: the memory minus 2 GiB)"
            } else {
                ""
            };
            format!(
                "{}{default}; jobs start while used memory (active, wired and compressed) is at or below it",
                gib(kib)
            )
        }
    };
    let memory_rule = if s.memory_limit == MemLimit::Off {
        "not a gate".to_owned()
    } else {
        format!("jobs start below {}", s.memory_limit.as_str())
    };
    let applied = if macos {
        ""
    } else {
        " (not applied here: macOS only)"
    };
    let mut lines = vec![
        format!(
            "cpu budget: {} cores{budget_default}; the running jobs' predicted cores fit it",
            fmt_g(budget)
        ),
        format!("capacity: {capacity}"),
        format!("memory budget: {memory}"),
        format!(
            "idle min: {}%; jobs start while the CPU is at least this idle",
            fmt_g(s.idle_min)
        ),
        format!(
            "memory limit: {}; {memory_rule} memory pressure",
            s.memory_limit.as_str()
        ),
        format!(
            "load factor: {}; on Linux only, jobs start below load {} on {} effective CPUs",
            fmt_g(s.load_factor),
            fmt_fixed(load_limit, 2),
            number(cpus)
        ),
        format!(
            "load wait: {}; a job that the machine's pressure alone holds back longer fails",
            span(Some(s.load_wait))
        ),
        format!(
            "aging: {}; waiting jobs go shortest predicted run time first, less this many seconds for each second waited",
            fmt_g(s.aging)
        ),
        format!(
            "backfill wait: {}; a job that waited longer keeps every job behind it",
            span(Some(s.backfill_wait))
        ),
        format!(
            "quiet drain: {}; the longest an exclusive job holds new jobs predicted above the small weight before it starts",
            span(Some(s.quiet_drain))
        ),
        format!(
            "quiet max: {}; the longest an exclusive job keeps them out once it runs",
            span(Some(s.quiet_max))
        ),
        format!(
            "job qos: {}; every job but the exclusive ones runs at this QoS, and its children with it, so the user's own work comes first{applied}",
            s.job_qos.as_str()
        ),
        format!(
            "small weight: {}; a job predicted to use this many cores or fewer may start past the head for a while",
            fmt_g(s.small_weight)
        ),
        format!(
            "log limit: {} bytes; a job whose log grows past it is stopped",
            s.log_limit
        ),
        format!(
            "keep: {} days of events and of the logs of ended tasks",
            fmt_g(s.keep_days)
        ),
    ];
    lines.push(if s.self_accept {
        "self accept: on; a command's author may accept it too".to_owned()
    } else {
        "self accept: off; a queued command needs an accepter other than its author".to_owned()
    });
    lines.push(format!(
        "timer quota: {} enabled timers per identity",
        s.timer_quota
    ));
    lines.push(format!(
        "run role: {}; identities below it cannot run or accept commands",
        s.run_role
    ));
    lines.push(format!("max waiting: {} jobs per identity", s.max_waiting));
    lines.push(match s.disk_floor_kib {
        Some(kib) => format!(
            "disk floor: {}; below it the reconciler pauses admission",
            gib(kib)
        ),
        None => {
            "disk floor: off; the reconciler does not pause admission for disk space".to_owned()
        }
    });
    lines.push(match s.memory_floor_kib {
        Some(kib) => format!(
            "memory floor: {}; below it the reconciler pauses admission",
            gib(kib)
        ),
        None => "memory floor: off; the reconciler does not pause admission for memory".to_owned(),
    });
    lines.push(match s.idle_floor_pct {
        Some(pct) => format!(
            "idle floor: {}%; below it the reconciler pauses admission",
            fmt_g(pct)
        ),
        None => "idle floor: off; the reconciler does not pause admission for idle CPU".to_owned(),
    });
    lines.push(format!(
        "reconcile interval: {}",
        span(Some(s.reconcile_interval_s))
    ));
    lines.push(format!("lease ttl: {}", span(Some(s.lease_ttl_s))));
    lines.join("\n")
}

#[cfg(test)]
mod tests;
