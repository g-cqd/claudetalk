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

// ST1: size parsing.

#[test]
fn st1_sizes_convert_to_binary_kib_for_each_unit_spelling() {
    let cases = [
        ("14G", 14_680_064),
        ("14GiB", 14_680_064),
        ("14GB", 14_680_064),
        ("14336M", 14_680_064),
        ("1.5G", 1_572_864),
        ("512k", 512),
        ("1T", 1_073_741_824),
    ];
    for (text, kib) in cases {
        assert_eq!(parse_size_kib(text), Ok(kib), "{text:?}");
    }
}

#[test]
fn st1_unit_letters_and_the_b_suffix_are_case_insensitive() {
    let cases = [
        ("14g", 14_680_064),
        ("14gib", 14_680_064),
        ("14GIB", 14_680_064),
        ("14gB", 14_680_064),
        ("14336m", 14_680_064),
        ("512K", 512),
    ];
    for (text, kib) in cases {
        assert_eq!(parse_size_kib(text), Ok(kib), "{text:?}");
    }
}

#[test]
fn st1_whitespace_around_the_text_and_between_number_and_unit_is_allowed() {
    assert_eq!(parse_size_kib("14 G"), Ok(14_680_064));
    assert_eq!(parse_size_kib("  14G  "), Ok(14_680_064));
    assert_eq!(parse_size_kib("\t1.5\tGiB\n"), Ok(1_572_864));
}

#[test]
fn st1_fractional_kib_round_half_to_even() {
    assert_eq!(parse_size_kib("2.5K"), Ok(2));
    assert_eq!(parse_size_kib("3.5K"), Ok(4));
    assert_eq!(parse_size_kib("0.5M"), Ok(512));
}

#[test]
fn st1_a_bare_number_is_an_error_naming_the_missing_unit() {
    let err = parse_size_kib("14").expect_err("14 has no unit");
    assert!(err.contains("is not a size with its unit"), "{err}");
}

#[test]
fn st1_a_size_that_rounds_to_zero_or_is_zero_is_an_error() {
    for text in ["0K", "0G", "0.4K", "0.5K"] {
        let err = parse_size_kib(text).expect_err("zero KiB is not a size");
        assert!(err.contains("is not greater than zero"), "{text:?}: {err}");
    }
}

#[test]
fn st1_malformed_and_oversized_sizes_are_rejected() {
    let bad = [
        "",
        "   ",
        "G",
        "-1G",
        "+1G",
        "1.G",
        ".5G",
        "14 GB extra",
        "14Gi",
        "1e3K",
        "14 Gx",
        "1_000K",
        "99999999999999999999T",
    ];
    for text in bad {
        assert!(parse_size_kib(text).is_err(), "{text:?} must be rejected");
    }
}

// ST2: the memory budget in settings_json on a 16 GiB and a 2 GiB machine.

#[test]
fn st2_auto_memory_budget_is_the_total_minus_two_gib_on_a_16_gib_machine() {
    let settings = WorkSettings::default();
    let document = settings_json(&settings, &machine_16_gib());
    assert_eq!(document["memory_budget_kib"], json!(14_680_064));
    assert_eq!(document["memory_budget_is_default"], json!(true));
}

#[test]
fn st2_off_memory_budget_is_null_and_not_the_default_on_a_16_gib_machine() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("memory_budget", "off")]))
        .expect("off is valid");
    let document = settings_json(&settings, &machine_16_gib());
    assert_eq!(document["memory_budget_kib"], Value::Null);
    assert_eq!(document["memory_budget_is_default"], json!(false));
}

#[test]
fn st2_explicit_memory_budget_is_reported_as_given_not_reduced_by_the_reserve() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("memory_budget", "512M")]))
        .expect("512M is valid");
    let document = settings_json(&settings, &machine_16_gib());
    assert_eq!(document["memory_budget_kib"], json!(524_288));
    assert_eq!(document["memory_budget_is_default"], json!(false));
}

#[test]
fn st2_auto_memory_budget_is_none_on_a_2_gib_or_smaller_or_unknown_machine() {
    let settings = WorkSettings::default();
    for total in [Some(2_097_152), Some(1_048_576), None] {
        let document = settings_json(&settings, &machine_with_memory(total));
        assert_eq!(
            document["memory_budget_kib"],
            Value::Null,
            "total {total:?}"
        );
    }
}

