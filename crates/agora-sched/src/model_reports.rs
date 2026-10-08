//! Report and wait shapes returned by the scheduling queries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::admit::MemLevel;
use crate::model::{LeaseRecord, NoticeRecord, ScopeRecord, TaskRecord};
use crate::settings::MemLimit;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitSpec {
    Tasks {
        ids: Vec<String>,
        #[serde(default)]
        any: bool,
    },
    All {
        #[serde(default)]
        scope: Option<String>,
    },
    Cards {
        ids: Vec<String>,
        #[serde(default)]
        states: Vec<String>,
    },
    Messages {
        channel: String,
        #[serde(default)]
        after: i64,
    },
    Notices {
        #[serde(default)]
        after: i64,
    },
    Pid {
        pids: Vec<i64>,
    },
    File {
        paths: Vec<String>,
    },
    Gone {
        paths: Vec<String>,
    },
    Log {
        path: String,
        pattern: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResult {
    pub status: i32,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub condition: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub tail: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuietView {
    /// window | drain | manual
    pub state: String,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub lease: Option<String>,
    pub seconds_left: f64,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineView {
    pub cpus: f64,
    #[serde(default)]
    pub idle: Option<f64>,
    pub idle_min: f64,
    #[serde(default)]
    pub memory: Option<MemLevel>,
    #[serde(default)]
    pub memory_used_kib: Option<u64>,
    #[serde(default)]
    pub memory_budget_kib: Option<u64>,
    pub memory_limit: MemLimit,
    #[serde(default)]
    pub load: Option<f64>,
    #[serde(default)]
    pub load_gate: bool,
    pub load_limit: f64,
    pub cpu_budget: f64,
    pub used: f64,
    #[serde(default)]
    pub capacity: Option<f64>,
    pub declared: f64,
    pub jobs: u32,
    #[serde(default)]
    pub holds: Option<String>,
    #[serde(default)]
    pub quiet: Option<QuietView>,
    #[serde(default)]
    pub paused: Option<String>,
    #[serde(default)]
    pub disk_free_kib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Swept {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub time_ms: i64,
    #[serde(default)]
    pub scope: Option<String>,
    pub machine: MachineView,
    #[serde(default)]
    pub scopes: Vec<ScopeRecord>,
    #[serde(default)]
    pub running: Vec<TaskRecord>,
    #[serde(default)]
    pub waiting: Vec<TaskRecord>,
    #[serde(default)]
    pub counts: crate::model::Counts,
    #[serde(default)]
    pub swept: Vec<Swept>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleCard {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub owner: Option<String>,
    pub state: String,
    pub idle_s: f64,
    pub stage: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WipView {
    pub team: String,
    pub lane: String,
    pub count: u32,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeedsOwner {
    pub card: String,
    pub title: String,
    #[serde(default)]
    pub owner_action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchdogReport {
    pub now_ms: i64,
    #[serde(default)]
    pub stale: Vec<StaleCard>,
    #[serde(default)]
    pub expiring_leases: Vec<LeaseRecord>,
    #[serde(default)]
    pub notices: Vec<NoticeRecord>,
    #[serde(default)]
    pub needs_owner: Vec<NeedsOwner>,
    #[serde(default)]
    pub wip: Vec<WipView>,
    #[serde(default)]
    pub paused: Option<String>,
    #[serde(default)]
    pub orphaned_jobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub since_ms: i64,
    pub until_ms: i64,
    #[serde(default)]
    pub cards_done: u64,
    #[serde(default)]
    pub lead_time_p50_s: Option<f64>,
    #[serde(default)]
    pub lead_time_p90_s: Option<f64>,
    #[serde(default)]
    pub cycle_time_p50_s: Option<f64>,
    #[serde(default)]
    pub cycle_time_p90_s: Option<f64>,
    #[serde(default)]
    pub wip: BTreeMap<String, u64>,
    #[serde(default)]
    pub blocked_time_total_s: f64,
    #[serde(default)]
    pub blocked_time_avg_s: Option<f64>,
    #[serde(default)]
    pub jobs_done: u64,
    #[serde(default)]
    pub jobs_failed: u64,
    #[serde(default)]
    pub queue_wait_p50_s: Option<f64>,
    #[serde(default)]
    pub queue_wait_p90_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardExtras {
    #[serde(default)]
    pub needs_owner: Vec<NeedsOwner>,
    #[serde(default)]
    pub wip: Vec<WipView>,
    #[serde(default)]
    pub running_jobs: u32,
    #[serde(default)]
    pub waiting_jobs: u32,
    #[serde(default)]
    pub queued_tasks: u32,
    #[serde(default)]
    pub open_leases: u32,
    #[serde(default)]
    pub quiet: Option<QuietView>,
    #[serde(default)]
    pub paused: Option<String>,
}
