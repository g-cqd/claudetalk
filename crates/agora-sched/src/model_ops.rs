//! Arguments of signed scheduling operations: the `arg` of each op is `serde_json::to_string(&XxxArgs)`.

use serde::{Deserialize, Serialize};

use crate::model::{LeaseKind, LinkRel, ScopeKind, TaskState};
use crate::order::OrderPolicy;
use crate::policy::TeamPolicy;
use crate::settings::SettingsPatch;
use crate::timer::TimerSpec;

/// `Option<Option<T>>` where `Some(None)` is a real value (serialized as `null`) and an absent key is `None`.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeArgs {
    pub id: String,
    #[serde(default)]
    pub kind: Option<ScopeKind>,
    /// Some("") clears
    #[serde(default)]
    pub parent: Option<String>,
    /// Some("") clears
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddArgs {
    pub id: Option<String>,
    pub title: String,
    /// default "other"
    pub kind: String,
    pub scope: Option<String>,
    pub after: Vec<String>,
    /// default 1.0
    pub size: f64,
    /// default 1.0
    pub value: f64,
    pub files: Vec<String>,
    /// default 1.0
    pub weight: f64,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub card: Option<String>,
}

impl Default for AddArgs {
    fn default() -> Self {
        AddArgs {
            id: None,
            title: String::new(),
            kind: "other".to_string(),
            scope: None,
            after: Vec::new(),
            size: 1.0,
            value: 1.0,
            files: Vec::new(),
            weight: 1.0,
            command: None,
            cwd: None,
            card: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextArgs {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub policy: OrderPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceptArgs {
    pub digest: String,
}

/// work.run: exactly one of `task` (run a claimed+accepted task) or `add` (inline command; add.command required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunArgs {
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub add: Option<AddArgs>,
}

/// Done | Failed only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishArgs {
    pub state: TaskState,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelArgs {
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateArgs {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub size: Option<f64>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub after: Option<Vec<String>>,
    /// Some(None) unassigns (claimed -> queued), Some(Some(pk)) assigns (Supervisor+)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub agent: Option<Option<String>>,
}

/// Subject `task:<id>` | `lease:<id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReclaimArgs {
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsArgs {
    #[serde(default)]
    pub patch: SettingsPatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerArgs {
    #[serde(default)]
    pub id: Option<String>,
    pub spec: TimerSpec,
    pub template: AddArgs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolArgs {
    pub name: String,
    pub capacity: f64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseArgs {
    pub kind: LeaseKind,
    pub resource: String,
    /// pool units (Pool), ignored for Path
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub ttl_s: Option<f64>,
    #[serde(default)]
    pub card: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRenewArgs {
    #[serde(default)]
    pub ttl_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuietArgs {
    pub expected_end_ms: i64,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyArgs {
    pub team: String,
    pub lane: String,
    #[serde(default)]
    pub policy: TeamPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardUpdateArgs {
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub size: Option<f64>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub needs_owner: Option<bool>,
    #[serde(default)]
    pub owner_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardDepArgs {
    pub after: String,
    #[serde(default)]
    pub remove: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardLinkArgs {
    pub rel: LinkRel,
    pub target: String,
    #[serde(default)]
    pub remove: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EscalateArgs {
    pub owner_action: String,
    /// empty = the effective policy's escalation_chain, else supervisors
    #[serde(default)]
    pub to: Vec<String>,
}

// ---- system op args (signed by the daemon's system identity) ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStartArgs {
    pub pid: i64,
    pub supervisor_pid: i64,
    #[serde(default)]
    pub supervisor_start_s: Option<u64>,
    #[serde(default)]
    pub qos: String,
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub quiet_until_ms: Option<i64>,
    #[serde(default)]
    pub log: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobFinishArgs {
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub cpu_seconds: Option<f64>,
    #[serde(default)]
    pub rss_peak_kib: Option<u64>,
    pub ended_ms: i64,
    /// true when the job never started (load-wait fail, cwd gone before spawn, supervisor died early)
    #[serde(default)]
    pub before_start: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobNoteArgs {
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardSyncArgs {
    pub task: String,
    pub outcome: JobOutcome,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerFireArgs {
    pub fire_ms: i64,
}
