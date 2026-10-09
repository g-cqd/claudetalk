use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resuming_a_timed_out_wait_with_its_cursor_adds_the_new_seconds_to_the_old() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("resume.log");
    fs::write(&log, b"nothing here\n").expect("write the log");
    let spec = || WaitSpec::Log {
        path: path_str(&log),
        pattern: "^never$".to_string(),
    };
    let first = wait_on(&mut w.alice, spec(), 500, None).await;
    assert!(first.timed_out);

    let second = wait_on(&mut w.alice, spec(), 500, first.cursor.clone()).await;

    assert!(second.timed_out);
    assert_eq!(second.status, 124);
    assert!(
        second.seconds >= first.seconds + 0.4,
        "the second wait adds its own ~0.5 s to the first: {} then {}",
        first.seconds,
        second.seconds
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_asking_longer_than_the_cap_still_returns_when_its_condition_is_met() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let mut child = Command::new("sleep")
        .arg("1")
        .spawn()
        .expect("start a child process");
    let pid = i64::from(child.id());

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Pid { pids: vec![pid] },
        120_000,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    child.wait().expect("reap the child");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_naming_more_than_256_targets_is_refused_as_invalid() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let paths = absent_paths(d.home.path(), 257);

    let error = wait_result(&mut w.alice, WaitSpec::Gone { paths }, 1_000, None)
        .await
        .expect_err("257 targets are refused");

    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.message, "name at most 256 targets");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_naming_exactly_256_targets_is_accepted() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let paths = absent_paths(d.home.path(), 256);

    let result = wait_on(&mut w.alice, WaitSpec::Gone { paths }, WAIT_MS, None).await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets.len(), 256);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_is_refused_every_pid_file_gone_and_log_wait() {
    let d = daemon().await;
    let mut w = world(&d).await;
    grant(&mut w.owner, w.carol.pk_hex(), "observer").await;
    let file = d.home.path().join("watched.txt");
    fs::write(&file, b"x\n").expect("write the file");
    let me = i64::from(std::process::id());
    let specs = [
        ("pid", WaitSpec::Pid { pids: vec![me] }),
        (
            "file",
            WaitSpec::File {
                paths: vec![path_str(&file)],
            },
        ),
        (
            "gone",
            WaitSpec::Gone {
                paths: vec![path_str(&file)],
            },
        ),
        (
            "log",
            WaitSpec::Log {
                path: path_str(&file),
                pattern: "x".to_string(),
            },
        ),
    ];

    for (name, spec) in specs {
        let error = wait_result(&mut w.carol, spec, 1_000, None)
            .await
            .expect_err(name);
        assert_eq!(
            error.code,
            ErrorCode::Forbidden,
            "a {name} wait by an observer"
        );
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_may_wait_on_a_task() {
    let d = daemon().await;
    let mut w = world(&d).await;
    grant(&mut w.owner, w.carol.pk_hex(), "observer").await;
    let task = run_inline(&mut w.owner, d.home.path(), &["true"]).await;

    let result = wait_on(
        &mut w.carol,
        tasks_spec(vec![task.id], false),
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_may_not_wait_on_a_job_log_by_path_but_a_supervisor_may() {
    let d = daemon().await;
    let mut w = world(&d).await;
    // A job's log where the daemon keeps it; the pattern already matches, so a permitted wait returns at once.
    let log = jobdir::log_path(&jobdir::work_root(d.home.path()), "gate-job");
    fs::create_dir_all(log.parent().expect("a logs directory")).expect("create the logs directory");
    fs::write(&log, b"ready\n").expect("write the job log");
    let spec = WaitSpec::Log {
        path: path_str(&log),
        pattern: "^ready$".to_string(),
    };

    // Bob registered as a worker: a job's log is read through its task wait, which applies S-2, not by path.
    let error = wait_result(&mut w.bob, spec.clone(), 1_000, None)
        .await
        .expect_err("a worker's log wait under the work root");
    assert_eq!(error.code, ErrorCode::Forbidden);

    grant(&mut w.owner, w.carol.pk_hex(), "supervisor").await;
    let result = wait_on(&mut w.carol, spec, WAIT_MS, None).await;
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![path_str(&log)]);
    d.stop().await;
}
