//! clap declarations of the work-queue and scheduling commands (M5). `main.rs` re-exports everything here,
//! so the `cmd_*` modules reach it as `crate::WorkCmd`, `crate::PoolCmd`, ...
//!
//! work.py compatibility: `--agent` is accepted and ignored (identity is the signed key), `--interval` is
//! accepted and ignored (waits are event-driven). `--scope` defaults to `$WORK_SCOPE`.

use crate::ScheduleCmd;
use agora_sched::order::OrderPolicy;
use agora_sched::settings::SettingsPatch;
use clap::{Args, Subcommand, ValueEnum};

/// `agora work ...`: the shared work queue. `work order|plan` with `--tasks`/`--task` is answered
/// before this parser runs (no daemon, no identity).
#[derive(Args, Debug)]
pub struct WorkArgs {
    #[command(subcommand)]
    pub cmd: WorkCmd,
}

/// Parsed once at startup, so the size of the largest variant does not matter.
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum WorkCmd {
    /// Create or show scopes.
    #[command(subcommand)]
    Scope(ScopeCmd),
    /// Queue a task (with a command after `--`, or without one for a plain to-do).
    Add(WorkAddArgs),
    /// Show the queue in dispatch order.
    Order(WorkOrderArgs),
    /// Plan the queue over N agents.
    Plan(WorkPlanArgs),
    /// Claim the next claimable task.
    Next(WorkNextArgs),
    /// Claim one task.
    Claim(WorkClaimArgs),
    /// Accept the command of a task you claimed: read it with `show` first.
    Accept(WorkAcceptArgs),
    /// Show one task, its command (as untrusted data) and its digest.
    Show { id: String },
    /// Run a command or a task you claimed and wait for it.
    Run(WorkRunArgs),
    /// Wait for tasks, processes, files, log lines, cards, messages or notices.
    Wait(WorkWaitArgs),
    /// Mark a task without a command done.
    Done(WorkCloseArgs),
    /// Mark a task without a command failed.
    Fail(WorkCloseArgs),
    /// Cancel a task (a running job is stopped).
    Cancel(WorkCloseArgs),
    /// Edit, reprioritise or assign a task.
    Update(WorkUpdateArgs),
    /// Take back a stale task (`task:ID`) or lease (`lease:ID`).
    Reclaim {
        /// `task:<id>` or `lease:<id>`.
        subject: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Show or change the work settings (changing needs the controller role).
    Settings(WorkSettingsArgs),
    /// Machine, running and waiting jobs.
    Status {
        #[arg(long, env = "WORK_SCOPE")]
        scope: Option<String>,
    },
    /// Recent queue events.
    Log(WorkLogArgs),
    /// Timers that queue a task on a schedule.
    #[command(subcommand)]
    Schedule(ScheduleCmd),
}

#[derive(Subcommand, Debug)]
pub enum ScopeCmd {
    /// Create a scope or change its kind, parent or task.
    Set {
        id: String,
        /// project | conversation | agent
        #[arg(long)]
        kind: Option<String>,
        /// Parent scope; an empty value clears it.
        #[arg(long)]
        parent: Option<String>,
        /// What the scope is about; an empty value clears it.
        #[arg(long)]
        task: Option<String>,
        /// Accepted and ignored: identity is the signed key.
        #[arg(long, hide = true)]
        agent: Option<String>,
    },
    /// Show one scope and its descendants, or all scopes.
    Show { id: Option<String> },
}

/// Ordering policy of `order`, `plan` and `next`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyArg {
    Fifo,
    Wsjf,
    CriticalPath,
}

impl From<PolicyArg> for OrderPolicy {
    fn from(p: PolicyArg) -> OrderPolicy {
        match p {
            PolicyArg::Fifo => OrderPolicy::Fifo,
            PolicyArg::Wsjf => OrderPolicy::Wsjf,
            PolicyArg::CriticalPath => OrderPolicy::CriticalPath,
        }
    }
}

