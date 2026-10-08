use super::{MemBudget, Qos, SettingsPatch, WorkSettings, parse_size_kib};

#[test]
fn st1_memory_sizes_convert_to_kib() {
    assert_eq!(parse_size_kib("14G"), Ok(14_680_064));
    assert_eq!(parse_size_kib("14GiB"), Ok(14_680_064));
    assert_eq!(parse_size_kib("14336M"), Ok(14_680_064));
    assert_eq!(parse_size_kib("1.5G"), Ok(1_572_864));
    assert_eq!(parse_size_kib("512k"), Ok(512));
    assert_eq!(parse_size_kib(" 2 mib "), Ok(2048));
}

#[test]
fn st1_sizes_need_a_unit_and_a_positive_value() {
    assert_eq!(
        parse_size_kib("14"),
        Err("'14' is not a size with its unit, as 14G or 14336M".to_owned())
    );
    assert_eq!(
        parse_size_kib("0K"),
        Err("'0K' is not greater than zero".to_owned())
    );
    for bad in ["", "-1G", "1.G", ".5G", "14 GB extra", "14Gi", "1e3K"] {
        assert!(parse_size_kib(bad).is_err(), "{bad:?} must be rejected");
    }
}
#[test]
fn st3_invalid_values_are_rejected_with_work_py_wording() {
    let cases: [(SettingsPatch, &str); 15] = [
        (
            SettingsPatch {
                capacity: Some("0".into()),
                ..Default::default()
            },
            "--capacity: '0' is not greater than zero",
        ),
        (
            SettingsPatch {
                capacity: Some("lots".into()),
                ..Default::default()
            },
            "--capacity: 'lots' is not a number",
        ),
        (
            SettingsPatch {
                cpu_budget: Some("0".into()),
                ..Default::default()
            },
            "--cpu-budget: '0' is not greater than zero",
        ),
        (
            SettingsPatch {
                cpu_budget: Some("off".into()),
                ..Default::default()
            },
            "--cpu-budget: 'off' is not a number",
        ),
        (
            SettingsPatch {
                job_qos: Some("maintenance".into()),
                ..Default::default()
            },
            "--job-qos: 'maintenance' is not one of utility, default, background",
        ),
        (
            SettingsPatch {
                aging: Some("-1".into()),
                ..Default::default()
            },
            "--aging: '-1' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                quiet_drain: Some("-1".into()),
                ..Default::default()
            },
            "--quiet-drain: '-1' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                quiet_max: Some("nan".into()),
                ..Default::default()
            },
            "--quiet-max: 'nan' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                load_factor: Some("-1".into()),
                ..Default::default()
            },
            "--load-factor: '-1' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                load_wait: Some("nan".into()),
                ..Default::default()
            },
            "--load-wait: 'nan' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                log_limit: Some("0".into()),
                ..Default::default()
            },
            "--log-limit: '0' is less than 1",
        ),
        (
            SettingsPatch {
                keep_days: Some("inf".into()),
                ..Default::default()
            },
            "--keep-days: 'inf' is not a finite number, zero or more",
        ),
        (
            SettingsPatch {
                idle_min: Some("100".into()),
                ..Default::default()
            },
            "--idle-min: '100' is not below 100",
        ),
        (
            SettingsPatch {
                memory_limit: Some("high".into()),
                ..Default::default()
            },
            "--memory-limit: 'high' is not one of warning, critical, off",
        ),
        (
            SettingsPatch {
                memory_budget: Some("14".into()),
                ..Default::default()
            },
            "--memory-budget: '14' is not a size with its unit, as 14G or 14336M, nor auto or off",
        ),
    ];
    for (patch, message) in cases {
        let mut settings = WorkSettings::default();
        assert_eq!(settings.apply(&patch), Err(message.to_owned()));
    }
}

#[test]
fn st4_off_and_auto_store_no_value_for_the_optional_limits() {
    let mut settings = WorkSettings {
        capacity: Some(3.0),
        cpu_budget: Some(4.0),
        ..WorkSettings::default()
    };
    let changes = settings.apply(&SettingsPatch {
        capacity: Some("off".into()),
        cpu_budget: Some("auto".into()),
        ..Default::default()
    });
    assert!(changes.is_ok());
    assert_eq!(settings.capacity, None);
    assert_eq!(settings.cpu_budget, None);
    assert!(
        settings
            .apply(&SettingsPatch {
                capacity: Some("auto".into()),
                ..Default::default()
            })
            .is_ok()
    );
    assert_eq!(settings.capacity, None);
    settings
        .apply(&SettingsPatch {
            capacity: Some("2.5".into()),
            ..Default::default()
        })
        .expect("valid");
    assert_eq!(settings.capacity, Some(2.5));
}

