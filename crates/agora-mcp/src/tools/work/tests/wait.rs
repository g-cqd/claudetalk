//! Argument mapping of the `work_wait` call: the condition each wait takes, its refusals, and which waits run on this machine.

use super::*;

#[test]
fn tasks_wait_takes_its_ids_and_any() {
    let a = WorkWaitArgs {
        tasks: Some(vec!["a".into(), "b".into()]),
        any: Some(true),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&a),
        Ok(WaitSpec::Tasks {
            ids: vec!["a".into(), "b".into()],
            any: true,
        })
    );
}

#[test]
fn all_wait_takes_a_scope_only_when_it_is_set() {
    let a = WorkWaitArgs {
        all: Some(true),
        scope: Some("proj".into()),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&a),
        Ok(WaitSpec::All {
            scope: Some("proj".into())
        })
    );
    let blank = WorkWaitArgs {
        all: Some(true),
        scope: Some(String::new()),
        ..wait_args()
    };
    assert_eq!(wait_spec(&blank), Ok(WaitSpec::All { scope: None }));
}

#[test]
fn cards_channel_and_notices_map_their_fields() {
    let cards = WorkWaitArgs {
        cards: Some(vec!["c1".into()]),
        states: Some(vec!["done".into()]),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&cards),
        Ok(WaitSpec::Cards {
            ids: vec!["c1".into()],
            states: vec!["done".into()],
        })
    );
    let channel = WorkWaitArgs {
        channel: Some("general".into()),
        after: Some(7),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&channel),
        Ok(WaitSpec::Messages {
            channel: "general".into(),
            after: 7,
        })
    );
    let notices = WorkWaitArgs {
        notices: Some(true),
        ..wait_args()
    };
    assert_eq!(wait_spec(&notices), Ok(WaitSpec::Notices { after: -1 }));
}

#[test]
fn pid_file_gone_and_log_map_their_targets() {
    let pid = WorkWaitArgs {
        pid: Some(vec![42]),
        ..wait_args()
    };
    assert_eq!(wait_spec(&pid), Ok(WaitSpec::Pid { pids: vec![42] }));
    let file = WorkWaitArgs {
        file: Some(vec!["out.txt".into()]),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&file),
        Ok(WaitSpec::File {
            paths: vec!["out.txt".into()]
        })
    );
    let gone = WorkWaitArgs {
        gone: Some(vec!["lock".into()]),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&gone),
        Ok(WaitSpec::Gone {
            paths: vec!["lock".into()]
        })
    );
    let log = WorkWaitArgs {
        log_path: Some("build.log".into()),
        pattern: Some("Finished".into()),
        ..wait_args()
    };
    assert_eq!(
        wait_spec(&log),
        Ok(WaitSpec::Log {
            path: "build.log".into(),
            pattern: "Finished".into(),
        })
    );
}

#[test]
fn a_wait_without_a_condition_names_the_choices() {
    assert_eq!(refusal(wait_spec(&wait_args())), WAIT_ONE);
    // An empty list is no condition.
    let empty = WorkWaitArgs {
        tasks: Some(Vec::new()),
        ..wait_args()
    };
    assert_eq!(refusal(wait_spec(&empty)), WAIT_ONE);
}

#[test]
fn a_wait_with_two_conditions_names_both() {
    let a = WorkWaitArgs {
        tasks: Some(vec!["a".into()]),
        pid: Some(vec![42]),
        ..wait_args()
    };
    assert_eq!(
        refusal(wait_spec(&a)),
        format!("{WAIT_ONE}, not tasks and pid")
    );
}

#[test]
fn the_log_condition_needs_its_path_and_pattern() {
    let only_path = WorkWaitArgs {
        log_path: Some("build.log".into()),
        ..wait_args()
    };
    assert_eq!(
        refusal(wait_spec(&only_path)),
        "the log condition needs both log_path and pattern"
    );
    let only_pattern = WorkWaitArgs {
        pattern: Some("x".into()),
        ..wait_args()
    };
    assert_eq!(
        refusal(wait_spec(&only_pattern)),
        "the log condition needs both log_path and pattern"
    );
}

#[test]
fn any_goes_with_tasks_and_scope_goes_with_all() {
    let any = WorkWaitArgs {
        pid: Some(vec![42]),
        any: Some(true),
        ..wait_args()
    };
    assert_eq!(refusal(wait_spec(&any)), "any goes with tasks");
    let scope = WorkWaitArgs {
        tasks: Some(vec!["a".into()]),
        scope: Some("proj".into()),
        ..wait_args()
    };
    assert_eq!(refusal(wait_spec(&scope)), "scope goes with all");
}

#[test]
fn a_pid_must_be_a_positive_process_id() {
    let a = WorkWaitArgs {
        pid: Some(vec![7, 0]),
        ..wait_args()
    };
    assert_eq!(refusal(wait_spec(&a)), "pid 0 is not a process id");
}

#[test]
fn the_watchers_are_the_local_waits_and_the_rest_are_daemon_side() {
    let local = [
        WaitSpec::Pid { pids: vec![1] },
        WaitSpec::File { paths: vec![] },
        WaitSpec::Gone { paths: vec![] },
        WaitSpec::Log {
            path: String::new(),
            pattern: String::new(),
        },
    ];
    for spec in &local {
        assert!(local_wait(spec).is_some(), "{spec:?} watches this machine");
    }
    let daemon = [
        WaitSpec::Tasks {
            ids: vec![],
            any: false,
        },
        WaitSpec::All { scope: None },
        WaitSpec::Cards {
            ids: vec![],
            states: vec![],
        },
        WaitSpec::Notices { after: 0 },
    ];
    for spec in &daemon {
        assert_eq!(local_wait(spec), None, "{spec:?} is daemon-side");
    }
}

#[test]
fn a_wait_is_capped_at_fifty_five_seconds() {
    assert_eq!(wait_secs(0), 0);
    assert_eq!(wait_secs(55), 55);
    assert_eq!(wait_secs(u64::MAX), 55);
}
