//! Pure pieces of the work handlers: the machine block of `WorkStatus`, the plan mode of `WorkPlan` and the
//! scope filter. No I/O: every input is passed in, so each rule is tested without a store or a machine.

use agora_sched::admit::{self, MachineSnapshot};
use agora_sched::model::{LeaseRecord, TaskRecord};
use agora_sched::model_reports::{MachineView, QuietView};
use agora_sched::plan::check_agents;
use agora_sched::predict::Level;
use agora_sched::pyfmt::printable;
use agora_sched::render::Mode;
use agora_sched::settings::WorkSettings;
use std::collections::BTreeMap;

/// Cores a running task is taken to use: the predicted cores, or, when no run of its kind is known, its sampled
/// CPU use (at least 0.1); both capped at its weight (`admit::cores`, work.py `estimated()`).
pub fn running_cores(task: &TaskRecord) -> f64 {
    admit::cores(task.weight, predicted_cores(task))
}

fn predicted_cores(task: &TaskRecord) -> Option<f64> {
    match (&task.prediction, task.cpu_percent) {
        (Some(prediction), _) if prediction.level != Level::None => Some(prediction.cores),
        (_, Some(cpu_percent)) => Some((cpu_percent / 100.0).max(0.1)),
        (Some(prediction), None) => Some(prediction.cores),
        (None, None) => None,
    }
}

/// The machine block of `WorkStatus`: probes from `snapshot`, budgets and holds from `agora_sched::admit`, usage
/// from `running` (every running task: the machine is shared by all scopes), the open quiet window, and the
/// admission pause.
pub fn machine_view(
    settings: &WorkSettings,
    snapshot: &MachineSnapshot,
    running: &[TaskRecord],
    manual: &[LeaseRecord],
    paused: Option<String>,
    now_ms: i64,
) -> MachineView {
    let budgets = admit::budgets(settings, snapshot);
    let signals = snapshot.signals.clone().unwrap_or_default();
    MachineView {
        cpus: snapshot.cpus,
        idle: signals.idle_pct,
        idle_min: settings.idle_min,
        memory: signals.memory_level,
        memory_used_kib: signals.memory_used_kib,
        memory_budget_kib: budgets.memory_budget_kib,
        memory_limit: settings.memory_limit,
        load: signals.load1,
        load_gate: signals.load_gate,
        load_limit: budgets.load_limit,
        cpu_budget: budgets.budget,
        used: running.iter().map(running_cores).sum(),
        capacity: budgets.capacity,
        declared: running.iter().map(|task| task.weight).sum(),
        jobs: u32::try_from(running.len()).unwrap_or(u32::MAX),
        holds: admit::pressure(settings, &budgets, snapshot.signals.as_ref())
            .map(|hold| hold.reason),
        quiet: quiet_view(running, manual, now_ms),
        paused,
        disk_free_kib: snapshot.disk_free_kib,
    }
}

/// The open quiet window, if any: an exclusive running job still inside its window (the one that ends last), else
/// an open manual window (a quiet lease whose expected end is still ahead). A window that has closed is not shown.
pub fn quiet_view(
    running: &[TaskRecord],
    manual: &[LeaseRecord],
    now_ms: i64,
) -> Option<QuietView> {
    let window = running
        .iter()
        .filter(|task| task.exclusive)
        .filter_map(|task| {
            let until = task.quiet_until_ms.filter(|until| *until > now_ms)?;
            Some((task, until))
        })
        .max_by_key(|(_, until)| *until);
    if let Some((task, until)) = window {
        return Some(QuietView {
            state: "window".to_string(),
            job: Some(task.id.clone()),
            lease: None,
            seconds_left: seconds_between(now_ms, until),
            text: format!("exclusive job {} runs in its quiet window", task.id),
        });
    }
    manual.iter().find_map(|lease| {
        let end = lease.expected_end_ms.filter(|end| *end > now_ms)?;
        let note = if lease.note.is_empty() {
            String::new()
        } else {
            format!(": {}", printable(&lease.note))
        };
        Some(QuietView {
            state: "manual".to_string(),
            job: None,
            lease: Some(lease.id.clone()),
            seconds_left: seconds_between(now_ms, end),
            text: format!(
                "quiet window {} opened by {}{note}",
                lease.id,
                printable(&lease.holder_name)
            ),
        })
    })
}