#[test]
fn st2_auto_memory_budget_is_one_kib_just_above_the_2_gib_reserve() {
    let document = settings_json(
        &WorkSettings::default(),
        &machine_with_memory(Some(2_097_153)),
    );
    assert_eq!(document["memory_budget_kib"], json!(1));
}

// ST3: every invalid value of the list is rejected, and apply is all or nothing.

#[test]
fn st3_every_listed_invalid_work_value_is_rejected_and_nothing_is_applied() {
    let cases = [
        ("capacity", "0", "--capacity"),
        ("capacity", "lots", "--capacity"),
        ("cpu_budget", "0", "--cpu-budget"),
        ("cpu_budget", "off", "--cpu-budget"),
        ("job_qos", "maintenance", "--job-qos"),
        ("aging", "-1", "--aging"),
        ("quiet_drain", "-1", "--quiet-drain"),
        ("quiet_max", "nan", "--quiet-max"),
        ("load_factor", "-1", "--load-factor"),
        ("load_wait", "nan", "--load-wait"),
        ("log_limit", "0", "--log-limit"),
        ("keep_days", "inf", "--keep-days"),
        ("idle_min", "100", "--idle-min"),
        ("memory_limit", "high", "--memory-limit"),
        ("memory_budget", "14", "--memory-budget"),
    ];
    for (key, value, flag) in cases {
        assert_rejected_and_unchanged(key, value, flag);
    }
}

// ST4: capacity off and auto store no cap; cpu-budget auto stores no budget.

#[test]
fn st4_capacity_off_and_auto_store_no_cap_while_a_number_sets_it() {
    for word in ["off", "auto"] {
        let mut settings = WorkSettings {
            capacity: Some(3.0),
            ..WorkSettings::default()
        };
        settings
            .apply(&patch(&[("capacity", word)]))
            .expect("off and auto are valid");
        assert_eq!(settings.capacity, None, "capacity {word}");
        let document = settings_json(&settings, &machine_16_gib());
        assert_eq!(document["capacity"], Value::Null, "capacity {word}");
        assert_eq!(document["capacity_is_default"], json!(true));
    }
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("capacity", "2.5")]))
        .expect("2.5 is valid");
    assert_eq!(settings.capacity, Some(2.5));
}

#[test]
fn st4_cpu_budget_auto_stores_no_budget_and_reports_the_cpus() {
    let mut settings = WorkSettings {
        cpu_budget: Some(4.0),
        ..WorkSettings::default()
    };
    settings
        .apply(&patch(&[("cpu_budget", "auto")]))
        .expect("auto is valid");
    assert_eq!(settings.cpu_budget, None);
    let document = settings_json(&settings, &machine_16_gib());
    assert_eq!(document["cpu_budget"], json!(8.0));
    assert_eq!(document["cpu_budget_is_default"], json!(true));
}

// The agora keys: each one valid and invalid.

#[test]
fn agora_self_accept_takes_true_or_false_only() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("self_accept", "true")]))
        .expect("true is valid");
    assert!(settings.self_accept);
    settings
        .apply(&patch(&[("self_accept", "false")]))
        .expect("false is valid");
    assert!(!settings.self_accept);
    for word in ["yes", "1", "on"] {
        let err = settings
            .apply(&patch(&[("self_accept", word)]))
            .expect_err("only true and false are booleans here");
        assert!(err.starts_with("--self-accept: "), "{word}: {err}");
    }
}

#[test]
fn agora_timer_quota_is_a_whole_count_from_one_to_u32_max() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("timer_quota", "1")]))
        .expect("1 is valid");
    assert_eq!(settings.timer_quota, 1);
    settings
        .apply(&patch(&[("timer_quota", "4294967295")]))
        .expect("u32::MAX is valid");
    assert_eq!(settings.timer_quota, u32::MAX);
    for (text, reason) in [
        ("0", "is less than 1"),
        ("-1", "is less than 1"),
        ("2.5", "is not a whole number"),
        ("4294967296", "is too large"),
    ] {
        let err = settings
            .apply(&patch(&[("timer_quota", text)]))
            .expect_err("not a timer quota");
        assert!(err.contains(reason), "{text}: {err}");
        assert_eq!(settings.timer_quota, u32::MAX, "{text} must not be stored");
    }
}

