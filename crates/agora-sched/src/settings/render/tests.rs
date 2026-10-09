use crate::admit::MachineSnapshot;
use crate::settings::{MemBudget, MemLimit, WorkSettings};

use super::{settings_json, settings_text};

const EIGHT_CPUS_16_GIB_KIB: u64 = 16_777_216;

fn machine(cpus: f64, total: Option<u64>) -> MachineSnapshot {
    MachineSnapshot {
        cpus,
        total_memory_kib: total,
        disk_free_kib: None,
        signals: None,
    }
}

fn default_machine() -> MachineSnapshot {
    machine(8.0, Some(EIGHT_CPUS_16_GIB_KIB))
}
#[test]
fn st2_auto_memory_budget_is_total_minus_two_gib_above_two_gib_only() {
    let memory =
        |s: &WorkSettings, m: &MachineSnapshot| settings_json(s, m)["memory_budget_kib"].clone();
    let defaults = WorkSettings::default();
    assert_eq!(memory(&defaults, &default_machine()), 14_680_064);
    assert!(memory(&defaults, &machine(8.0, Some(2_097_152))).is_null());
    assert!(memory(&defaults, &machine(8.0, None)).is_null());
    let off = WorkSettings {
        memory_budget: MemBudget::Off,
        ..WorkSettings::default()
    };
    assert!(memory(&off, &default_machine()).is_null());
    let fixed = WorkSettings {
        memory_budget: MemBudget::Kib(1000),
        ..WorkSettings::default()
    };
    assert_eq!(memory(&fixed, &machine(8.0, None)), 1000);
}
#[test]
fn json_defaults_match_the_work_py_example_on_eight_cpus_and_16_gib() {
    let expected = serde_json::json!({
        "cpu_budget": 8.0,
        "cpu_budget_is_default": true,
        "capacity": null,
        "capacity_is_default": true,
        "cpus": 8,
        "idle_min": 10.0,
        "memory_limit": "warning",
        "memory_budget_kib": 14_680_064,
        "memory_budget_is_default": true,
        "load_factor": 1.5,
        "load_limit": 12.0,
        "load_wait": 1800.0,
        "log_limit": 268_435_456,
        "keep_days": 14.0,
        "backfill_wait": 300.0,
        "small_weight": 1.0,
        "aging": 1.0,
        "job_qos": "utility",
        "quiet_drain": 60.0,
        "quiet_max": 180.0,
        "self_accept": false,
        "timer_quota": 32,
        "disk_floor_kib": 5_242_880,
        "memory_floor_kib": null,
        "idle_floor_pct": null,
        "reconcile_interval_s": 30.0,
        "lease_ttl_s": 900.0,
        "claim_ttl_s": 1800.0,
        "run_role": "worker",
        "max_waiting": 64,
    });
    assert_eq!(
        settings_json(&WorkSettings::default(), &default_machine()),
        expected
    );
}

#[test]
fn json_reports_the_run_role_and_waiting_cap_as_set() {
    let settings = WorkSettings {
        run_role: "supervisor".to_owned(),
        max_waiting: 7,
        ..WorkSettings::default()
    };
    let doc = settings_json(&settings, &default_machine());
    assert_eq!(doc["run_role"], "supervisor");
    assert_eq!(doc["max_waiting"], 7);
}

#[test]
fn json_reports_overrides_and_fractional_cpus() {
    let settings = WorkSettings {
        cpu_budget: Some(4.0),
        capacity: Some(3.0),
        memory_budget: MemBudget::Off,
        memory_limit: MemLimit::Off,
        ..WorkSettings::default()
    };
    let doc = settings_json(&settings, &default_machine());
    assert_eq!(doc["cpu_budget"], 4.0);
    assert_eq!(doc["cpu_budget_is_default"].as_bool(), Some(false));
    assert_eq!(doc["capacity"], 3.0);
    assert_eq!(doc["capacity_is_default"].as_bool(), Some(false));
    assert!(doc["memory_budget_kib"].is_null());
    assert_eq!(doc["memory_budget_is_default"].as_bool(), Some(false));
    assert_eq!(doc["memory_limit"], "off");
    let fractional = settings_json(&WorkSettings::default(), &machine(2.5, None));
    assert_eq!(fractional["cpus"], 2.5);
    assert_eq!(fractional["cpu_budget"], 2.5);
    assert_eq!(fractional["load_limit"], 3.75);
}

