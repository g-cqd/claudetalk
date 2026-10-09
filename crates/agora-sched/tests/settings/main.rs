//! Work settings through agora-sched's public API (SPEC section 2.7; ref-admission section 11 and section 15 G):
//! size parsing (ST1), the memory budget on a 16 GiB and a 2 GiB machine (ST2), rejected values that leave the
//! settings untouched (ST3), the optional limits that off and auto clear (ST4), the agora keys, the change list,
//! serde of the stored types, the settings document and text, and `exclusive_at` (C5). The machine is a
//! `MachineSnapshot` literal, so no host load, clock or filesystem is read.

use agora_sched::admit::MachineSnapshot;
use agora_sched::predict::exclusive_at;
use agora_sched::settings::{
    MemBudget, MemLimit, Qos, SettingsPatch, WorkSettings, parse_size_kib, settings_json,
    settings_text,
};
use serde_json::{Map, Value, json};

/// The example machine of ref-admission section 11: 8 CPUs and 16 GiB of memory, no signals.
fn machine_16_gib() -> MachineSnapshot {
    machine_with_memory(Some(16_777_216))
}

fn machine_with_memory(total_memory_kib: Option<u64>) -> MachineSnapshot {
    MachineSnapshot {
        cpus: 8.0,
        total_memory_kib,
        disk_free_kib: None,
        signals: None,
    }
}

/// A patch from `(field, raw value)` pairs, named as the struct's fields (the same names the serde form uses).
fn patch(pairs: &[(&str, &str)]) -> SettingsPatch {
    let mut object = Map::new();
    for (key, value) in pairs {
        object.insert((*key).to_owned(), json!(value));
    }
    serde_json::from_value(Value::Object(object)).expect("every key is a SettingsPatch field")
}

/// Non-default settings, so a value that an operation wrongly writes is visible.
fn base_settings() -> WorkSettings {
    WorkSettings {
        idle_min: 12.0,
        memory_limit: MemLimit::Critical,
        job_qos: Qos::Background,
        aging: 0.5,
        ..WorkSettings::default()
    }
}

/// Applies one invalid value, placed after valid values of an early key and of late keys, to a non-default base.
/// The rejection must name the flag, and the stored settings must be exactly the base: nothing is applied in part.
fn assert_rejected_and_unchanged(key: &str, value: &str, flag: &str) {
    let before = base_settings();
    let mut settings = before.clone();
    // The bad key is inserted last, so it replaces a companion value of the same key (cpu_budget).
    let companions = [
        ("cpu_budget", "4"),
        ("self_accept", "true"),
        ("timer_quota", "7"),
        ("lease_ttl", "60"),
        (key, value),
    ];
    let bad = patch(&companions);
    let err = settings
        .apply(&bad)
        .expect_err("an invalid value must be rejected");
    assert!(
        err.starts_with(&format!("{flag}: ")),
        "{key}={value:?} should be reported as {flag}, got {err:?}"
    );
    assert_eq!(
        settings, before,
        "{key}={value:?} was rejected, so nothing may be applied"
    );
}

mod agora_keys;
mod change_defaults;
mod size_budget;
mod text_c5;
