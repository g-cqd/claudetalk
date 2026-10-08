//! Argument types of the work-queue and scheduling tools (M5). Doc comments become the `description` of the
//! JSON schema the model sees, so they are written for the model. `deny_unknown_fields` makes a misspelled
//! argument an error the model can correct.
//!
//! Handlers live in `work` and `sched`. Some names equal types of `agora_sched::model` (`ReclaimArgs`,
//! `EscalateArgs`, `CardUpdateArgs`): a handler that needs both qualifies the model one.

use schemars::JsonSchema;
use serde::Deserialize;

fn default_wait_secs() -> u64 {
    55
}

fn default_agents() -> u32 {
    1
}

/// Create or change a scope, or show scopes.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkScopeArgs {
    /// Scope id. With none of kind, parent and task: show this scope and its descendants (all scopes when omitted).
    pub id: Option<String>,
    /// project, conversation or agent. Setting any of kind, parent or task creates or updates the scope.
    pub kind: Option<String>,
    /// Parent scope id; an empty string clears the parent.
    pub parent: Option<String>,
    /// What the scope is about; an empty string clears it.
    pub task: Option<String>,
}

/// Queue a task, optionally with a command.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkAddArgs {
    /// Task id (letters, digits and _ . : @ -). Default: task-N.
    pub id: Option<String>,
    /// Short title.
    pub title: Option<String>,
    /// Kind of work. validity, correctness, safety, security, data-loss and crash are ordered first. Default other.
    pub kind: Option<String>,
    /// Ids of tasks that must finish first.
    pub after: Option<Vec<String>>,
    /// Effort estimate, greater than 0 (default 1).
    pub size: Option<f64>,
    /// Value estimate, greater than 0 (default 1).
    pub value: Option<f64>,
    /// Files or directories the command changes; other tasks holding them are refused.
    pub files: Option<Vec<String>>,
    /// Cores the command is expected to use, greater than 0 (default 1).
    pub weight: Option<f64>,
    /// Working directory of the command (default: the server's current directory).
    pub cwd: Option<String>,
    /// Scope id; it must exist.
    pub scope: Option<String>,
    /// Board card id this task belongs to; the card follows the job.
    pub card: Option<String>,
    /// The command as an argument vector, never a shell line. Not available over HTTP.
    pub command: Option<Vec<String>>,
}

/// Show one task with its command and digest.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkShowArgs {
    /// Task id.
    pub id: String,
}

/// Claim the next claimable task.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkNextArgs {
    /// Only tasks of this scope and its descendants.
    pub scope: Option<String>,
    /// Ordering policy: fifo (default), wsjf or critical-path.
    pub policy: Option<String>,
}

/// Claim one task.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkClaimArgs {
    /// Task id.
    pub id: String,
}

/// Accept the command of a task you claimed.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkAcceptArgs {
    /// Task id.
    pub id: String,
    /// The digest work_show printed for this task. Accept only after reading the command.
    pub digest: String,
}

/// Run a claimed task or a new command on this machine.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkRunArgs {
    /// A task you claimed and accepted. Give this or `command`, not both.
    pub task: Option<String>,
    /// A new command as an argument vector, never a shell line. Give this or `task`.
    pub command: Option<Vec<String>>,
    /// Working directory (new command only).
    pub cwd: Option<String>,
    /// Title (new command only).
    pub title: Option<String>,
    /// Kind of work (new command only).
    pub kind: Option<String>,
    /// Cores the command is expected to use (new command only, default 1).
    pub weight: Option<f64>,
    /// Files or directories the command changes (new command only).
    pub files: Option<Vec<String>>,
    /// Scope id (new command only).
    pub scope: Option<String>,
    /// Board card id the job belongs to (new command only).
    pub card: Option<String>,
    /// Wait at most this long for the job to end (max 55, default 55). The job keeps running after a timeout; use
    /// work_wait to keep waiting.
    #[serde(default = "default_wait_secs")]
    pub timeout_secs: u64,
}