#[derive(Args, Debug)]
pub struct WorkAddArgs {
    /// Task id (default: task-N).
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    /// Kind; validity kinds (validity, correctness, safety, security, data-loss, crash) go first.
    #[arg(long)]
    pub kind: Option<String>,
    /// Tasks that must finish first.
    #[arg(long, num_args = 1..)]
    pub after: Vec<String>,
    #[arg(long)]
    pub size: Option<f64>,
    #[arg(long)]
    pub value: Option<f64>,
    /// Files or directories the command changes (other tasks holding them are refused).
    #[arg(long, num_args = 1..)]
    pub files: Vec<String>,
    /// Cores the command is expected to use.
    #[arg(long)]
    pub weight: Option<f64>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    /// Link the task to a board card; the card follows the job.
    #[arg(long)]
    pub card: Option<String>,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
    /// The command, after `--`. It is an argv, never a shell line.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WorkOrderArgs {
    #[arg(long, value_enum, default_value_t = PolicyArg::Fifo)]
    pub policy: PolicyArg,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    /// Order the tasks in FILE (JSON; `-` = stdin) instead of the queue. Handled before parsing.
    #[arg(long)]
    pub tasks: Vec<String>,
    /// Order one task given as `id=a,after=b,...` (repeatable). Handled before parsing.
    #[arg(long)]
    pub task: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WorkPlanArgs {
    /// Number of agents (1 to 256).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub agents: u32,
    /// Assign tasks by best-response dynamics over their files.
    #[arg(long)]
    pub assign: bool,
    #[arg(long)]
    pub conflict_cost: Option<f64>,
    #[arg(long, value_enum, default_value_t = PolicyArg::Fifo)]
    pub policy: PolicyArg,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    /// Plan the tasks in FILE (JSON; `-` = stdin) instead of the queue. Handled before parsing.
    #[arg(long)]
    pub tasks: Vec<String>,
    /// Plan one task given as `id=a,after=b,...` (repeatable). Handled before parsing.
    #[arg(long)]
    pub task: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WorkNextArgs {
    #[arg(long, value_enum, default_value_t = PolicyArg::Fifo)]
    pub policy: PolicyArg,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct WorkClaimArgs {
    pub id: String,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct WorkAcceptArgs {
    pub id: String,
    /// The digest `show` printed for this task.
    #[arg(long)]
    pub digest: String,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct WorkRunArgs {
    /// A task you claimed and accepted; omit it to run the command after `--`.
    pub task: Option<String>,
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long)]
    pub weight: Option<f64>,
    #[arg(long, num_args = 1..)]
    pub files: Vec<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    #[arg(long)]
    pub card: Option<String>,
    /// Stop waiting after SECS (the job runs on; exit 124).
    #[arg(long, default_value_t = 570.0)]
    pub timeout: f64,
    /// Lines of the log to show at the end.
    #[arg(long, default_value_t = 5)]
    pub tail: usize,
    /// Accepted and ignored: waits are event-driven.
    #[arg(long, hide = true)]
    pub interval: Option<f64>,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
    /// The command, after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WorkWaitArgs {
    /// Task ids to wait for.
    pub ids: Vec<String>,
    /// With task ids: the first one to end is enough.
    #[arg(long)]
    pub any: bool,
    /// Every job that runs or waits now.
    #[arg(long)]
    pub all: bool,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    /// Process ids that must exit.
    #[arg(long, num_args = 1..)]
    pub pid: Vec<i64>,
    /// Files that must exist and be non-empty.
    #[arg(long, num_args = 1..)]
    pub file: Vec<String>,
    /// Paths that must disappear.
    #[arg(long, num_args = 1..)]
    pub gone: Vec<String>,
    /// A log file and a regular expression (Rust regex syntax) that must match one of its lines.
    #[arg(long, num_args = 2, value_names = ["PATH", "PATTERN"])]
    pub log: Option<Vec<String>>,
    /// Card ids that must reach one of `--state` (default: done, cancelled, review, blocked).
    #[arg(long, num_args = 1..)]
    pub card: Vec<String>,
    /// Card states for `--card`.
    #[arg(long, num_args = 1.., requires = "card")]
    pub state: Vec<String>,
    /// A channel to wait on for a message after `--after`.
    #[arg(long, requires = "after")]
    pub channel: Option<String>,
    /// The sequence number after which messages (`--channel`) or notices (`--notices`) count. `--notices` without
    /// it waits for the notices that come after the latest one at the start.
    #[arg(long)]
    pub after: Option<i64>,
    /// Wait for a new notice addressed to you.
    #[arg(long)]
    pub notices: bool,
    /// Stop waiting after SECS (exit 124).
    #[arg(long, default_value_t = 570.0)]
    pub timeout: f64,
    #[arg(long, default_value_t = 5)]
    pub tail: usize,
    /// Accepted and ignored: waits are event-driven.
    #[arg(long, hide = true)]
    pub interval: Option<f64>,
}

/// `done`, `fail` and `cancel`.
#[derive(Args, Debug)]
pub struct WorkCloseArgs {
    pub id: String,
    #[arg(long, default_value = "")]
    pub note: String,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct WorkUpdateArgs {
    pub id: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub size: Option<f64>,
    #[arg(long)]
    pub value: Option<f64>,
    /// Replace the prerequisites (no ids clears them).
    #[arg(long, num_args = 0..)]
    pub after: Option<Vec<String>>,
    /// Assign the task to this public key (supervisor or above).
    #[arg(long, conflicts_with = "unassign")]
    pub assign: Option<String>,
    /// Release a claimed task back to the queue.
    #[arg(long)]
    pub unassign: bool,
}

#[derive(Args, Debug)]
pub struct WorkSettingsArgs {
    /// CORES or auto.
    #[arg(long)]
    pub cpu_budget: Option<String>,
    /// CORES, auto or off.
    #[arg(long)]
    pub capacity: Option<String>,
    /// SIZE (14G, 14GiB, 14336M), auto or off.
    #[arg(long)]
    pub memory_budget: Option<String>,
    #[arg(long)]
    pub idle_min: Option<String>,
    /// warning | critical | off
    #[arg(long)]
    pub memory_limit: Option<String>,
    #[arg(long)]
    pub load_factor: Option<String>,
    #[arg(long)]
    pub load_wait: Option<String>,
    #[arg(long)]
    pub log_limit: Option<String>,
    #[arg(long)]
    pub keep_days: Option<String>,
    #[arg(long)]
    pub backfill_wait: Option<String>,
    #[arg(long)]
    pub small_weight: Option<String>,
    #[arg(long)]
    pub aging: Option<String>,
    /// utility | default | background
    #[arg(long)]
    pub job_qos: Option<String>,
    #[arg(long)]
    pub quiet_drain: Option<String>,
    #[arg(long)]
    pub quiet_max: Option<String>,
    /// true | false: may the accepter be the author of a command.
    #[arg(long)]
    pub self_accept: Option<String>,
    #[arg(long)]
    pub timer_quota: Option<String>,
    /// SIZE or off: admission pauses when free disk falls below it.
    #[arg(long)]
    pub disk_floor: Option<String>,
    #[arg(long)]
    pub memory_floor: Option<String>,
    #[arg(long)]
    pub idle_floor: Option<String>,
    #[arg(long)]
    pub reconcile_interval: Option<String>,
    #[arg(long)]
    pub lease_ttl: Option<String>,
    /// Seconds a claimed task may stay unrun before it returns to the queue (default 1800).
    #[arg(long)]
    pub claim_ttl: Option<String>,
    /// The role an identity needs to run or accept a command: worker | supervisor | controller | owner.
    #[arg(long)]
    pub run_role: Option<String>,
    /// Jobs one identity may have queued and not yet started (a count).
    #[arg(long)]
    pub max_waiting: Option<String>,
    /// Accepted and ignored: identity is the signed key.
    #[arg(long, hide = true)]
    pub agent: Option<String>,
}

impl WorkSettingsArgs {
    /// The raw values as a [`SettingsPatch`] (empty when no flag was given: `settings` then only shows).
    pub fn patch(&self) -> SettingsPatch {
        SettingsPatch {
            cpu_budget: self.cpu_budget.clone(),
            capacity: self.capacity.clone(),
            memory_budget: self.memory_budget.clone(),
            idle_min: self.idle_min.clone(),
            memory_limit: self.memory_limit.clone(),
            load_factor: self.load_factor.clone(),
            load_wait: self.load_wait.clone(),
            log_limit: self.log_limit.clone(),
            keep_days: self.keep_days.clone(),
            backfill_wait: self.backfill_wait.clone(),
            small_weight: self.small_weight.clone(),
            aging: self.aging.clone(),
            job_qos: self.job_qos.clone(),
            quiet_drain: self.quiet_drain.clone(),
            quiet_max: self.quiet_max.clone(),
            self_accept: self.self_accept.clone(),
            timer_quota: self.timer_quota.clone(),
            disk_floor: self.disk_floor.clone(),
            memory_floor: self.memory_floor.clone(),
            idle_floor: self.idle_floor.clone(),
            reconcile_interval: self.reconcile_interval.clone(),
            lease_ttl: self.lease_ttl.clone(),
            claim_ttl: self.claim_ttl.clone(),
            run_role: self.run_role.clone(),
            max_waiting: self.max_waiting.clone(),
        }
    }
}

#[derive(Args, Debug)]
pub struct WorkLogArgs {
    #[arg(long)]
    pub task: Option<String>,
    /// Only events by this identity (public key or pseudonym).
    #[arg(long, alias = "agent")]
    pub actor: Option<String>,
    #[arg(long, env = "WORK_SCOPE")]
    pub scope: Option<String>,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,
}