#[test]
fn agora_run_role_is_one_of_the_four_roles_and_nothing_else_is_stored() {
    let mut settings = WorkSettings::default();
    for role in ["worker", "supervisor", "controller", "owner"] {
        settings
            .apply(&patch(&[("run_role", role)]))
            .expect("a listed role is valid");
        assert_eq!(settings.run_role, role);
    }
    for text in ["Owner", "root", "", "worker "] {
        let err = settings
            .apply(&patch(&[("run_role", text)]))
            .expect_err("not a listed role");
        assert!(
            err.starts_with("--run-role: ")
                && err.ends_with(" is not one of worker, supervisor, controller, owner"),
            "{text}: {err}"
        );
        assert_eq!(settings.run_role, "owner", "{text:?} must not be stored");
    }
}

#[test]
fn agora_max_waiting_is_a_whole_count_from_one_to_u32_max() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("max_waiting", "1")]))
        .expect("1 is valid");
    assert_eq!(settings.max_waiting, 1);
    settings
        .apply(&patch(&[("max_waiting", "4294967295")]))
        .expect("u32::MAX is valid");
    assert_eq!(settings.max_waiting, u32::MAX);
    for (text, reason) in [
        ("0", "is less than 1"),
        ("-1", "is less than 1"),
        ("2.5", "is not a whole number"),
        ("4294967296", "is too large"),
    ] {
        let err = settings
            .apply(&patch(&[("max_waiting", text)]))
            .expect_err("not a waiting cap");
        assert!(err.starts_with("--max-waiting: "), "{text}: {err}");
        assert!(err.contains(reason), "{text}: {err}");
        assert_eq!(settings.max_waiting, u32::MAX, "{text} must not be stored");
    }
}

#[test]
fn validate_refuses_a_stored_run_role_or_waiting_cap_that_apply_would_refuse() {
    assert_eq!(WorkSettings::default().validate(), Ok(()));
    let bad_role = WorkSettings {
        run_role: "root".to_owned(),
        ..WorkSettings::default()
    };
    assert_eq!(
        bad_role.validate(),
        Err("run_role must be one of worker, supervisor, controller, owner".to_owned())
    );
    let no_cap = WorkSettings {
        max_waiting: 0,
        ..WorkSettings::default()
    };
    assert_eq!(
        no_cap.validate(),
        Err("max_waiting must be at least 1".to_owned())
    );
}

#[test]
fn agora_disk_floor_is_off_or_a_size_and_a_size_below_one_kib_is_rejected() {
    let mut settings = WorkSettings::default();
    assert_eq!(settings.disk_floor_kib, Some(5_242_880), "default 5 GiB");
    settings
        .apply(&patch(&[("disk_floor", "off")]))
        .expect("off is valid");
    assert_eq!(settings.disk_floor_kib, None);
    settings
        .apply(&patch(&[("disk_floor", "5G")]))
        .expect("5G is valid");
    assert_eq!(settings.disk_floor_kib, Some(5_242_880));
    for text in ["1", "0.4K"] {
        let err = settings
            .apply(&patch(&[("disk_floor", text)]))
            .expect_err("not a floor");
        assert!(err.starts_with("--disk-floor: "), "{text}: {err}");
    }
}

#[test]
fn agora_memory_floor_is_off_or_a_size_and_defaults_to_none() {
    let mut settings = WorkSettings::default();
    assert_eq!(settings.memory_floor_kib, None);
    settings
        .apply(&patch(&[("memory_floor", "1.5M")]))
        .expect("1.5M is valid");
    assert_eq!(settings.memory_floor_kib, Some(1536));
    settings
        .apply(&patch(&[("memory_floor", "off")]))
        .expect("off is valid");
    assert_eq!(settings.memory_floor_kib, None);
    let err = settings
        .apply(&patch(&[("memory_floor", "512")]))
        .expect_err("a bare number has no unit");
    assert!(err.starts_with("--memory-floor: "), "{err}");
}

#[test]
fn agora_idle_floor_is_off_or_a_percent_from_zero_up_to_below_100() {
    let mut settings = WorkSettings::default();
    settings
        .apply(&patch(&[("idle_floor", "0")]))
        .expect("0 is valid");
    assert_eq!(settings.idle_floor_pct, Some(0.0));
    settings
        .apply(&patch(&[("idle_floor", "99.9")]))
        .expect("99.9 is valid");
    assert_eq!(settings.idle_floor_pct, Some(99.9));
    settings
        .apply(&patch(&[("idle_floor", "off")]))
        .expect("off is valid");
    assert_eq!(settings.idle_floor_pct, None);
    for text in ["100", "-1", "lots"] {
        let err = settings
            .apply(&patch(&[("idle_floor", text)]))
            .expect_err("not a percent");
        assert!(err.starts_with("--idle-floor: "), "{text}: {err}");
    }
}

