use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pid_wait_returns_status_zero_once_the_child_has_exited() {
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
        WAIT_MS,
        None,
    )
    .await;

    assert_eq!(result.condition, "pid");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![pid.to_string()]);
    assert!(
        child.try_wait().expect("poll the child").is_some(),
        "the wait returned before the child exited"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_wait_returns_when_the_file_is_created() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("ready.txt");
    let started = Instant::now();
    let writer = create_later(file.clone(), b"done\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::File {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert_eq!(result.condition, "file");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0, "a file that names no status ends with 0");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_wait_with_one_path_ends_with_the_number_the_file_holds() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("status.txt");
    fs::write(&file, b"7\n").expect("write the status file");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::File {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 7);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gone_wait_returns_when_the_file_is_removed() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let file = d.home.path().join("lock.txt");
    fs::write(&file, b"held\n").expect("write the lock file");
    let started = Instant::now();
    let remover = remove_later(file.clone());

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Gone {
            paths: vec![path_str(&file)],
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(remover).await;

    assert_eq!(result.condition, "gone");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_matches_a_line_written_after_the_wait_began() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("build.log");
    let started = Instant::now();
    let writer = append_later(log.clone(), b"booting\nready\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^ready$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert_eq!(result.condition, "log");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec![path_str(&log)]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_line_split_across_two_writes_matches_once_the_line_is_complete() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("split.log");
    fs::write(&log, b"hello").expect("write the first half");
    let started = Instant::now();
    let writer = append_later(log.clone(), b" world\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^hello world$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert!(!result.timed_out, "the completed line matches");
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_line_already_in_the_file_matches_at_once() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("old.log");
    fs::write(&log, b"booting\nready\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^ready$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;

    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_pattern_with_non_ascii_text_matches_when_its_character_is_split_across_writes() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("unicode.log");
    // "é" is C3 A9; the first write ends inside it.
    fs::write(&log, b"caf\xC3").expect("write the first bytes");
    let started = Instant::now();
    let writer = append_later(log.clone(), b"\xA9\n");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^café$".to_string(),
        },
        WAIT_MS,
        None,
    )
    .await;
    assert_waited_for_the_change(started);
    join(writer).await;

    assert!(
        !result.timed_out,
        "the character is whole once its last byte lands"
    );
    assert_eq!(result.status, 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_refuses_a_pattern_that_is_not_a_regular_expression() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("any.log");
    fs::write(&log, b"x\n").expect("write the log");

    let error = wait_result(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "(".to_string(),
        },
        1_000,
        None,
    )
    .await
    .expect_err("an invalid pattern is refused");

    assert_eq!(error.code, ErrorCode::Invalid);
    assert!(
        error.message.contains("not a valid regular expression"),
        "the message names the pattern error, got {:?}",
        error.message
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_log_wait_that_times_out_reports_124_with_a_cursor_and_its_log_as_target() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("quiet.log");
    fs::write(&log, b"nothing here\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^never$".to_string(),
        },
        500,
        None,
    )
    .await;

    assert_eq!(result.condition, "log");
    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.targets, vec![path_str(&log)]);
    assert!(result.cursor.is_some(), "a timed-out wait returns a cursor");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_log_wait_lists_its_log_as_pending() {
    let d = daemon().await;
    let mut w = world(&d).await;
    let log = d.home.path().join("pending.log");
    fs::write(&log, b"nothing here\n").expect("write the log");

    let result = wait_on(
        &mut w.alice,
        WaitSpec::Log {
            path: path_str(&log),
            pattern: "^never$".to_string(),
        },
        500,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.pending, vec![path_str(&log)]);
    d.stop().await;
}