#[test]
fn apply_is_all_or_nothing() {
    let before = WorkSettings::default();
    let mut settings = before.clone();
    let patch = SettingsPatch {
        cpu_budget: Some("4".into()),
        aging: Some("-1".into()),
        ..Default::default()
    };
    assert!(settings.apply(&patch).is_err());
    assert_eq!(settings, before);
}

#[test]
fn apply_lists_raw_values_in_field_order() {
    let mut settings = WorkSettings::default();
    let patch = SettingsPatch {
        self_accept: Some("true".into()),
        job_qos: Some("background".into()),
        memory_budget: Some("14G".into()),
        cpu_budget: Some("4".into()),
        ..Default::default()
    };
    let changes = settings.apply(&patch).expect("valid patch");
    assert_eq!(
        changes,
        [
            "cpu_budget 4",
            "memory_budget 14G",
            "job_qos background",
            "self_accept true"
        ]
    );
    assert_eq!(settings.cpu_budget, Some(4.0));
    assert_eq!(settings.memory_budget, MemBudget::Kib(14_680_064));
    assert_eq!(settings.job_qos, Qos::Background);
    assert!(settings.self_accept);
}

#[test]
fn an_empty_patch_changes_nothing() {
    let mut settings = WorkSettings::default();
    assert!(SettingsPatch::default().is_empty());
    assert_eq!(settings.apply(&SettingsPatch::default()), Ok(Vec::new()));
    assert_eq!(settings, WorkSettings::default());
}

#[test]
fn agora_keys_follow_their_own_rules() {
    let mut settings = WorkSettings::default();
    let bad = [
        (
            SettingsPatch {
                self_accept: Some("yes".into()),
                ..Default::default()
            },
            "--self-accept: 'yes' is not true or false",
        ),
        (
            SettingsPatch {
                timer_quota: Some("0".into()),
                ..Default::default()
            },
            "--timer-quota: '0' is less than 1",
        ),
        (
            SettingsPatch {
                idle_floor: Some("100".into()),
                ..Default::default()
            },
            "--idle-floor: '100' is not below 100",
        ),
        (
            SettingsPatch {
                reconcile_interval: Some("0.5".into()),
                ..Default::default()
            },
            "--reconcile-interval: '0.5' is not a finite number, one second or more",
        ),
        (
            SettingsPatch {
                lease_ttl: Some("0".into()),
                ..Default::default()
            },
            "--lease-ttl: '0' is not greater than zero",
        ),
        (
            SettingsPatch {
                disk_floor: Some("0K".into()),
                ..Default::default()
            },
            "--disk-floor: '0K' is not greater than zero",
        ),
    ];
    for (patch, message) in bad {
        assert_eq!(settings.apply(&patch), Err(message.to_owned()));
    }
    let good = SettingsPatch {
        disk_floor: Some("off".into()),
        memory_floor: Some("2G".into()),
        idle_floor: Some("7.5".into()),
        timer_quota: Some("5".into()),
        lease_ttl: Some("60".into()),
        ..Default::default()
    };
    settings.apply(&good).expect("valid agora keys");
    assert_eq!(settings.disk_floor_kib, None);
    assert_eq!(settings.memory_floor_kib, Some(2_097_152));
    assert_eq!(settings.idle_floor_pct, Some(7.5));
    assert_eq!(settings.timer_quota, 5);
    assert_eq!(settings.lease_ttl_s, 60.0);
}

#[test]
fn validate_accepts_defaults_and_rejects_impossible_stored_values() {
    assert_eq!(WorkSettings::default().validate(), Ok(()));
    let idle = WorkSettings {
        idle_min: 100.0,
        ..WorkSettings::default()
    };
    assert!(idle.validate().is_err());
    let quota = WorkSettings {
        timer_quota: 0,
        ..WorkSettings::default()
    };
    assert!(quota.validate().is_err());
    let zero_kib = WorkSettings {
        memory_budget: MemBudget::Kib(0),
        ..WorkSettings::default()
    };
    assert!(zero_kib.validate().is_err());
}