#[test]
fn agora_reconcile_interval_is_finite_and_at_least_one_second() {
    let mut settings = WorkSettings::default();
    assert_eq!(settings.reconcile_interval_s, 30.0, "default 30 s");
    settings
        .apply(&patch(&[("reconcile_interval", "1")]))
        .expect("one second is valid");
    assert_eq!(settings.reconcile_interval_s, 1.0);
    for text in ["0.999", "inf", "nan", "-5"] {
        let err = settings
            .apply(&patch(&[("reconcile_interval", text)]))
            .expect_err("not an interval");
        assert!(err.starts_with("--reconcile-interval: "), "{text}: {err}");
    }
    assert_eq!(
        settings.reconcile_interval_s, 1.0,
        "rejected values are not stored"
    );
}

#[test]
fn agora_lease_ttl_is_a_finite_positive_number_of_seconds() {
    let mut settings = WorkSettings::default();
    assert_eq!(settings.lease_ttl_s, 900.0, "default 900 s");
    settings
        .apply(&patch(&[("lease_ttl", "0.5")]))
        .expect("0.5 is valid");
    assert_eq!(settings.lease_ttl_s, 0.5);
    for text in ["0", "-1", "inf", "nan"] {
        let err = settings
            .apply(&patch(&[("lease_ttl", text)]))
            .expect_err("not a positive TTL");
        assert!(err.starts_with("--lease-ttl: "), "{text}: {err}");
    }
}

#[test]
fn agora_invalid_keys_leave_settings_unchanged_together_with_valid_ones() {
    let cases = [
        ("self_accept", "yes", "--self-accept"),
        ("timer_quota", "0", "--timer-quota"),
        ("disk_floor", "0.4K", "--disk-floor"),
        ("memory_floor", "512", "--memory-floor"),
        ("idle_floor", "100", "--idle-floor"),
        ("reconcile_interval", "0.5", "--reconcile-interval"),
        ("lease_ttl", "0", "--lease-ttl"),
    ];
    for (key, value, flag) in cases {
        assert_rejected_and_unchanged(key, value, flag);
    }
}

// The change list returned by apply.

#[test]
fn a_full_patch_changes_every_key_and_lists_the_raw_values_in_field_order() {
    let mut settings = WorkSettings::default();
    let full = patch(&[
        ("lease_ttl", "60"),
        ("max_waiting", "128"),
        ("run_role", "supervisor"),
        ("reconcile_interval", "45"),
        ("idle_floor", "7.5"),
        ("memory_floor", "2G"),
        ("disk_floor", "off"),
        ("timer_quota", "5"),
        ("self_accept", "true"),
        ("quiet_max", "90"),
        ("quiet_drain", "30"),
        ("job_qos", "background"),
        ("aging", "0"),
        ("small_weight", "0.5"),
        ("backfill_wait", "120"),
        ("keep_days", "30"),
        ("log_limit", "1024"),
        ("load_wait", "600"),
        ("load_factor", "2"),
        ("memory_limit", "critical"),
        ("idle_min", "7.5"),
        ("memory_budget", "14G"),
        ("capacity", "3"),
        ("cpu_budget", "4"),
    ]);
    let changes = settings.apply(&full).expect("every value is valid");
    assert_eq!(
        changes,
        [
            "cpu_budget 4",
            "capacity 3",
            "memory_budget 14G",
            "idle_min 7.5",
            "memory_limit critical",
            "load_factor 2",
            "load_wait 600",
            "log_limit 1024",
            "keep_days 30",
            "backfill_wait 120",
            "small_weight 0.5",
            "aging 0",
            "job_qos background",
            "quiet_drain 30",
            "quiet_max 90",
            "self_accept true",
            "timer_quota 5",
            "disk_floor off",
            "memory_floor 2G",
            "idle_floor 7.5",
            "reconcile_interval 45",
            "lease_ttl 60",
            "run_role supervisor",
            "max_waiting 128",
        ]
    );
    let expected = WorkSettings {
        cpu_budget: Some(4.0),
        capacity: Some(3.0),
        memory_budget: MemBudget::Kib(14_680_064),
        idle_min: 7.5,
        memory_limit: MemLimit::Critical,
        load_factor: 2.0,
        load_wait: 600.0,
        log_limit: 1024,
        keep_days: 30.0,
        backfill_wait: 120.0,
        small_weight: 0.5,
        aging: 0.0,
        job_qos: Qos::Background,
        quiet_drain: 30.0,
        quiet_max: 90.0,
        self_accept: true,
        timer_quota: 5,
        disk_floor_kib: None,
        memory_floor_kib: Some(2_097_152),
        idle_floor_pct: Some(7.5),
        reconcile_interval_s: 45.0,
        lease_ttl_s: 60.0,
        run_role: "supervisor".to_owned(),
        max_waiting: 128,
    };
    assert_eq!(settings, expected);
}

