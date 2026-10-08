use super::*;

fn is_usage<T>(r: &Result<T, CliError>) -> bool {
    matches!(r, Err(CliError::Usage(_)))
}

#[test]
fn team_policy_passes_set_flags_through_and_leaves_unset_ones_inheriting() {
    let p = TeamPolicy {
        stale_after_s: Some(120.0),
        reclaim: Some(false),
        wip_limit: Some(3),
        escalation_chain: Some(vec!["ab12".into()]),
        ..TeamPolicy::default()
    };
    assert_eq!(team_policy(p.clone()).expect("valid flags"), p);
    assert_eq!(
        team_policy(TeamPolicy::default()).expect("no flags"),
        TeamPolicy::default()
    );
}

#[test]
fn team_policy_rejects_negative_nan_and_infinite_thresholds() {
    for bad in [-1.0, f64::NAN, f64::INFINITY] {
        let fields = [
            TeamPolicy {
                stale_after_s: Some(bad),
                ..TeamPolicy::default()
            },
            TeamPolicy {
                escalate_after_s: Some(bad),
                ..TeamPolicy::default()
            },
            TeamPolicy {
                reclaim_after_s: Some(bad),
                ..TeamPolicy::default()
            },
        ];
        for p in fields {
            assert!(is_usage(&team_policy(p)), "{bad} should be refused");
        }
    }
}

#[test]
fn team_policy_rejects_an_empty_escalation_key() {
    let p = TeamPolicy {
        escalation_chain: Some(vec!["ab12".into(), String::new()]),
        ..TeamPolicy::default()
    };
    assert!(is_usage(&team_policy(p)));
}

#[test]
fn quiet_window_ends_for_secs_after_now() {
    let a = quiet_args(1_000, Some(90), None, "lunch".into()).expect("valid window");
    assert_eq!(a.expected_end_ms, 91_000);
    assert_eq!(a.note, "lunch");
}

#[test]
fn quiet_window_is_between_one_second_and_one_day() {
    assert!(is_usage(&quiet_args(0, Some(0), None, String::new())));
    assert!(is_usage(&quiet_args(
        0,
        Some(QUIET_MAX_SECS + 1),
        None,
        String::new()
    )));
    let longest = quiet_args(5, Some(QUIET_MAX_SECS), None, String::new()).expect("one day");
    assert_eq!(longest.expected_end_ms, 5 + QUIET_MAX_MS);
    assert!(quiet_args(0, Some(1), None, String::new()).is_ok());
}

#[test]
fn quiet_window_refuses_an_end_past_the_largest_time() {
    assert!(is_usage(&quiet_args(
        i64::MAX - 5_000,
        Some(10),
        None,
        String::new()
    )));
}

#[test]
fn quiet_until_ends_at_the_given_epoch_milliseconds() {
    let a = quiet_args(1_000, None, Some("91000"), "lunch".into()).expect("epoch ms");
    assert_eq!(a.expected_end_ms, 91_000);
    assert_eq!(a.note, "lunch");
}

#[test]
fn quiet_until_takes_rfc3339_utc() {
    let a = quiet_args(1_000, None, Some("1970-01-01T00:01:31Z"), String::new()).expect("rfc 3339");
    assert_eq!(a.expected_end_ms, 91_000);
}

#[test]
fn quiet_until_must_be_one_second_to_one_day_away() {
    let too_long = (1_000 + QUIET_MAX_MS + 1).to_string();
    let longest = (1_000 + QUIET_MAX_MS).to_string();
    assert!(is_usage(&quiet_args(
        1_000,
        None,
        Some("1000"),
        String::new()
    )));
    assert!(is_usage(&quiet_args(
        1_000,
        None,
        Some("500"),
        String::new()
    )));
    assert!(is_usage(&quiet_args(
        1_000,
        None,
        Some(too_long.as_str()),
        String::new()
    )));
    assert!(quiet_args(1_000, None, Some("2000"), String::new()).is_ok());
    assert!(quiet_args(1_000, None, Some(longest.as_str()), String::new()).is_ok());
}

#[test]
fn quiet_until_refuses_text_that_is_not_a_time() {
    assert!(is_usage(&quiet_args(0, None, Some("soon"), String::new())));
}

#[test]
fn quiet_open_needs_exactly_one_end() {
    assert!(is_usage(&quiet_args(0, None, None, String::new())));
    assert!(is_usage(&quiet_args(
        0,
        Some(60),
        Some("120000"),
        String::new()
    )));
}

#[test]
fn quiet_open_flags_parse_for_for_secs_or_until_but_not_both() {
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct QuietWrap {
        #[command(subcommand)]
        cmd: crate::QuietCmd,
    }
    let parse = |argv: &[&str]| {
        QuietWrap::try_parse_from(std::iter::once("x").chain(argv.iter().copied())).map(|w| w.cmd)
    };
    for flag in ["--for", "--for-secs"] {
        match parse(&["open", flag, "90"]).expect("a duration flag") {
            crate::QuietCmd::Open {
                for_secs, until, ..
            } => {
                assert_eq!((for_secs, until), (Some(90), None), "{flag}");
            }
            other => panic!("expected open, got {other:?}"),
        }
    }
    match parse(&["open", "--until", "2026-10-08T10:00:00Z"]).expect("until") {
        crate::QuietCmd::Open {
            for_secs, until, ..
        } => {
            assert_eq!(for_secs, None);
            assert_eq!(until.as_deref(), Some("2026-10-08T10:00:00Z"));
        }
        other => panic!("expected open, got {other:?}"),
    }
    assert!(parse(&["open"]).is_err(), "one end is required");
    assert!(parse(&["open", "--for", "90", "--until", "120000"]).is_err());
}