/// Wait for exactly one thing: tasks, all, cards, channel, notices, pid, file, gone or log.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkWaitArgs {
    /// Task ids to wait for.
    pub tasks: Option<Vec<String>>,
    /// Wait for every job that runs or waits now.
    pub all: Option<bool>,
    /// With `tasks`: return when the first one ends.
    pub any: Option<bool>,
    /// With `all`: only jobs of this scope.
    pub scope: Option<String>,
    /// Card ids to wait for.
    pub cards: Option<Vec<String>>,
    /// With `cards`: states that end the wait (default done, cancelled, review, blocked).
    pub states: Option<Vec<String>>,
    /// Channel id: wait for a message with a sequence number greater than `after`.
    pub channel: Option<String>,
    /// With channel: messages after this seq. With notices: notices after this seq (default: only new ones).
    pub after: Option<i64>,
    /// Wait for a new notice addressed to you.
    pub notices: Option<bool>,
    /// Process ids that must exit (not available over HTTP).
    pub pid: Option<Vec<i64>>,
    /// Files that must exist and be non-empty (not available over HTTP).
    pub file: Option<Vec<String>>,
    /// Paths that must disappear (not available over HTTP).
    pub gone: Option<Vec<String>>,
    /// Log file to watch (not available over HTTP); give `pattern` too.
    pub log_path: Option<String>,
    /// Regular expression (Rust regex syntax) that must match a line of `log_path`.
    pub pattern: Option<String>,
    /// Wait at most this long (max 55, default 55). On a timeout, call again with the returned `cursor`.
    #[serde(default = "default_wait_secs")]
    pub timeout_secs: u64,
    /// The `cursor` of a previous timed-out call, to continue it.
    pub cursor: Option<String>,
}

/// Machine, running jobs and waiting jobs.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkStatusArgs {
    /// Only jobs of this scope and its descendants.
    pub scope: Option<String>,
}

/// Cancel a task; a running job is stopped.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkCancelArgs {
    /// Task id.
    pub id: String,
    /// Why.
    pub note: Option<String>,
}

/// Mark a task without a command done or failed.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkFinishArgs {
    /// Task id.
    pub id: String,
    /// done or failed.
    pub state: String,
    /// Why.
    pub note: Option<String>,
}

/// Create, list or delete a timer that queues a task on a schedule.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkScheduleArgs {
    /// create, list or delete.
    pub action: String,
    /// Timer id (delete: required; create: optional name).
    pub id: Option<String>,
    /// When, for create: `at:<epoch ms or RFC 3339 time>`, `every:<N>[s|m|h|d]` (at least 60 s) or `cron:<5 fields>`
    /// (UTC).
    pub when: Option<String>,
    /// For list: only this owner (public key).
    pub owner: Option<String>,
    /// Task template (create): title.
    pub title: Option<String>,
    /// Task template: kind.
    pub kind: Option<String>,
    /// Task template: prerequisites.
    pub after: Option<Vec<String>>,
    /// Task template: size.
    pub size: Option<f64>,
    /// Task template: value.
    pub value: Option<f64>,
    /// Task template: files the command changes.
    pub files: Option<Vec<String>>,
    /// Task template: weight in cores.
    pub weight: Option<f64>,
    /// Task template: working directory. A command needs one.
    pub cwd: Option<String>,
    /// Task template: scope id.
    pub scope: Option<String>,
    /// Task template: board card id.
    pub card: Option<String>,
    /// Task template: command as an argument vector. Not available over HTTP.
    pub command: Option<Vec<String>>,
}

