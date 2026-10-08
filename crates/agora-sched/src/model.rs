//! Shared serde data model of the scheduling layer: tasks, scopes, leases, pools, timers, notices, card extensions.
//!
//! Operation arguments live in `model_ops`, report shapes in `model_reports`; both are re-exported here so that
//! `agora_sched::model::AddArgs` and friends resolve. Pubkeys are 64-hex lowercase strings; `*_name` fields are
//! pseudonyms for display. Every `Option`/`Vec`/`bool` field defaults when absent so older JSON still parses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::model_ops::*;
pub use crate::model_reports::*;
use crate::predict::Prediction;
use crate::timer::TimerSpec;

/// Task lifecycle. `Done`, `Failed` and `Cancelled` are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Claimed,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Claimed => "claimed",
            TaskState::Running => "running",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => TaskState::Queued,
            "claimed" => TaskState::Claimed,
            "running" => TaskState::Running,
            "done" => TaskState::Done,
            "failed" => TaskState::Failed,
            "cancelled" => TaskState::Cancelled,
            _ => return None,
        })
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Cancelled
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Project,
    Conversation,
    Agent,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Project => "project",
            ScopeKind::Conversation => "conversation",
            ScopeKind::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "project" => ScopeKind::Project,
            "conversation" => ScopeKind::Conversation,
            "agent" => ScopeKind::Agent,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub seq: i64,
    pub id: String,
    pub title: String,
    pub kind: String,
    pub tier: u8,
    #[serde(default)]
    pub scope: Option<String>,
    pub author: String,
    pub author_name: String,
    pub state: TaskState,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub accepted_by: Option<String>,
    #[serde(default)]
    pub needs_acceptance: bool,
    #[serde(default)]
    pub after: Vec<String>,
    pub size: f64,
    pub value: f64,
    #[serde(default)]
    pub files: Vec<String>,
    pub weight: f64,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    pub queued_ms: i64,
    #[serde(default)]
    pub claimed_ms: Option<i64>,
    #[serde(default)]
    pub requested_ms: Option<i64>,
    #[serde(default)]
    pub started_ms: Option<i64>,
    #[serde(default)]
    pub ended_ms: Option<i64>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub supervisor_pid: Option<i64>,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub qos: Option<String>,
    #[serde(default)]
    pub prediction: Option<Prediction>,
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub rss_kib: Option<u64>,
    #[serde(default)]
    pub rss_peak_kib: Option<u64>,
    #[serde(default)]
    pub cpu_seconds: Option<f64>,
    #[serde(default)]
    pub sampled_ms: Option<i64>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub card: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub quiet_until_ms: Option<i64>,
    #[serde(default)]
    pub cancel_requested: bool,
}

impl TaskRecord {
    /// Seconds spent waiting for capacity: `(started | ended | now) - requested`, at least 0. None when the task
    /// never asked to run.
    pub fn waited_s(&self, now_ms: i64) -> Option<f64> {
        let requested = self.requested_ms?;
        let until = self.started_ms.or(self.ended_ms).unwrap_or(now_ms);
        Some(until.saturating_sub(requested).max(0) as f64 / 1000.0)
    }

    /// Seconds the command ran: `(ended | now) - started`, at least 0. None when it never started.
    pub fn ran_s(&self, now_ms: i64) -> Option<f64> {
        let started = self.started_ms?;
        let until = self.ended_ms.unwrap_or(now_ms);
        Some(until.saturating_sub(started).max(0) as f64 / 1000.0)
    }

