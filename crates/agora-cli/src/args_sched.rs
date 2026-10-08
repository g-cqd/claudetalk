//! clap declarations of `work schedule`, `pool`, `quiet` and `policy` (M5). `main.rs` re-exports them with the
//! rest of the scheduling arguments.

use crate::WorkAddArgs;
use clap::{Args, Subcommand};

/// Parsed once at startup, so the size of the largest variant does not matter.
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ScheduleCmd {
    /// Queue a task on a schedule (at a time, every interval or by cron expression, all UTC).
    Add(ScheduleAddArgs),
    /// List timers.
    List {
        /// Only this owner (public key).
        #[arg(long)]
        owner: Option<String>,
    },
    /// Delete a timer.
    Rm { id: String },
}

#[derive(Args, Debug)]
pub struct ScheduleAddArgs {
    #[command(flatten)]
    pub when: ScheduleWhen,
    /// The task to queue each time (`--id` names the timer; tasks get `<timer>-<n>`).
    #[command(flatten)]
    pub task: WorkAddArgs,
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct ScheduleWhen {
    /// Once, at epoch milliseconds or an RFC 3339 time such as 2026-01-02T03:04:05Z.
    #[arg(long)]
    pub at: Option<String>,
    /// Every N seconds, or Nm / Nh / Nd (at least 60 s).
    #[arg(long)]
    pub every: Option<String>,
    /// Five fields: minute hour day-of-month month day-of-week (UTC).
    #[arg(long)]
    pub cron: Option<String>,
}

/// `agora pool ...`: capacity pools and leases.
#[derive(Subcommand, Debug)]
pub enum PoolCmd {
    /// Define or resize a pool (controller or above).
    Define {
        name: String,
        #[arg(long)]
        capacity: f64,
        #[arg(long, default_value = "")]
        unit: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    List,
    /// Lease units of a pool (`--pool`) or a path (`--path`).
    Lease {
        #[arg(long, conflicts_with = "path", required_unless_present = "path")]
        pool: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        amount: f64,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        ttl_secs: Option<f64>,
        #[arg(long)]
        card: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Extend a lease you hold.
    Renew {
        lease: String,
        #[arg(long)]
        ttl_secs: Option<f64>,
    },
    Release {
        lease: String,
    },
    /// List leases.
    Leases {
        /// path | pool | quiet
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        holder: Option<String>,
        /// Only leases that are neither released nor expired.
        #[arg(long)]
        active: bool,
    },
}

/// `agora quiet ...`: manual quiet windows (controller or above).
#[derive(Subcommand, Debug)]
pub enum QuietCmd {
    /// Open a window: no job predicted above the small weight starts while it is open.
    Open {
        /// Expected length in seconds, between 1 and 86400 (`--for` is the same flag). Or give `--until`.
        #[arg(
            long = "for-secs",
            visible_alias = "for",
            conflicts_with = "until",
            required_unless_present = "until"
        )]
        for_secs: Option<u64>,
        /// The time the window ends (epoch milliseconds or RFC 3339 UTC), one second to one day from now. Or give
        /// `--for-secs`.
        #[arg(long, conflicts_with = "for_secs")]
        until: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    Close {
        lease: String,
    },
    /// Open quiet windows.
    Status,
}

/// `agora policy ...`: team and lane policies.
#[derive(Subcommand, Debug)]
pub enum PolicyCmd {
    /// Set the policy of a team and lane (`*` = any). Unset flags keep inheriting.
    Set {
        team: String,
        lane: String,
        /// Seconds without activity before the owner is told.
        #[arg(long)]
        stale_after: Option<f64>,
        #[arg(long)]
        escalate_after: Option<f64>,
        #[arg(long)]
        reclaim_after: Option<f64>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        reclaim: Option<bool>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        require_review: Option<bool>,
        #[arg(long)]
        wip_limit: Option<u32>,
        /// Public keys notified in order at escalation (controller or above).
        #[arg(long, num_args = 1..)]
        escalation_chain: Option<Vec<String>>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        sprint_rollover: Option<bool>,
    },
    List,
}