/// Show or change the work settings; changing needs the controller role. Values are text such as "4", "auto", "off"
/// or "14G".
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkSettingsArgs {
    /// Cores the scheduler may use, or auto.
    pub cpu_budget: Option<String>,
    /// Total capacity in cores, auto or off.
    pub capacity: Option<String>,
    /// Memory budget such as 14G, auto or off.
    pub memory_budget: Option<String>,
    /// Minimum CPU idle percent to start a job.
    pub idle_min: Option<String>,
    /// warning, critical or off.
    pub memory_limit: Option<String>,
    /// Load limit as a factor of the CPU count.
    pub load_factor: Option<String>,
    /// Seconds a job waits on machine pressure before it fails.
    pub load_wait: Option<String>,
    /// Log size limit in bytes.
    pub log_limit: Option<String>,
    /// Days to keep ended tasks and their logs.
    pub keep_days: Option<String>,
    /// Seconds the head of the queue is protected from backfill.
    pub backfill_wait: Option<String>,
    /// Weight up to which a job counts as small.
    pub small_weight: Option<String>,
    /// Aging factor of the waiting order.
    pub aging: Option<String>,
    /// utility, default or background.
    pub job_qos: Option<String>,
    /// Seconds an exclusive job drains the machine first.
    pub quiet_drain: Option<String>,
    /// Longest quiet window in seconds.
    pub quiet_max: Option<String>,
    /// true or false: may the accepter be the author of a command.
    pub self_accept: Option<String>,
    /// Enabled timers per identity.
    pub timer_quota: Option<String>,
    /// Free disk size below which admission pauses, or off.
    pub disk_floor: Option<String>,
    /// Available memory below which admission pauses, or off.
    pub memory_floor: Option<String>,
    /// CPU idle percent below which admission pauses, or off.
    pub idle_floor: Option<String>,
    /// Seconds between reconciler passes.
    pub reconcile_interval: Option<String>,
    /// Default lease time in seconds.
    pub lease_ttl: Option<String>,
    /// Lowest role allowed to run or accept commands: worker|supervisor|controller|owner
    pub run_role: Option<String>,
    /// Maximum jobs waiting to start per identity
    pub max_waiting: Option<String>,
}

/// Order or plan tasks: the queue of a scope, or the tasks you pass in.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkPlanArgs {
    /// Only queued tasks of this scope and its descendants.
    pub scope: Option<String>,
    /// fifo (default), wsjf or critical-path.
    pub policy: Option<String>,
    /// Number of agents to plan for (default 1 = just the order).
    #[serde(default = "default_agents")]
    pub agents: u32,
    /// Assign tasks to agents by their files.
    pub assign: Option<bool>,
    /// Cost of two agents touching the same files, for `assign`.
    pub conflict_cost: Option<f64>,
    /// Tasks to plan instead of the queue: objects with id, title, kind, after, size, value, files and queued.
    pub tasks: Option<Vec<serde_json::Value>>,
}

/// Define or resize a capacity pool (controller or above).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolDefineArgs {
    /// Pool name.
    pub name: String,
    /// Total units, greater than 0.
    pub capacity: f64,
    /// What a unit is.
    pub unit: Option<String>,
    /// Free-form note.
    pub note: Option<String>,
}

/// Lease units of a pool or a path, or renew a lease you hold.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolLeaseArgs {
    /// Pool name (give this or `path`).
    pub pool: Option<String>,
    /// Units to lease from the pool (default 1).
    pub amount: Option<f64>,
    /// A file or directory to hold exclusively (give this or `pool`).
    pub path: Option<String>,
    /// Lease time in seconds (default: the lease TTL setting).
    pub ttl_secs: Option<f64>,
    /// Board card the lease is for.
    pub card: Option<String>,
    /// Work task the lease is for.
    pub task: Option<String>,
    /// Free-form note.
    pub note: Option<String>,
    /// A lease id: renew it instead of acquiring a new one.
    pub renew: Option<String>,
}

/// Release a lease.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolReleaseArgs {
    /// Lease id.
    pub lease: String,
}

/// Open, close or show manual quiet windows (open and close need the controller role).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuietWindowArgs {
    /// open, close or status.
    pub action: String,
    /// For open: expected length in seconds.
    pub for_secs: Option<u64>,
    /// For open: why.
    pub note: Option<String>,
    /// For close: the lease id of the window.
    pub lease: Option<String>,
}