/// Seconds from `from_ms` to `to_ms`, 0 when `to_ms` is not ahead.
fn seconds_between(from_ms: i64, to_ms: i64) -> f64 {
    to_ms.saturating_sub(from_ms).max(0) as f64 / 1000.0
}

/// The plan mode of a `WorkPlan` request. `agents` must be within `plan::MAX_AGENTS` (so 0 is refused, with or
/// without assignment); one agent without assignment is the order, anything else is a plan over those lanes.
/// The error is the user-facing message.
pub fn plan_mode(agents: u32, assign: bool, conflict_cost: Option<f64>) -> Result<Mode, String> {
    let agents = check_agents(usize::try_from(agents).unwrap_or(usize::MAX))?;
    if agents == 1 && !assign {
        return Ok(Mode::Order);
    }
    Ok(Mode::Plan {
        agents,
        assign,
        conflict_cost,
    })
}

/// Most queued tasks one `WorkPlan` may plan over; a larger queue is refused before the scheduler sees it (S-6).
pub const MAX_PLAN_TASKS: u64 = 2000;

/// The refusal for a scope whose queue holds `queued` tasks, when that is more than `MAX_PLAN_TASKS`.
pub fn check_plan_size(queued: u64) -> Result<(), String> {
    if queued > MAX_PLAN_TASKS {
        Err(format!(
            "the queue has more than {MAX_PLAN_TASKS} tasks to plan"
        ))
    } else {
        Ok(())
    }
}

/// Largest total of name and value bytes a `WorkRun` environment may carry (S-5).
pub const MAX_ENV_BYTES: usize = 256 * 1024;

/// The refusal for an environment whose names and values together exceed `MAX_ENV_BYTES`.
pub fn check_env_size(env: &BTreeMap<String, String>) -> Result<(), String> {
    let total = env.iter().fold(0usize, |total, (name, value)| {
        total.saturating_add(name.len()).saturating_add(value.len())
    });
    if total > MAX_ENV_BYTES {
        Err("the environment is larger than 256 KiB".to_string())
    } else {
        Ok(())
    }
}

/// Most entries one `WorkLog` call returns, whatever limit the caller asks for.
pub const MAX_LOG_LIMIT: u32 = 1000;

/// The limit a `WorkLog` call really uses: the requested one, at most `MAX_LOG_LIMIT`.
pub fn log_limit(limit: u32) -> u32 {
    limit.min(MAX_LOG_LIMIT)
}

/// Milliseconds between two sampling passes (live samples and the sweep) of `WorkStatus`.
pub const SAMPLE_INTERVAL_MS: i64 = 2_000;

/// Whether a sampling pass is due at `now_ms`, given the time of the last one (`None`: never). A clock that went
/// back is due at once, so a pass is never skipped for longer than `SAMPLE_INTERVAL_MS`.
pub fn sample_due(last_ms: Option<i64>, now_ms: i64) -> bool {
    match last_ms {
        None => true,
        Some(last) => now_ms < last || now_ms.saturating_sub(last) >= SAMPLE_INTERVAL_MS,
    }
}