#[test]
fn text_first_and_last_lines_on_the_default_machine() {
    let text = settings_text(&WorkSettings::default(), &default_machine(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 25);
    assert_eq!(
        lines[0],
        "cpu budget: 8 cores (the default: the effective CPU capacity); the running jobs' predicted cores fit it"
    );
    assert_eq!(
        lines[1],
        "capacity: off (the default): no cap on declared weights"
    );
    assert_eq!(
        lines[2],
        "memory budget: 14.0 GiB (the default: the memory minus 2 GiB); jobs start while used memory (active, wired and compressed) is at or below it"
    );
    assert_eq!(
        lines[4],
        "memory limit: warning; jobs start below warning memory pressure"
    );
    assert_eq!(
        lines[5],
        "load factor: 1.5; on Linux only, jobs start below load 12.00 on 8 effective CPUs"
    );
    assert_eq!(
        lines[14],
        "keep: 14 days of events and of the logs of ended tasks"
    );
    assert_eq!(
        lines[15],
        "self accept: off; a queued command needs an accepter other than its author"
    );
    assert_eq!(lines[16], "timer quota: 32 enabled timers per identity");
    assert_eq!(
        lines[17],
        "run role: worker; identities below it cannot run or accept commands"
    );
    assert_eq!(lines[18], "max waiting: 64 jobs per identity");
    assert_eq!(
        lines[19],
        "disk floor: 5.0 GiB; below it the reconciler pauses admission"
    );
    assert_eq!(lines[22], "reconcile interval: 30.0 s");
    assert_eq!(lines[23], "lease ttl: 15 min 0 s");
    assert_eq!(lines[24], "claim ttl: 30 min 0 s");
}

#[test]
fn text_marks_the_macos_only_qos_and_the_unknown_memory_budget() {
    let defaults = WorkSettings::default();
    let linux = settings_text(&defaults, &machine(8.0, None), false);
    let lines: Vec<&str> = linux.lines().collect();
    assert_eq!(
        lines[2],
        "memory budget: unknown here: the machine's memory cannot be read, so used memory is not a gate"
    );
    assert!(
        lines[11].ends_with("so the user's own work comes first (not applied here: macOS only)")
    );
    let mac = settings_text(&defaults, &default_machine(), true);
    assert!(
        mac.lines()
            .nth(11)
            .is_some_and(|line| !line.contains("not applied here"))
    );
}

#[test]
fn text_reports_overrides_and_absent_floors() {
    let settings = WorkSettings {
        cpu_budget: Some(4.0),
        capacity: Some(3.0),
        memory_budget: MemBudget::Off,
        self_accept: true,
        disk_floor_kib: None,
        ..WorkSettings::default()
    };
    let text = settings_text(&settings, &default_machine(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "cpu budget: 4 cores; the running jobs' predicted cores fit it"
    );
    assert_eq!(
        lines[1],
        "capacity: 3 cores; the running jobs' declared weights fit it"
    );
    assert_eq!(lines[2], "memory budget: off; used memory is not a gate");
    assert_eq!(
        lines[15],
        "self accept: on; a command's author may accept it too"
    );
    assert_eq!(
        lines[19],
        "disk floor: off; the reconciler does not pause admission for disk space"
    );
    assert_eq!(
        lines[20],
        "memory floor: off; the reconciler does not pause admission for memory"
    );
    assert_eq!(
        lines[21],
        "idle floor: off; the reconciler does not pause admission for idle CPU"
    );
}