/// No arguments.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchdogReportArgs {}

/// Take back a stale task, lease or card.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReclaimArgs {
    /// Task id to reclaim (give exactly one of task, lease and card).
    pub task: Option<String>,
    /// Lease id to reclaim.
    pub lease: Option<String>,
    /// Card id to reclaim.
    pub card: Option<String>,
    /// Why.
    pub note: Option<String>,
}

/// Flag a card as needing its owner and notify the escalation chain.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalateArgs {
    /// Card id.
    pub card: String,
    /// What the owner has to do.
    pub owner_action: String,
    /// Public keys to notify (default: the policy's escalation chain, else the supervisors).
    pub to: Option<Vec<String>>,
}

/// Set the policy of a team and lane, or list all policies.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicySetArgs {
    /// true: list every policy (all other fields are ignored).
    pub list: Option<bool>,
    /// Team (`*` = any). Required unless `list`.
    pub team: Option<String>,
    /// Lane (`*` = any). Required unless `list`.
    pub lane: Option<String>,
    /// Seconds without activity before the owner is told.
    pub stale_after_s: Option<f64>,
    /// Further seconds before the escalation chain is told.
    pub escalate_after_s: Option<f64>,
    /// Seconds without activity before the card is reclaimed.
    pub reclaim_after_s: Option<f64>,
    /// Reclaim stale cards automatically.
    pub reclaim: Option<bool>,
    /// Moving to done needs an independent approval.
    pub require_review: Option<bool>,
    /// Most cards in progress (claimed or doing) for the team and lane.
    pub wip_limit: Option<u32>,
    /// Public keys notified in order at escalation (needs the controller role).
    pub escalation_chain: Option<Vec<String>>,
    /// Notify supervisors when a sprint ends with open cards.
    pub sprint_rollover: Option<bool>,
}

/// Flow metrics: lead and cycle time, WIP, blocked time, job counts and queue waits.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsArgs {
    /// Only this team.
    pub team: Option<String>,
    /// Only this lane.
    pub lane: Option<String>,
    /// Only work finished after this time (ms since the Unix epoch).
    pub since_ms: Option<i64>,
}

/// Change card fields, dependencies and links.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardUpdateArgs {
    /// Card id.
    pub card: String,
    /// Higher = more urgent.
    pub priority: Option<i64>,
    /// Value estimate.
    pub value: Option<f64>,
    /// Effort estimate.
    pub size: Option<f64>,
    /// Team that owns the card's lane.
    pub team: Option<String>,
    /// Flag the card as needing its owner.
    pub needs_owner: Option<bool>,
    /// What the owner has to do.
    pub owner_action: Option<String>,
    /// Card ids this card must wait for (added).
    pub after_add: Option<Vec<String>>,
    /// Card ids to stop waiting for (removed).
    pub after_remove: Option<Vec<String>>,
    /// Link type: re, blocks, relates, duplicates, caused_by, job, message or note.
    pub link_rel: Option<String>,
    /// Link target (card id, job id, message id or note id). Give with `link_rel`.
    pub link_target: Option<String>,
    /// true: remove the link instead of adding it.
    pub unlink: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn work_args_refuse_unknown_fields_and_apply_defaults() {
        assert!(serde_json::from_value::<WorkShowArgs>(json!({"id": "a", "x": 1})).is_err());
        let a: WorkWaitArgs = serde_json::from_value(json!({})).expect("all optional");
        assert_eq!(a.timeout_secs, 55);
        let a: WorkRunArgs = serde_json::from_value(json!({"command": ["true"]})).expect("run");
        assert_eq!(
            (a.timeout_secs, a.command.as_deref()),
            (55, Some(&["true".to_string()][..]))
        );
        let a: WorkPlanArgs = serde_json::from_value(json!({})).expect("plan");
        assert_eq!(a.agents, 1);
    }
}
