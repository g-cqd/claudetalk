//! Work settings: the typed settings row, raw CLI-style patches and their rendering (ref-admission section 11).

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod parse;
mod render;
#[cfg(test)]
mod tests;

pub use render::{settings_json, settings_text};

/// Replaces the stored field with the parsed patch value, or leaves it, and records "key raw" for the change list.
/// `$key` is the patch field (flag) name, `$field` the stored field.
macro_rules! set_field {
    ($patch:ident, $next:ident, $changes:ident, $key:ident => $field:ident, $parse:path) => {
        if let Some(raw) = &$patch.$key {
            $next.$field = $parse(raw).map_err(|e| parse::flag_error(stringify!($key), &e))?;
            $changes.push(format!("{} {raw}", stringify!($key)));
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemLimit {
    #[default]
    Warning,
    Critical,
    Off,
}

impl MemLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            MemLimit::Warning => "warning",
            MemLimit::Critical => "critical",
            MemLimit::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Qos {
    #[default]
    Utility,
    Default,
    Background,
}

impl Qos {
    pub fn as_str(self) -> &'static str {
        match self {
            Qos::Utility => "utility",
            Qos::Default => "default",
            Qos::Background => "background",
        }
    }
}

/// serde: "auto" | "off" | integer KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemBudget {
    #[default]
    Auto,
    Off,
    Kib(u64),
}

impl Serialize for MemBudget {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MemBudget::Auto => s.serialize_str("auto"),
            MemBudget::Off => s.serialize_str("off"),
            MemBudget::Kib(k) => s.serialize_u64(*k),
        }
    }
}

struct MemBudgetVisitor;

impl Visitor<'_> for MemBudgetVisitor {
    type Value = MemBudget;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"auto\", \"off\" or a non-negative integer of KiB")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<MemBudget, E> {
        match v {
            "auto" => Ok(MemBudget::Auto),
            "off" => Ok(MemBudget::Off),
            other => Err(E::invalid_value(de::Unexpected::Str(other), &self)),
        }
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<MemBudget, E> {
        Ok(MemBudget::Kib(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<MemBudget, E> {
        u64::try_from(v)
            .map(MemBudget::Kib)
            .map_err(|_| E::invalid_value(de::Unexpected::Signed(v), &self))
    }
}

impl<'de> Deserialize<'de> for MemBudget {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(MemBudgetVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkSettings {
    pub cpu_budget: Option<f64>,
    pub capacity: Option<f64>,
    pub memory_budget: MemBudget,
    pub idle_min: f64,
    pub memory_limit: MemLimit,
    pub load_factor: f64,
    pub load_wait: f64,
    pub log_limit: u64,
    pub keep_days: f64,
    pub backfill_wait: f64,
    pub small_weight: f64,
    pub aging: f64,
    pub job_qos: Qos,
    pub quiet_drain: f64,
    pub quiet_max: f64,
    /// agora: accepter may equal author (D5). default false
    pub self_accept: bool,
    /// default 32
    pub timer_quota: u32,
    /// reconciler floors; default disk 5 GiB (5242880 KiB), memory None, idle None
    pub disk_floor_kib: Option<u64>,
    pub memory_floor_kib: Option<u64>,
    pub idle_floor_pct: Option<f64>,
    /// default 30.0
    pub reconcile_interval_s: f64,
    /// path/pool lease default TTL, default 900.0
    pub lease_ttl_s: f64,
    /// agora (M6): a task that stays `claimed` without a run request for this long goes back to the
    /// queue (reconciler rule R7); default 1800.0, finite and at least 60.0. Old rows without the field
    /// deserialize to the default.
    pub claim_ttl_s: f64,
    /// agora: lowest role that may run or accept commands (worker|supervisor|controller|owner); default "worker"
    pub run_role: String,
    /// agora: jobs one identity may hold claimed, requested or waiting to start, at least 1; default 64
    pub max_waiting: u32,
}

/// The roles `run_role` may name, lowest first.
const RUN_ROLES: [&str; 4] = ["worker", "supervisor", "controller", "owner"];

/// The shortest claim TTL in seconds: a shorter one makes every claim due at the next reconcile pass.
const CLAIM_TTL_MIN_S: f64 = 60.0;

impl Default for WorkSettings {
    fn default() -> Self {
        WorkSettings {
            cpu_budget: None,
            capacity: None,
            memory_budget: MemBudget::Auto,
            idle_min: 10.0,
            memory_limit: MemLimit::Warning,
            load_factor: 1.5,
            load_wait: 1800.0,
            log_limit: 268_435_456,
            keep_days: 14.0,
            backfill_wait: 300.0,
            small_weight: 1.0,
            aging: 1.0,
            job_qos: Qos::Utility,
            quiet_drain: 60.0,
            quiet_max: 180.0,
            self_accept: false,
            timer_quota: 32,
            disk_floor_kib: Some(5_242_880),
            memory_floor_kib: None,
            idle_floor_pct: None,
            reconcile_interval_s: 30.0,
            lease_ttl_s: 900.0,
            claim_ttl_s: 1800.0,
            run_role: "worker".to_owned(),
            max_waiting: 64,
        }
    }
}

/// Raw CLI-style values (strings), every field optional; parsed with work.py rules (ref-admission section 11
/// "Parser details").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsPatch {
    pub cpu_budget: Option<String>,
    pub capacity: Option<String>,
    pub memory_budget: Option<String>,
    pub idle_min: Option<String>,
    pub memory_limit: Option<String>,
    pub load_factor: Option<String>,
    pub load_wait: Option<String>,
    pub log_limit: Option<String>,
    pub keep_days: Option<String>,
    pub backfill_wait: Option<String>,
    pub small_weight: Option<String>,
    pub aging: Option<String>,
    pub job_qos: Option<String>,
    pub quiet_drain: Option<String>,
    pub quiet_max: Option<String>,
    pub self_accept: Option<String>,
    pub timer_quota: Option<String>,
    pub disk_floor: Option<String>,
    pub memory_floor: Option<String>,
    pub idle_floor: Option<String>,
    pub reconcile_interval: Option<String>,
    pub lease_ttl: Option<String>,
    pub claim_ttl: Option<String>,
    pub run_role: Option<String>,
    pub max_waiting: Option<String>,
}

impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        *self == SettingsPatch::default()
    }
}

fn ensure(ok: bool, message: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message.to_owned()) }
}

