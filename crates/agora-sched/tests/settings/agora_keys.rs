use super::*;

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
fn agora_claim_ttl_default_and_60_seconds_are_accepted() {
    let mut settings = WorkSettings::default();
    assert_eq!(settings.claim_ttl_s, 1800.0, "default 1800 s");
    settings.validate().expect("the default claim TTL is valid");
    settings
        .apply(&patch(&[("claim_ttl", "60")]))
        .expect("60 is valid");
    assert_eq!(settings.claim_ttl_s, 60.0);
    settings
        .validate()
        .expect("a stored 60 s claim TTL is valid");
}

#[test]
fn agora_claim_ttl_below_60_seconds_is_rejected_and_not_stored() {
    let mut settings = WorkSettings::default();
    for text in ["0.001", "59.9", "0", "-1"] {
        let err = settings
            .apply(&patch(&[("claim_ttl", text)]))
            .expect_err("a claim TTL below 60 s is not valid");
        assert!(err.starts_with("--claim-ttl: "), "{text}: {err}");
        assert_eq!(settings.claim_ttl_s, 1800.0, "{text} must not be stored");
    }
    for value in [0.001, 59.9, 0.0] {
        let stored = WorkSettings {
            claim_ttl_s: value,
            ..WorkSettings::default()
        };
        assert_eq!(
            stored.validate(),
            Err("claim_ttl must be a finite number, 60 seconds or more".to_owned()),
            "{value} stored"
        );
    }
}

#[test]
fn agora_claim_ttl_nan_and_infinity_are_rejected() {
    let mut settings = WorkSettings::default();
    for text in ["inf", "nan"] {
        let err = settings
            .apply(&patch(&[("claim_ttl", text)]))
            .expect_err("a claim TTL must be finite");
        assert!(err.starts_with("--claim-ttl: "), "{text}: {err}");
        assert_eq!(settings.claim_ttl_s, 1800.0, "{text} must not be stored");
    }
    for value in [f64::NAN, f64::INFINITY] {
        let stored = WorkSettings {
            claim_ttl_s: value,
            ..WorkSettings::default()
        };
        assert_eq!(
            stored.validate(),
            Err("claim_ttl must be a finite number, 60 seconds or more".to_owned()),
            "{value} stored"
        );
    }
}

#[test]
fn agora_claim_ttl_defaults_to_1800_for_rows_written_before_it_existed() {
    let old: WorkSettings = serde_json::from_str(r#"{"lease_ttl_s": 900.0, "run_role": "worker"}"#)
        .expect("a row without claim_ttl_s is valid");
    assert_eq!(old.claim_ttl_s, 1800.0);
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
        ("claim_ttl", "0", "--claim-ttl"),
    ];
    for (key, value, flag) in cases {
        assert_rejected_and_unchanged(key, value, flag);
    }
}