    /// work.py `describe_state`, with pseudonyms for agents. The result is built from stored data and must still be
    /// passed through `pyfmt::printable` by text renderers.
    pub fn describe_state(&self) -> String {
        let claimant = || {
            self.agent_name
                .clone()
                .or_else(|| self.agent.clone())
                .unwrap_or_else(|| "nobody".to_string())
        };
        match self.state {
            TaskState::Claimed if self.requested_ms.is_some() => {
                format!("waiting for capacity (claimed by {})", claimant())
            }
            TaskState::Claimed => format!("claimed by {}", claimant()),
            TaskState::Running => match self.pid {
                Some(pid) => format!("running as process {pid} (claimed by {})", claimant()),
                None => "starting".to_string(),
            },
            TaskState::Failed => match self.status {
                Some(status) => format!("failed with status {status}"),
                None => "failed".to_string(),
            },
            other => other.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeRecord {
    pub id: String,
    pub kind: ScopeKind,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub updated_ms: i64,
    #[serde(default)]
    pub counts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextOutcome {
    #[serde(default)]
    pub claimed: Option<TaskRecord>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutcome {
    pub task: TaskRecord,
    /// Some when the task was running: the daemon must stop it.
    #[serde(default)]
    pub stop: Option<StopTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopTarget {
    pub supervisor_pid: i64,
    #[serde(default)]
    pub supervisor_start_s: Option<u64>,
    pub job_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkEvent {
    pub seq: i64,
    pub ts_ms: i64,
    pub kind: String,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    pub actor: String,
    pub actor_name: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskFilter {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub states: Vec<TaskState>,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default)]
    pub queued: u64,
    #[serde(default)]
    pub ready: u64,
    #[serde(default)]
    pub claimed: u64,
    #[serde(default)]
    pub running: u64,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub cancelled: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    Path,
    Pool,
    Quiet,
}

impl LeaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseKind::Path => "path",
            LeaseKind::Pool => "pool",
            LeaseKind::Quiet => "quiet",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "path" => LeaseKind::Path,
            "pool" => LeaseKind::Pool,
            "quiet" => LeaseKind::Quiet,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub id: String,
    pub kind: LeaseKind,
    pub resource: String,
    pub amount: f64,
    pub holder: String,
    pub holder_name: String,
    #[serde(default)]
    pub card: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    pub created_ms: i64,
    #[serde(default)]
    pub expires_ms: Option<i64>,
    pub heartbeat_ms: i64,
    #[serde(default)]
    pub expected_end_ms: Option<i64>,
    #[serde(default)]
    pub released_ms: Option<i64>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolRecord {
    pub name: String,
    pub capacity: f64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub used: f64,
    #[serde(default)]
    pub note: String,
    pub created_ms: i64,
    pub created_by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerRecord {
    pub id: String,
    pub owner: String,
    pub owner_name: String,
    pub spec: TimerSpec,
    pub template: AddArgs,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub next_fire_ms: Option<i64>,
    #[serde(default)]
    pub last_fire_ms: Option<i64>,
    #[serde(default)]
    pub fired: u32,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoticeRecord {
    pub seq: i64,
    pub ts_ms: i64,
    pub target: String,
    pub kind: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub team: String,
    pub lane: String,
    pub policy: crate::policy::TeamPolicy,
    pub updated_ms: i64,
    pub by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkRel {
    Re,
    Blocks,
    Relates,
    Duplicates,
    CausedBy,
    Job,
    Message,
    Note,
}

impl LinkRel {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkRel::Re => "re",
            LinkRel::Blocks => "blocks",
            LinkRel::Relates => "relates",
            LinkRel::Duplicates => "duplicates",
            LinkRel::CausedBy => "caused_by",
            LinkRel::Job => "job",
            LinkRel::Message => "message",
            LinkRel::Note => "note",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "re" => LinkRel::Re,
            "blocks" => LinkRel::Blocks,
            "relates" => LinkRel::Relates,
            "duplicates" => LinkRel::Duplicates,
            "caused_by" => LinkRel::CausedBy,
            "job" => LinkRel::Job,
            "message" => LinkRel::Message,
            "note" => LinkRel::Note,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardLink {
    pub rel: LinkRel,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardExt {
    pub id: String,
    #[serde(default)]
    pub priority: i64,
    pub value: f64,
    pub size: f64,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub needs_owner: bool,
    #[serde(default)]
    pub owner_action: String,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub links: Vec<CardLink>,
    #[serde(default)]
    pub jobs: Vec<String>,
}