/// Whether `task` belongs to `scope`; every task belongs when no scope is given.
pub fn in_scope(task: &TaskRecord, scope: Option<&str>) -> bool {
    scope.is_none_or(|wanted| task.scope.as_deref() == Some(wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_sched::admit::{MemLevel, Signals};
    use agora_sched::settings::MemLimit;
    use serde_json::json;

    /// A queued task with weight 1 and no prediction, built from its wire form.
    fn task(id: &str) -> TaskRecord {
        serde_json::from_value(json!({
            "seq": 1, "id": id, "title": "t", "kind": "work", "tier": 0,
            "author": "aa", "author_name": "A", "state": "running",
            "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0,
        }))
        .expect("the test task is valid")
    }

    fn lease(id: &str, expected_end_ms: Option<i64>) -> LeaseRecord {
        serde_json::from_value(json!({
            "id": id, "kind": "quiet", "resource": "quiet", "amount": 1.0,
            "holder": "bb", "holder_name": "Bo", "created_ms": 0,
            "heartbeat_ms": 0, "expected_end_ms": expected_end_ms, "note": "",
        }))
        .expect("the test lease is valid")
    }

    fn machine(cpus: f64, signals: Option<Signals>) -> MachineSnapshot {
        MachineSnapshot {
            cpus,
            total_memory_kib: None,
            disk_free_kib: Some(1024),
            signals,
        }
    }

    #[test]
    fn plan_mode_is_order_for_one_agent_without_assignment() {
        assert_eq!(plan_mode(1, false, None), Ok(Mode::Order));
        assert_eq!(plan_mode(1, false, Some(2.0)), Ok(Mode::Order));
    }

    #[test]
    fn plan_mode_accepts_exactly_256_agents() {
        assert_eq!(
            plan_mode(256, false, None),
            Ok(Mode::Plan {
                agents: 256,
                assign: false,
                conflict_cost: None
            })
        );
    }

    #[test]
    fn plan_mode_is_a_plan_for_several_agents_or_an_assignment() {
        assert_eq!(
            plan_mode(3, false, Some(2.5)),
            Ok(Mode::Plan {
                agents: 3,
                assign: false,
                conflict_cost: Some(2.5)
            })
        );
        assert_eq!(
            plan_mode(1, true, None),
            Ok(Mode::Plan {
                agents: 1,
                assign: true,
                conflict_cost: None
            })
        );
    }

    #[test]
    fn plan_mode_refuses_zero_agents_and_more_than_256_agents() {
        let message = "agents must be between 1 and 256";
        assert_eq!(plan_mode(0, false, None), Err(message.to_string()));
        assert_eq!(plan_mode(0, true, None), Err(message.to_string()));
        assert_eq!(plan_mode(257, false, None), Err(message.to_string()));
        assert_eq!(plan_mode(u32::MAX, true, None), Err(message.to_string()));
    }

    #[test]
    fn running_cores_uses_the_prediction_capped_at_the_weight() {
        let mut t = task("a");
        t.weight = 4.0;
        t.prediction = Some(agora_sched::predict::Prediction {
            cores: 2.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 1.0,
            duration_p90_s: None,
            level: Level::Full,
            runs: 3,
        });
        assert_eq!(running_cores(&t), 2.0);
        t.prediction = None;
        assert_eq!(running_cores(&t), 4.0);
    }

    #[test]
    fn running_cores_falls_back_to_the_sample_when_nothing_is_predicted() {
        let mut t = task("a");
        t.weight = 4.0;
        t.cpu_percent = Some(150.0);
        assert_eq!(running_cores(&t), 1.5);
        t.cpu_percent = Some(5.0);
        assert_eq!(running_cores(&t), 0.1);
    }

    #[test]
    fn machine_view_sums_usage_over_every_running_task() {
        let mut a = task("a");
        a.weight = 2.0;
        let mut b = task("b");
        b.weight = 3.0;
        b.scope = Some("other".to_string());
        let snap = machine(
            8.0,
            Some(Signals {
                idle_pct: Some(50.0),
                ..Signals::default()
            }),
        );
        let view = machine_view(&WorkSettings::default(), &snap, &[a, b], &[], None, 0);
        assert_eq!(view.cpus, 8.0);
        assert_eq!(view.cpu_budget, 8.0);
        assert_eq!(view.used, 5.0);
        assert_eq!(view.declared, 5.0);
        assert_eq!(view.jobs, 2);
        assert_eq!(view.idle, Some(50.0));
        assert_eq!(view.disk_free_kib, Some(1024));
        assert!(view.holds.is_none());
        assert!(view.quiet.is_none());
    }

    #[test]
    fn machine_view_reports_a_hold_and_the_pause() {
        let signals = Signals {
            memory_level: Some(MemLevel::Warning),
            ..Signals::default()
        };
        let settings = WorkSettings {
            memory_limit: MemLimit::Warning,
            ..WorkSettings::default()
        };
        let view = machine_view(
            &settings,
            &machine(4.0, Some(signals)),
            &[],
            &[],
            Some("disk is low".to_string()),
            0,
        );
        assert!(
            view.holds
                .as_deref()
                .is_some_and(|h| h.contains("memory pressure"))
        );
        assert_eq!(view.paused.as_deref(), Some("disk is low"));
        assert_eq!(view.jobs, 0);
    }

    #[test]
    fn quiet_view_is_the_exclusive_job_window_while_it_is_open() {
        let mut open = task("big");
        open.exclusive = true;
        open.quiet_until_ms = Some(12_500);
        let view = quiet_view(std::slice::from_ref(&open), &[], 10_000).expect("window is open");
        assert_eq!(view.state, "window");
        assert_eq!(view.job.as_deref(), Some("big"));
        assert_eq!(view.lease, None);
        assert_eq!(view.seconds_left, 2.5);
        assert!(quiet_view(&[open], &[], 12_500).is_none());
    }

    #[test]
    fn quiet_view_ignores_a_non_exclusive_job_and_a_closed_manual_window() {
        let mut shared = task("small");
        shared.quiet_until_ms = Some(99_000);
        assert!(quiet_view(&[shared], &[], 0).is_none());
        let closed = lease("q-1", Some(500));
        assert!(quiet_view(&[], &[closed], 500).is_none());
    }

    #[test]
    fn quiet_view_shows_an_open_manual_window_with_its_note() {
        let mut open = lease("q-2", Some(60_000));
        open.note = "release build".to_string();
        let view = quiet_view(&[], &[open], 30_000).expect("manual window is open");
        assert_eq!(view.state, "manual");
        assert_eq!(view.lease.as_deref(), Some("q-2"));
        assert_eq!(view.job, None);
        assert_eq!(view.seconds_left, 30.0);
        assert_eq!(view.text, "quiet window q-2 opened by Bo: release build");
    }

    #[test]
    fn in_scope_matches_every_task_without_a_scope_and_only_its_scope_otherwise() {
        let mut t = task("a");
        assert!(in_scope(&t, None));
        assert!(!in_scope(&t, Some("alpha")));
        t.scope = Some("alpha".to_string());
        assert!(in_scope(&t, Some("alpha")));
        assert!(in_scope(&t, None));
        assert!(!in_scope(&t, Some("beta")));
    }

    fn env_of(pairs: &[(&str, usize)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, len)| (name.to_string(), "x".repeat(*len)))
            .collect()
    }

    #[test]
    fn env_at_the_256_kib_limit_is_accepted_and_one_byte_more_is_refused() {
        assert!(check_env_size(&env_of(&[("A", MAX_ENV_BYTES - 1)])).is_ok());
        let message = "the environment is larger than 256 KiB";
        assert_eq!(
            check_env_size(&env_of(&[("A", MAX_ENV_BYTES)])),
            Err(message.to_string())
        );
        // Names count too: many small variables add up.
        assert_eq!(
            check_env_size(&env_of(&[("AB", MAX_ENV_BYTES - 1), ("C", 0)])),
            Err(message.to_string())
        );
    }

    #[test]
    fn plan_size_at_2000_tasks_is_accepted_and_2001_is_refused() {
        assert!(check_plan_size(2000).is_ok());
        assert_eq!(
            check_plan_size(2001),
            Err("the queue has more than 2000 tasks to plan".to_string())
        );
    }

    #[test]
    fn log_limit_is_clamped_to_1000() {
        assert_eq!(log_limit(0), 0);
        assert_eq!(log_limit(50), 50);
        assert_eq!(log_limit(1000), 1000);
        assert_eq!(log_limit(1001), 1000);
        assert_eq!(log_limit(u32::MAX), 1000);
    }

    #[test]
    fn sampling_is_due_when_never_run_and_after_two_seconds() {
        assert!(sample_due(None, 0));
        assert!(!sample_due(Some(10_000), 11_999));
        assert!(sample_due(Some(10_000), 12_000));
        assert!(sample_due(Some(10_000), 60_000));
    }

    #[test]
    fn sampling_is_due_at_once_when_the_clock_went_back() {
        assert!(sample_due(Some(10_000), 5_000));
    }
}
