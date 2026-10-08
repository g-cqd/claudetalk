//! `work status` text: the machine and cores lines, the quiet line, the scope tree, and the running and waiting
//! jobs.

use super::{claimant, scope_label, scope_tree_lines};
use agora_sched::admit::MemLevel;
use agora_sched::model::{MachineView, StatusReport, TaskRecord};
use agora_sched::pyfmt::{fmt_fixed, fmt_g, printable};
use agora_sched::settings::MemLimit;
use agora_sched::textfmt::{cores_text, display, gib, memory, number, span};

/// work.py `status` text: the machine and cores lines, the optional quiet line, the scope tree, running and waiting
/// jobs, the task tally and the sweep changes. Not prefixed with `work: `.
pub fn status_text(r: &StatusReport, now_ms: i64) -> String {
    let m = &r.machine;
    let mut lines = vec![machine_line(m), cores_line(m)];
    if let Some(quiet) = &m.quiet {
        lines.push(format!(
            "quiet {}: {}",
            printable(&quiet.state),
            printable(&quiet.text)
        ));
    }
    if !r.scopes.is_empty() {
        lines.push("scopes:".to_owned());
        lines.extend(
            scope_tree_lines(&r.scopes)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    if !r.running.is_empty() {
        lines.push("running:".to_owned());
        lines.extend(r.running.iter().map(|t| running_line(t, m.cpus, now_ms)));
    }
    if !r.waiting.is_empty() {
        lines.push("waiting for capacity:".to_owned());
        lines.extend(r.waiting.iter().map(|t| waiting_line(t, m.cpus, now_ms)));
    }
    let c = &r.counts;
    lines.push(format!(
        "tasks: {} queued ({} ready), {} claimed, {} running, {} done, {} failed, {} cancelled",
        c.queued, c.ready, c.claimed, c.running, c.done, c.failed, c.cancelled
    ));
    lines.extend(r.swept.iter().map(|s| {
        format!(
            "swept: {} {}: {}",
            printable(&s.id),
            printable(&s.state),
            printable(&s.note)
        )
    }));
    lines.join("\n")
}

/// work.py `machine:` line.
fn machine_line(m: &MachineView) -> String {
    let idle = m.idle.map_or_else(
        || "unknown".to_owned(),
        |idle| format!("{}%", fmt_fixed(idle, 0)),
    );
    let used = m.memory_used_kib.map_or_else(|| "unknown".to_owned(), gib);
    let budget = match m.memory_budget_kib {
        None => "no budget".to_owned(),
        Some(budget) => format!("jobs start at or below the budget of {}", gib(budget)),
    };
    let level = m.memory.map_or("unknown", level_name);
    let memory_rule = if m.memory_limit == MemLimit::Off {
        "not a gate".to_owned()
    } else {
        format!("jobs start below {}", m.memory_limit.as_str())
    };
    let load = m
        .load
        .map_or_else(|| "unknown".to_owned(), |load| fmt_fixed(load, 2));
    let load_rule = if m.load_gate {
        format!("jobs start below {}", fmt_fixed(m.load_limit, 2))
    } else {
        "not a gate here: it counts threads waiting on memory and disk".to_owned()
    };
    format!(
        "machine: {} effective CPUs; CPU {idle} idle (jobs start at {}% or more); memory used {used} ({budget}); \
         memory pressure {level} ({memory_rule}); load {load} ({load_rule})",
        number(m.cpus),
        fmt_g(m.idle_min)
    )
}

/// work.py `cores:` line, with the pressure reason when new jobs wait.
fn cores_line(m: &MachineView) -> String {
    let cap = m.capacity.map_or_else(
        || "no cap".to_owned(),
        |cap| format!("capacity {}", number(cap)),
    );
    let mut line = format!(
        "cores: predicted {} of the CPU budget of {} taken by {} job(s); declared {} ({cap})",
        number(m.used),
        number(m.cpu_budget),
        m.jobs,
        number(m.declared)
    );
    if let Some(holds) = &m.holds {
        line.push_str(&format!("; new jobs wait: {}", printable(holds)));
    }
    line
}

/// One running job in `status`.
fn running_line(t: &TaskRecord, cpus: f64, now_ms: i64) -> String {
    let usage = match t.rss_kib {
        Some(rss) => {
            let cpu = t
                .cpu_percent
                .map_or_else(|| "-".to_owned(), |c| format!("{}%", fmt_fixed(c, 0)));
            format!(
                "cpu {cpu}, memory {} (peak {})",
                memory(Some(rss)),
                memory(t.rss_peak_kib)
            )
        }
        None => "not sampled yet".to_owned(),
    };
    let pid = t.pid.map_or_else(|| "-".to_owned(), |pid| pid.to_string());
    let qos = t
        .qos
        .as_deref()
        .filter(|qos| !qos.is_empty())
        .unwrap_or("default");
    format!(
        "  {}: process {pid}, {}, {usage}, weight {}, {}, {} QoS, {}, scope {}: {}{}",
        printable(&t.id),
        span(t.ran_s(now_ms)),
        fmt_g(t.weight),
        describe_hold(t, cpus),
        printable(qos),
        claimant(t),
        scope_label(t.scope.as_deref()),
        t.command.as_deref().map_or_else(String::new, display),
        note_suffix_paren(&t.note)
    )
}

/// One waiting job in `status`.
fn waiting_line(t: &TaskRecord, cpus: f64, now_ms: i64) -> String {
    let note = if t.note.is_empty() {
        String::new()
    } else {
        format!(": {}", printable(&t.note))
    };
    format!(
        "  {}: {}, weight {}, {}, {}, scope {}{note}",
        printable(&t.id),
        span(t.waited_s(now_ms)),
        fmt_g(t.weight),
        describe_hold(t, cpus),
        claimant(t),
        scope_label(t.scope.as_deref())
    )
}

/// work.py `describe_hold`: what a job holds against the budget, with its prediction when there is one.
fn describe_hold(t: &TaskRecord, cpus: f64) -> String {
    let kind = if exclusive_at(t.weight, cpus) {
        "exclusive, "
    } else {
        ""
    };
    match &t.prediction {
        None => format!(
            "{kind}holds {} cores, its weight (no prediction)",
            number(t.weight)
        ),
        Some(p) => format!(
            "{kind}predicted {}, {} at peak, {}, p90 {} ({})",
            cores_text(p.cores.min(t.weight)),
            memory(p.rss_kib),
            span(Some(p.duration_s)),
            span(p.duration_p90_s),
            p.basis()
        ),
    }
}

/// ` (note)` for a non-empty note, nothing otherwise.
fn note_suffix_paren(note: &str) -> String {
    if note.is_empty() {
        String::new()
    } else {
        format!(" ({})", printable(note))
    }
}

/// work.py `exclusive`: a job of at least all the CPUs, on a machine of at least two.
fn exclusive_at(weight: f64, cpus: f64) -> bool {
    cpus >= 2.0 && weight >= cpus
}

/// The name of a memory pressure level, as work.py prints it.
fn level_name(level: MemLevel) -> &'static str {
    match level {
        MemLevel::Normal => "normal",
        MemLevel::Warning => "warning",
        MemLevel::Critical => "critical",
    }
}