// Defaults, serde, and the document.

#[test]
fn defaults_are_work_py_defaults_plus_the_agora_keys() {
    let expected = WorkSettings {
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
        run_role: "worker".to_owned(),
        max_waiting: 64,
    };
    assert_eq!(WorkSettings::default(), expected);
}

#[test]
fn mem_budget_serializes_as_auto_off_or_an_integer_and_round_trips() {
    let cases = [
        (MemBudget::Auto, "\"auto\""),
        (MemBudget::Off, "\"off\""),
        (MemBudget::Kib(1234), "1234"),
    ];
    for (budget, text) in cases {
        assert_eq!(serde_json::to_string(&budget).expect("serializes"), text);
        let back: MemBudget = serde_json::from_str(text).expect("deserializes");
        assert_eq!(back, budget, "{text}");
    }
}

#[test]
fn mem_budget_rejects_other_words_negatives_fractions_and_null() {
    for text in ["\"on\"", "\"AUTO\"", "\"14G\"", "-1", "1.5", "null"] {
        assert!(
            serde_json::from_str::<MemBudget>(text).is_err(),
            "{text} must not be a memory budget"
        );
    }
}

#[test]
fn work_settings_json_round_trips_defaults_and_changed_values() {
    let changed = WorkSettings {
        cpu_budget: Some(4.0),
        capacity: Some(2.5),
        memory_budget: MemBudget::Kib(1234),
        memory_limit: MemLimit::Off,
        job_qos: Qos::Background,
        self_accept: true,
        timer_quota: 5,
        disk_floor_kib: None,
        memory_floor_kib: Some(2_097_152),
        idle_floor_pct: Some(7.5),
        reconcile_interval_s: 45.0,
        lease_ttl_s: 60.0,
        ..WorkSettings::default()
    };
    for settings in [WorkSettings::default(), changed] {
        let text = serde_json::to_string(&settings).expect("serializes");
        let back: WorkSettings = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, settings, "{text}");
    }
}

#[test]
fn work_settings_json_rejects_an_unknown_key() {
    let err = serde_json::from_str::<WorkSettings>(r#"{"aging": 2.0, "bogus": 1}"#)
        .expect_err("an unknown key is an error, not ignored");
    assert!(err.to_string().contains("unknown field `bogus`"), "{err}");
}

#[test]
fn work_settings_json_fills_absent_keys_from_the_defaults() {
    let settings: WorkSettings =
        serde_json::from_str(r#"{"aging": 2.0}"#).expect("a partial document is valid");
    assert_eq!(
        settings,
        WorkSettings {
            aging: 2.0,
            ..WorkSettings::default()
        }
    );
}

#[test]
fn settings_json_lists_the_work_py_keys_and_the_agora_keys_with_default_values() {
    let document = settings_json(&WorkSettings::default(), &machine_16_gib());
    assert_eq!(
        document,
        json!({
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
            "run_role": "worker",
            "max_waiting": 64,
        })
    );
}

// settings_text.

const WORK_PY_PREFIXES: [&str; 15] = [
    "cpu budget: ",
    "capacity: ",
    "memory budget: ",
    "idle min: ",
    "memory limit: ",
    "load factor: ",
    "load wait: ",
    "aging: ",
    "backfill wait: ",
    "quiet drain: ",
    "quiet max: ",
    "job qos: ",
    "small weight: ",
    "log limit: ",
    "keep: ",
];

#[test]
fn settings_text_starts_with_the_fifteen_work_py_lines_in_work_py_order() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 24, "15 work.py lines and 9 agora lines");
    for (line, prefix) in lines.iter().zip(WORK_PY_PREFIXES) {
        assert!(
            line.starts_with(prefix),
            "{line:?} should start with {prefix:?}"
        );
    }
}