fn is_positive(v: f64) -> bool {
    v.is_finite() && v > 0.0
}

fn is_zero_or_more(v: f64) -> bool {
    v.is_finite() && v >= 0.0
}

fn is_percent(v: f64) -> bool {
    (0.0..100.0).contains(&v)
}

/// A claim TTL: a finite number of seconds, `CLAIM_TTL_MIN_S` or more.
fn parse_claim_ttl(text: &str) -> Result<f64, String> {
    let value = parse::duration(text)?;
    if value >= CLAIM_TTL_MIN_S {
        Ok(value)
    } else {
        Err(format!(
            "{} is not at least {CLAIM_TTL_MIN_S} seconds",
            crate::pyfmt::repr_str(text)
        ))
    }
}

impl WorkSettings {
    /// Checks the stored values against the same rules `apply` enforces (used when a row is read back).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(v) = self.cpu_budget {
            ensure(
                is_positive(v),
                "cpu_budget must be a finite number above zero",
            )?;
        }
        if let Some(v) = self.capacity {
            ensure(
                is_positive(v),
                "capacity must be a finite number above zero",
            )?;
        }
        ensure(
            self.memory_budget != MemBudget::Kib(0),
            "memory_budget must be auto, off or more than 0 KiB",
        )?;
        ensure(
            is_percent(self.idle_min),
            "idle_min must be at least 0 and below 100",
        )?;
        ensure(
            is_positive(self.load_factor),
            "load_factor must be a finite number above zero",
        )?;
        ensure(
            is_positive(self.load_wait),
            "load_wait must be a finite number above zero",
        )?;
        ensure(self.log_limit >= 1, "log_limit must be at least 1")?;
        ensure(
            is_positive(self.keep_days),
            "keep_days must be a finite number above zero",
        )?;
        ensure(
            is_positive(self.backfill_wait),
            "backfill_wait must be a finite number above zero",
        )?;
        ensure(
            is_positive(self.small_weight),
            "small_weight must be a finite number above zero",
        )?;
        ensure(
            is_zero_or_more(self.aging),
            "aging must be a finite number, zero or more",
        )?;
        ensure(
            is_zero_or_more(self.quiet_drain),
            "quiet_drain must be a finite number, zero or more",
        )?;
        ensure(
            is_zero_or_more(self.quiet_max),
            "quiet_max must be a finite number, zero or more",
        )?;
        ensure(self.timer_quota >= 1, "timer_quota must be at least 1")?;
        ensure(
            self.disk_floor_kib != Some(0),
            "disk_floor must be off or more than 0 KiB",
        )?;
        ensure(
            self.memory_floor_kib != Some(0),
            "memory_floor must be off or more than 0 KiB",
        )?;
        if let Some(v) = self.idle_floor_pct {
            ensure(is_percent(v), "idle_floor must be at least 0 and below 100")?;
        }
        ensure(
            self.reconcile_interval_s.is_finite() && self.reconcile_interval_s >= 1.0,
            "reconcile_interval must be a finite number, one second or more",
        )?;
        ensure(
            is_positive(self.lease_ttl_s),
            "lease_ttl must be a finite number above zero",
        )?;
        ensure(
            self.claim_ttl_s.is_finite() && self.claim_ttl_s >= CLAIM_TTL_MIN_S,
            "claim_ttl must be a finite number, 60 seconds or more",
        )?;
        ensure(
            RUN_ROLES.contains(&self.run_role.as_str()),
            "run_role must be one of worker, supervisor, controller, owner",
        )?;
        ensure(self.max_waiting >= 1, "max_waiting must be at least 1")
    }

    /// Apply a patch; Err(work.py message) on any invalid value (nothing applied); Ok(list of "key value" changes).
    pub fn apply(&mut self, patch: &SettingsPatch) -> Result<Vec<String>, String> {
        let mut next = self.clone();
        let mut changes = Vec::new();
        set_field!(patch, next, changes, cpu_budget => cpu_budget, parse::cpu_budget);
        set_field!(patch, next, changes, capacity => capacity, parse::capacity);
        set_field!(patch, next, changes, memory_budget => memory_budget, parse::memory_budget);
        set_field!(patch, next, changes, idle_min => idle_min, parse::idle_min);
        set_field!(patch, next, changes, memory_limit => memory_limit, parse::memory_limit);
        set_field!(patch, next, changes, load_factor => load_factor, parse::positive);
        set_field!(patch, next, changes, load_wait => load_wait, parse::positive);
        set_field!(patch, next, changes, log_limit => log_limit, parse::log_limit);
        set_field!(patch, next, changes, keep_days => keep_days, parse::positive);
        set_field!(patch, next, changes, backfill_wait => backfill_wait, parse::positive);
        set_field!(patch, next, changes, small_weight => small_weight, parse::positive);
        set_field!(patch, next, changes, aging => aging, parse::duration);
        set_field!(patch, next, changes, job_qos => job_qos, parse::job_qos);
        set_field!(patch, next, changes, quiet_drain => quiet_drain, parse::duration);
        set_field!(patch, next, changes, quiet_max => quiet_max, parse::duration);
        set_field!(patch, next, changes, self_accept => self_accept, parse::self_accept);
        set_field!(patch, next, changes, timer_quota => timer_quota, parse::timer_quota);
        set_field!(patch, next, changes, disk_floor => disk_floor_kib, parse::size_floor);
        set_field!(patch, next, changes, memory_floor => memory_floor_kib, parse::size_floor);
        set_field!(patch, next, changes, idle_floor => idle_floor_pct, parse::idle_floor);
        set_field!(patch, next, changes, reconcile_interval => reconcile_interval_s, parse::reconcile_interval);
        set_field!(patch, next, changes, lease_ttl => lease_ttl_s, parse::positive);
        set_field!(patch, next, changes, claim_ttl => claim_ttl_s, parse_claim_ttl);
        set_field!(patch, next, changes, run_role => run_role, parse::run_role);
        set_field!(patch, next, changes, max_waiting => max_waiting, parse::max_waiting);
        *self = next;
        Ok(changes)
    }
}

/// SIZE: (\d+(\.\d+)?)\s*([KMGT])(i?B)? case-insensitive -> KiB (round half-even), > 0.
pub fn parse_size_kib(text: &str) -> Result<u64, String> {
    parse::size_kib(text, "is not a size with its unit, as 14G or 14336M")
}
