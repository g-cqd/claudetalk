use super::*;

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
