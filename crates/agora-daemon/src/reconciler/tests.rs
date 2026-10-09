use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agora-reconciler-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn notice() -> ReconcileAction {
    ReconcileAction::Notice {
        key: "k".into(),
        target: "pk1".into(),
        kind: "stale".into(),
        subject: "c1".into(),
        text: "t".into(),
    }
}

#[test]
fn events_map_leases_to_lease_changed() {
    let expire = ReconcileAction::ExpireLease {
        key: "expire:l1".into(),
        lease: "l1".into(),
    };
    let quiet = ReconcileAction::CloseQuiet {
        key: "quiet:q1".into(),
        lease: "q1".into(),
    };
    assert_eq!(
        events_for(&expire, None, false),
        vec![Event::LeaseChanged { lease: "l1".into() }]
    );
    assert_eq!(
        events_for(&quiet, None, false),
        vec![Event::LeaseChanged { lease: "q1".into() }]
    );
}

#[test]
fn events_map_notice_to_notice_posted_with_seq_zero() {
    assert_eq!(
        events_for(&notice(), None, false),
        vec![Event::NoticePosted {
            target: "pk1".into(),
            seq: 0
        }]
    );
}

#[test]
fn events_map_reclaim_to_card_changed() {
    let reclaim = ReconcileAction::ReclaimCard {
        key: "reclaim:c1:5".into(),
        card: "c1".into(),
        owner: "pk1".into(),
    };
    assert_eq!(
        events_for(&reclaim, None, false),
        vec![Event::CardChanged { card: "c1".into() }]
    );
}

#[test]
fn events_map_blocker_to_its_card_or_nothing() {
    let open = ReconcileAction::Blocker {
        key: "blocker:1".into(),
        open: true,
        title: "Machine floor breached".into(),
        owner_action: "free disk".into(),
    };
    assert_eq!(
        events_for(&open, Some("blocker-3"), false),
        vec![Event::CardChanged {
            card: "blocker-3".into()
        }]
    );
    // A blocker whose id the store did not report has no card to announce.
    assert!(events_for(&open, None, false).is_empty());
}

#[test]
fn events_map_admission_to_work_settings_changed() {
    let pause = ReconcileAction::Admission {
        key: "pause:1".into(),
        paused: Some("disk".into()),
    };
    assert_eq!(
        events_for(&pause, None, false),
        vec![Event::WorkSettingsChanged]
    );
}

fn claim_expiry() -> ReconcileAction {
    ReconcileAction::ExpireClaim {
        key: "claim:t1:5".into(),
        task: "t1".into(),
        agent: "pk1".into(),
        claimed_ms: 5,
        notice_key: "claim-notice:t1:5".into(),
        text: "claim on task t1 expired".into(),
    }
}

#[test]
fn an_expired_claim_publishes_work_changed_and_kicks_admission() {
    let expire = claim_expiry();
    assert_eq!(
        events_for(&expire, None, false),
        vec![Event::WorkChanged { task: "t1".into() }]
    );
    assert!(kicks_admission(&expire));
}

#[test]
fn an_expired_claim_whose_notice_was_posted_publishes_notice_posted_to_the_agent() {
    assert_eq!(
        events_for(&claim_expiry(), None, true),
        vec![
            Event::WorkChanged { task: "t1".into() },
            Event::NoticePosted {
                target: "pk1".into(),
                seq: 0
            },
        ]
    );
}

#[test]
fn only_admission_lease_and_quiet_actions_kick_admission() {
    assert!(kicks_admission(&ReconcileAction::Admission {
        key: "a".into(),
        paused: None
    }));
    assert!(kicks_admission(&ReconcileAction::ExpireLease {
        key: "e".into(),
        lease: "l".into()
    }));
    assert!(kicks_admission(&ReconcileAction::CloseQuiet {
        key: "q".into(),
        lease: "l".into()
    }));
    assert!(!kicks_admission(&notice()));
}

#[test]
fn interval_comes_from_the_setting_and_falls_back_to_thirty_seconds() {
    assert_eq!(interval_ms(30.0), 30_000);
    assert_eq!(interval_ms(90.5), 90_500);
    for bad in [0.5, 0.0, -4.0, f64::NAN, f64::INFINITY] {
        assert_eq!(interval_ms(bad), 30_000, "interval {bad}");
    }
}

#[test]
fn prune_cutoff_is_keep_days_before_now() {
    let now = 1_700_000_000_000;
    assert_eq!(prune_cutoff(now, 14.0), now - 14 * 86_400_000);
    // Unusable values use the default of 14 days, never a cutoff that prunes everything.
    assert_eq!(prune_cutoff(now, 0.0), now - 14 * 86_400_000);
    assert_eq!(prune_cutoff(now, f64::NAN), now - 14 * 86_400_000);
    // An absurdly large window prunes nothing (the cutoff is far in the past).
    assert!(prune_cutoff(now, 1e300) < 0);
}

#[test]
fn retention_runs_once_an_hour() {
    let now = 10_000_000;
    assert!(prune_due(None, now));
    assert!(!prune_due(Some(now - 1_000), now));
    assert!(!prune_due(Some(now - (PRUNE_EVERY_MS - 1)), now));
    assert!(prune_due(Some(now - PRUNE_EVERY_MS), now));
    // A clock stepped back before the last run counts as due.
    assert!(prune_due(Some(now + 5_000), now));
}

#[test]
fn a_kick_within_five_seconds_of_a_pass_waits_for_the_debounce() {
    let done = 100_000;
    assert_eq!(debounce_deadline(done + 1_000, done), Some(done + 5_000));
    assert_eq!(debounce_deadline(done + 4_999, done), Some(done + 5_000));
    assert_eq!(debounce_deadline(done + 5_000, done), None);
}

#[test]
fn remove_log_deletes_a_regular_file_and_tolerates_none() {
    let dir = scratch("log-regular");
    let log = dir.join("t1.log");
    std::fs::write(&log, b"output").expect("write log");
    assert!(remove_log(&log).expect("remove"));
    assert!(!log.exists());
    assert!(!remove_log(&log).expect("missing is fine"));
}

#[test]
fn remove_log_refuses_a_symlink_and_leaves_its_target() {
    let dir = scratch("log-symlink");
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, b"not a log").expect("write victim");
    let log = dir.join("t2.log");
    std::os::unix::fs::symlink(&victim, &log).expect("symlink");
    assert!(remove_log(&log).is_err());
    assert!(victim.exists(), "the symlink's target must survive");
    assert!(
        std::fs::symlink_metadata(&log).is_ok(),
        "the link is left alone"
    );
}

#[test]
fn remove_task_files_refuses_a_symlinked_log_and_removes_a_regular_one() {
    let root = scratch("task-files");
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs).expect("logs dir");
    let victim = root.join("outside.txt");
    std::fs::write(&victim, b"keep me").expect("write victim");
    std::os::unix::fs::symlink(&victim, logs.join("linked.log")).expect("symlink");
    assert!(remove_task_files(&root, "linked").is_err());
    assert!(victim.exists());

    std::fs::write(logs.join("plain.log"), b"log").expect("write log");
    remove_task_files(&root, "plain").expect("regular log and no job dir is fine");
    assert!(!logs.join("plain.log").exists());
}

#[test]
fn remove_task_files_rejects_an_invalid_task_name() {
    let root = scratch("task-name");
    assert!(remove_task_files(&root, "../escape").is_err());
}
