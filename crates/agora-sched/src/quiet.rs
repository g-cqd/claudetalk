//! Quiet windows and the admission pass (ref-admission 7-9, 14, 15).

use crate::admit::{self, Budgets, Cause, Hold, MachineSnapshot, RunningJob, Signals, WaitingJob};
use crate::predict::exclusive_at;
use crate::settings::WorkSettings;
use crate::textfmt::{cores_text, enumerate_ids, number, span};

#[derive(Debug, Clone, PartialEq)]
pub struct ManualQuiet {
    pub lease: String,
    pub holder_name: String,
    pub until_s: f64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdmitInput {
    pub now_s: f64,
    pub settings: WorkSettings,
    pub machine: MachineSnapshot,
    pub running: Vec<RunningJob>,
    pub waiting: Vec<WaitingJob>,
    /// (task id, close time) for every task that ever had a quiet window (drain_start).
    pub window_closes: Vec<(String, f64)>,
    /// Some(reason) when the reconciler paused admission (floor breach): every job is held with Cause::Paused,
    /// reason "admission is paused: <reason>".
    pub paused: Option<String>,
    /// Open manual quiet windows (quiet.open leases): while one is open no job predicted above small_weight starts:
    /// reason "quiet window <lease> opened by <holder> is open for <span> more[: <note>]; no job predicted above
    /// <small> starts in it, and this job needs <n>" (Cause::Quiet).
    pub manual_quiet: Vec<ManualQuiet>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub id: String,
    pub exclusive: bool,
    pub quiet_until_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub id: String,
    pub cause: crate::admit::Cause,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub id: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decision {
    pub start: Vec<Start>,
    pub held: Vec<Held>,
    pub fail: Vec<Fail>,
    /// new machine_held_since per waiting job id (None = cleared).
    pub machine_held_since: Vec<(String, Option<f64>)>,
    /// earliest future instant at which a rule's outcome can change (ref-admission section 14 item 5), seconds.
    pub next_wakeup_s: Option<f64>,
    /// true if any job is held by a machine cause (the daemon then re-probes every 2 s).
    pub machine_gated: bool,
}

fn view(input: &AdmitInput) -> (Budgets, Option<&Signals>) {
    (
        admit::budgets(&input.settings, &input.machine),
        input.machine.signals.as_ref(),
    )
}

fn quiet(reason: String) -> Hold {
    Hold {
        cause: Cause::Quiet,
        reason,
    }
}

/// Python `drain_start`: when the job asked, or when the last quiet window of another job closed, whichever is later.
fn drain_start(job: &WaitingJob, closes: &[(String, f64)]) -> f64 {
    let closed = closes
        .iter()
        .filter(|(id, _)| id != &job.id)
        .map(|(_, t)| *t)
        .reduce(f64::max);
    job.requested_s.max(closed.unwrap_or(0.0))
}

/// Python `open_window`: the first running job whose quiet window is open, with the window's close time.
fn open_window(running: &[RunningJob], now: f64) -> Option<(&RunningJob, f64)> {
    running
        .iter()
        .find_map(|r| r.quiet_until_s.filter(|u| *u > now).map(|u| (r, u)))
}

fn manual_open(input: &AdmitInput) -> bool {
    input.manual_quiet.iter().any(|q| q.until_s > input.now_s)
}

/// The open manual window that ends first, named in the reason (the caller has checked that the job is heavy).
fn manual_hold(input: &AdmitInput, job_cores: f64) -> Option<Hold> {
    let now = input.now_s;
    let open = input
        .manual_quiet
        .iter()
        .filter(|q| q.until_s > now)
        .min_by(|x, y| x.until_s.total_cmp(&y.until_s))?;
    let note = if open.note.is_empty() {
        String::new()
    } else {
        format!(": {}", open.note)
    };
    Some(quiet(format!(
        "quiet window {} opened by {} is open for {} more{note}; no job predicted above {} starts in it, and this \
         job needs {}",
        open.lease,
        open.holder_name,
        span(Some(open.until_s - now)),
        cores_text(input.settings.small_weight),
        number(job_cores),
    )))
}

/// Supervisor.blocker for one waiting job against `running` (ref-admission section 7), plus `paused` and
/// `manual_quiet` checked first (paused, then manual quiet for heavy jobs). `waiting` is the line of jobs not yet
/// started, the job itself included. A manual window also holds back the heavy jobs behind it, as a drain does, so
/// light jobs still start.
pub fn blocker(
    job: &WaitingJob,
    input: &AdmitInput,
    running: &[RunningJob],
    waiting: &[WaitingJob],
) -> Option<Hold> {
    if let Some(reason) = &input.paused {
        return Some(Hold {
            cause: Cause::Paused,
            reason: format!("admission is paused: {reason}"),
        });
    }
    let (b, sig) = view(input);
    let s = &input.settings;
    let now = input.now_s;
    let job_cores = admit::cores(job.weight, job.pred_cores);
    let heavy_job = admit::is_heavy(job_cores, s.small_weight);
    let exclusive = exclusive_at(job.weight, b.cpus);
    // An open manual window holds every heavy job, exclusive ones included, before the exclusive rules apply.
    if heavy_job && let Some(hold) = manual_hold(input, job_cores) {
        return Some(hold);
    }
    let mut quiet_line: Vec<&WaitingJob> = waiting
        .iter()
        .filter(|w| exclusive_at(w.weight, b.cpus))
        .collect();
    quiet_line.sort_by(|x, y| admit::waiting_order_cmp(x, y, s.aging));
    if exclusive {
        return quiet_blocker(job, input, running, &quiet_line);
    }
    let small = cores_text(s.small_weight);
    let window = open_window(running, now);
    let drain = match quiet_line.first() {
        Some(first) if window.is_none() => Some((
            *first,
            drain_start(first, &input.window_closes) + s.quiet_drain,
        )),
        _ => None,
    };
    if heavy_job {
        if let Some((w, until)) = window {
            return Some(quiet(format!(
                "job {} runs in its quiet window for {} more; no job predicted above {small} starts in it, and this \
                 job needs {}",
                w.id,
                span(Some(until - now)),
                number(job_cores),
            )));
        }
        if let Some((first, end)) = drain
            && now < end
        {
            return Some(quiet(format!(
                "job {} waits to run exclusive, and for {} more no job predicted above {small} starts; this job \
                 needs {}",
                first.id,
                span(Some(end - now)),
                number(job_cores),
            )));
        }
    }
    let holding = window.is_some() || drain.is_some_and(|(_, end)| now < end) || manual_open(input);
    let mut line: Vec<&WaitingJob> = waiting
        .iter()
        .filter(|w| {
            let c = admit::cores(w.weight, w.pred_cores);
            !exclusive_at(w.weight, b.cpus)
                && !admit::is_tiny(c)
                && !(holding && admit::is_heavy(c, s.small_weight))
        })
        .collect();
    line.sort_by(|x, y| admit::waiting_order_cmp(x, y, s.aging));
    if !admit::is_tiny(job_cores) {
        let pos = line.iter().position(|w| w.id == job.id).filter(|p| *p > 0);
        if let Some(ahead) = pos.and_then(|p| line.get(..p))
            && let Some(hold) = admit::backfill(job, ahead, running, s, &b, sig, now)
        {
            return Some(hold);
        }
    }
    admit::admission(job, running, s, &b, sig, now)
}

/// Supervisor.quiet_blocker (ref-admission section 8) for an exclusive job. `quiet_line` is the exclusive jobs of the
/// waiting line in waiting order, the job itself included when it is exclusive. Paused and manual quiet are not
/// checked here; `blocker` does that first.
pub fn quiet_blocker(
    job: &WaitingJob,
    input: &AdmitInput,
    running: &[RunningJob],
    quiet_line: &[&WaitingJob],
) -> Option<Hold> {
    let (b, sig) = view(input);
    let s = &input.settings;
    let now = input.now_s;
    if let Some(first) = quiet_line.first().filter(|f| f.id != job.id) {
        return Some(quiet(format!(
            "job {}, exclusive too, is ahead of it: one quiet window at a time",
            first.id
        )));
    }
    if let Some((w, until)) = open_window(running, now) {
        return Some(quiet(format!(
            "job {}'s quiet window is open for {} more: one quiet window at a time",
            w.id,
            span(Some(until - now)),
        )));
    }
    let end = drain_start(job, &input.window_closes) + s.quiet_drain;
    if now >= end {
        // Past the drain it starts whatever runs: no cores or capacity check, only memory and pressure.
        return admit::memory_reservation(job.pred_rss_p90_kib, running, &b, sig, now)
            .or_else(|| admit::pressure(s, &b, sig));
    }
    let busy: Vec<String> = running
        .iter()
        .filter(|r| admit::is_heavy(admit::cores(r.weight, r.pred_cores), s.small_weight))
        .map(|r| r.id.clone())
        .collect();
    if !busy.is_empty() {
        let (noun, verb) = if busy.len() == 1 {
            ("job", "runs")
        } else {
            ("jobs", "run")
        };
        return Some(quiet(format!(
            "it drains: {noun} {}, predicted above {}, still {verb}; it starts in {} at the latest",
            enumerate_ids(&busy),
            cores_text(s.small_weight),
            span(Some(end - now)),
        )));
    }
    match admit::admission(job, running, s, &b, sig, now) {
        Some(hold) if matches!(hold.cause, Cause::Cores | Cause::Capacity) => Some(Hold {
            cause: hold.cause,
            reason: format!(
                "{}; it starts in {} at the latest",
                hold.reason,
                span(Some(end - now))
            ),
        }),
        other => other,
    }
}

/// Earliest instant after `now` at which a rule's outcome can change (ref-admission section 14 item 5).
fn next_wakeup(
    input: &AdmitInput,
    b: &Budgets,
    running: &[RunningJob],
    remaining: &[WaitingJob],
    since: &[(String, Option<f64>)],
) -> Option<f64> {
    let s = &input.settings;
    let mut at: Vec<f64> = Vec::new();
    for w in remaining.iter().filter(|w| exclusive_at(w.weight, b.cpus)) {
        at.push(w.requested_s + s.quiet_drain);
    }
    for (_, close) in &input.window_closes {
        at.push(*close + s.quiet_drain);
    }
    for r in running {
        at.extend(r.quiet_until_s);
        if let (Some(started), Some(p90)) = (r.started_s, r.pred_duration_p90_s) {
            at.push(started + p90);
        }
    }
    let head = remaining.iter().find(|w| {
        !exclusive_at(w.weight, b.cpus) && !admit::is_tiny(admit::cores(w.weight, w.pred_cores))
    });
    if let Some(head) = head {
        at.push(head.requested_s + s.backfill_wait);
    }
    at.extend(
        since
            .iter()
            .filter_map(|(_, t)| (*t).map(|t| t + s.load_wait)),
    );
    at.extend(input.manual_quiet.iter().map(|q| q.until_s));
    at.into_iter()
        .filter(|t| t.is_finite() && *t > input.now_s)
        .reduce(f64::min)
}

/// One admission pass (ref-admission section 14 tick): waiting order, each job judged against the running set
/// including jobs started earlier in this pass; load-wait failures (section 9) with the exact note.
pub fn decide(input: &AdmitInput) -> Decision {
    let s = &input.settings;
    let now = input.now_s;
    let (b, _) = view(input);
    let mut order = input.waiting.clone();
    order.sort_by(|x, y| admit::waiting_order_cmp(x, y, s.aging));
    let mut running = input.running.clone();
    // The line as the blocker sees it: every job not yet started or failed, in waiting order.
    let mut remaining = order.clone();
    let mut out = Decision::default();
    for job in &order {
        match blocker(job, input, &running, &remaining) {
            None => {
                let exclusive = exclusive_at(job.weight, b.cpus);
                let quiet_until_s = exclusive.then_some(now + s.quiet_max);
                out.start.push(Start {
                    id: job.id.clone(),
                    exclusive,
                    quiet_until_s,
                });
                running.push(RunningJob {
                    id: job.id.clone(),
                    weight: job.weight,
                    pred_cores: job.pred_cores,
                    pred_rss_p90_kib: job.pred_rss_p90_kib,
                    pred_duration_p90_s: job.pred_duration_p90_s,
                    rss_kib: None,
                    started_s: Some(now),
                    quiet_until_s,
                });
                remaining.retain(|w| w.id != job.id);
            }
            Some(hold) => {
                if hold.cause.is_machine() {
                    let since = job.machine_held_since_s.unwrap_or(now);
                    let waited = now - since;
                    if waited >= s.load_wait {
                        out.fail.push(Fail {
                            id: job.id.clone(),
                            note: format!(
                                "{} for {}, the longest a job waits on the machine's pressure (agora work settings \
                                 --load-wait)",
                                hold.reason,
                                span(Some(waited)),
                            ),
                        });
                        remaining.retain(|w| w.id != job.id);
                        continue;
                    }
                    out.machine_held_since.push((job.id.clone(), Some(since)));
                } else {
                    out.machine_held_since.push((job.id.clone(), None));
                }
                out.held.push(Held {
                    id: job.id.clone(),
                    cause: hold.cause,
                    reason: hold.reason,
                });
            }
        }
    }
    out.machine_gated = out.held.iter().any(|h| h.cause.is_machine());
    out.next_wakeup_s = next_wakeup(input, &b, &running, &remaining, &out.machine_held_since);
    out
}

#[cfg(test)]
mod pass_tests;
#[cfg(test)]
mod tests;
