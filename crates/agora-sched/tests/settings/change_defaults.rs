use super::*;

// The change list returned by apply.

#[test]
fn a_full_patch_changes_every_key_and_lists_the_raw_values_in_field_order() {
    let mut settings = WorkSettings::default();
    let full = patch(&[
        ("lease_ttl", "60"),
        ("claim_ttl", "600"),
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
            "claim_ttl 600",
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
        claim_ttl_s: 600.0,
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
        claim_ttl_s: 1800.0,
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
            "claim_ttl_s": 1800.0,
            "run_role": "worker",
            "max_waiting": 64,
        })
    );
}