#[test]
fn pool_lease_takes_amount_units_and_keeps_the_ttl_and_card() {
    let a = lease_args(
        Some("gpu".into()),
        None,
        2.0,
        Some(30.0),
        Some("c1".into()),
        None,
        "n".into(),
    )
    .expect("pool lease");
    assert_eq!(a.kind, LeaseKind::Pool);
    assert_eq!(a.resource, "gpu");
    assert_eq!(a.amount, 2.0);
    assert_eq!(a.ttl_s, Some(30.0));
    assert_eq!(a.card.as_deref(), Some("c1"));
}

#[test]
fn path_lease_has_no_amount_and_keeps_the_task() {
    let a = lease_args(
        None,
        Some("/tmp/x".into()),
        5.0,
        None,
        None,
        Some("t1".into()),
        String::new(),
    )
    .expect("path lease");
    assert_eq!(a.kind, LeaseKind::Path);
    assert_eq!(a.resource, "/tmp/x");
    assert_eq!(a.amount, 0.0);
    assert_eq!(a.task.as_deref(), Some("t1"));
}

#[test]
fn lease_needs_exactly_one_of_pool_or_path() {
    let neither = lease_args(None, None, 1.0, None, None, None, String::new());
    assert!(is_usage(&neither));
    let both = lease_args(
        Some("gpu".into()),
        Some("/tmp/x".into()),
        1.0,
        None,
        None,
        None,
        String::new(),
    );
    assert!(is_usage(&both));
}

#[test]
fn lease_rejects_a_bad_amount_or_ttl() {
    for amount in [0.0, -1.0, f64::NAN] {
        let r = lease_args(
            Some("gpu".into()),
            None,
            amount,
            None,
            None,
            None,
            String::new(),
        );
        assert!(is_usage(&r), "amount {amount} should be refused");
    }
    assert!(is_usage(&ttl(Some(0.0))));
    assert!(is_usage(&ttl(Some(-5.0))));
    assert_eq!(ttl(None).expect("absent"), None);
    assert_eq!(ttl(Some(60.0)).expect("valid"), Some(60.0));
}

#[test]
fn pool_define_needs_a_name_and_a_positive_capacity() {
    assert!(is_usage(&pool_args(
        String::new(),
        10.0,
        String::new(),
        String::new()
    )));
    assert!(is_usage(&pool_args(
        "gpu".into(),
        0.0,
        String::new(),
        String::new()
    )));
    let ok = pool_args("gpu".into(), 4.0, "cores".into(), String::new()).expect("valid");
    assert_eq!(ok.capacity, 4.0);
    assert_eq!(ok.unit, "cores");
}

#[test]
fn lease_kind_accepts_the_three_kinds_only() {
    assert_eq!(lease_kind(None).expect("absent"), None);
    assert_eq!(
        lease_kind(Some("quiet")).expect("quiet"),
        Some(LeaseKind::Quiet)
    );
    assert!(is_usage(&lease_kind(Some("bogus"))));
}

#[test]
fn card_update_needs_a_field_and_finite_numbers() {
    let none = card_update_args(None, None, None, None, None, None);
    assert!(is_usage(&none));
    let one = card_update_args(Some(3), None, None, None, None, None).expect("priority only");
    assert_eq!(one.priority, Some(3));
    assert!(is_usage(&card_update_args(
        None,
        Some(f64::NAN),
        None,
        None,
        None,
        None
    )));
}

#[test]
fn card_dep_refuses_a_self_dependency() {
    assert!(is_usage(&card_dep_args("c1", "c1".into(), false)));
    assert!(is_usage(&card_dep_args("c1", String::new(), false)));
    let ok = card_dep_args("c1", "c2".into(), true).expect("valid");
    assert_eq!(ok.after, "c2");
    assert!(ok.remove);
}

#[test]
fn card_link_parses_the_relation_name() {
    let ok = card_link_args("blocks", "c9".into(), false).expect("blocks");
    assert_eq!(ok.rel, LinkRel::Blocks);
    let caused = card_link_args("caused_by", "c9".into(), false).expect("caused_by");
    assert_eq!(caused.rel, LinkRel::CausedBy);
    assert!(is_usage(&card_link_args("bogus", "c9".into(), false)));
}

#[test]
fn escalate_needs_an_owner_action_and_non_empty_keys() {
    assert!(is_usage(&escalate_args("   ".into(), vec![])));
    assert!(is_usage(&escalate_args(
        "review it".into(),
        vec![String::new()]
    )));
    let ok = escalate_args("review it".into(), vec!["ab12".into()]).expect("valid");
    assert_eq!(ok.to, vec!["ab12".to_string()]);
}