#[test]
fn settings_text_renders_the_default_values_in_work_py_units() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..15],
        [
            "cpu budget: 8 cores (the default: the effective CPU capacity); the running jobs' predicted cores fit it",
            "capacity: off (the default): no cap on declared weights",
            "memory budget: 14.0 GiB (the default: the memory minus 2 GiB); jobs start while used memory (active, wired and compressed) is at or below it",
            "idle min: 10%; jobs start while the CPU is at least this idle",
            "memory limit: warning; jobs start below warning memory pressure",
            "load factor: 1.5; on Linux only, jobs start below load 12.00 on 8 effective CPUs",
            "load wait: 30 min 0 s; a job that the machine's pressure alone holds back longer fails",
            "aging: 1; waiting jobs go shortest predicted run time first, less this many seconds for each second waited",
            "backfill wait: 5 min 0 s; a job that waited longer keeps every job behind it",
            "quiet drain: 1 min 0 s; the longest an exclusive job holds new jobs predicted above the small weight before it starts",
            "quiet max: 3 min 0 s; the longest an exclusive job keeps them out once it runs",
            "job qos: utility; every job but the exclusive ones runs at this QoS, and its children with it, so the user's own work comes first",
            "small weight: 1; a job predicted to use this many cores or fewer may start past the head for a while",
            "log limit: 268435456 bytes; a job whose log grows past it is stopped",
            "keep: 14 days of events and of the logs of ended tasks",
        ]
    );
}

#[test]
fn settings_text_appends_one_line_per_agora_key_after_the_work_py_lines() {
    let text = settings_text(&WorkSettings::default(), &machine_16_gib(), true);
    let lines: Vec<&str> = text.lines().collect();
    let agora = &lines[15..];
    let prefixes = [
        "self accept: ",
        "timer quota: ",
        "run role: ",
        "max waiting: ",
        "disk floor: ",
        "memory floor: ",
        "idle floor: ",
        "reconcile interval: ",
        "lease ttl: ",
    ];
    assert_eq!(agora.len(), prefixes.len());
    for (line, prefix) in agora.iter().zip(prefixes) {
        assert!(
            line.starts_with(prefix),
            "{line:?} should start with {prefix:?}"
        );
    }
    assert!(
        agora[1].contains("32"),
        "timer quota line shows the quota: {}",
        agora[1]
    );
    assert_eq!(
        agora[2],
        "run role: worker; identities below it cannot run or accept commands"
    );
    assert_eq!(agora[3], "max waiting: 64 jobs per identity");
    assert!(
        agora[4].contains("5.0 GiB"),
        "disk floor line shows 5 GiB: {}",
        agora[4]
    );
    assert!(
        agora[7].contains("30.0 s"),
        "reconcile line shows 30 s: {}",
        agora[7]
    );
    assert!(
        agora[8].contains("15 min 0 s"),
        "lease line shows 900 s: {}",
        agora[8]
    );
}

#[test]
fn settings_text_says_job_qos_is_not_applied_off_macos_only() {
    let settings = WorkSettings::default();
    let on_mac = settings_text(&settings, &machine_16_gib(), true);
    let elsewhere = settings_text(&settings, &machine_16_gib(), false);
    let qos_line = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("job qos: "))
            .expect("the job qos line is present")
            .to_owned()
    };
    assert!(!qos_line(&on_mac).contains("not applied here"));
    assert!(qos_line(&elsewhere).ends_with("(not applied here: macOS only)"));
}

// C5: exclusive_at.

#[test]
fn c5_exclusive_needs_two_cpus_and_a_weight_covering_every_cpu() {
    let cases = [
        (1.0, 1.0, false),
        (1.0, 0.5, false),
        (2.0, 2.0, true),
        (3.5, 4.0, false),
        (8.0, 8.0, true),
        (1.5, 1.5, false),
        (1.9999, 2.0, false),
    ];
    for (weight, cpus, expected) in cases {
        assert_eq!(
            exclusive_at(weight, cpus),
            expected,
            "weight {weight} on {cpus} CPUs"
        );
    }
}
