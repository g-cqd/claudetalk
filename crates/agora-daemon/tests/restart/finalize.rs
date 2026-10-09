use super::*;

/// Starts a `sleep 8` job, stops the daemon, and starts a new one on the same home: the supervisor survived the
/// restart, the job is still running, and the adopted supervisor's status.json finalizes it as done with status 0.
/// The job is 8 s, not 4 s: under host load the test can observe the start late, and a 4 s job may already have ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_job_outlives_a_daemon_restart_and_is_finalized_from_its_status() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "adopt", &["/bin/sleep", "8"]).await;
    await_task(&mut alice, "adopt", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "adopt");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let during = show(&mut alice, "adopt").await;
    assert_eq!(
        during.state,
        TaskState::Running,
        "the job is still running after the restart"
    );
    assert!(
        alive(info.supervisor_pid, info.supervisor_start_s),
        "the supervisor {} survived the daemon's shutdown",
        info.supervisor_pid
    );

    let done = await_task(&mut alice, "adopt", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the adopted job ends as done");
    assert_eq!(done.status, Some(0), "the adopted job records its status 0");
}

/// The daemon stops while a job runs; the supervisor and its command are killed while the daemon is down. The
/// restarted daemon's startup sweep fails the task with no status, and names the dead supervisor in the note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_supervisor_and_command_died_while_the_daemon_was_down_fails_without_status() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "both", &["/bin/sleep", "30"]).await;
    await_task(&mut alice, "both", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "both");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);
    let command = info
        .pid
        .expect("the command's pid is recorded once it runs");

    signal_pid(info.supervisor_pid, Sig::Kill).expect("kill the supervisor");
    signal_group(command, Sig::Kill).expect("kill the command's process group");
    until_gone(
        info.supervisor_pid,
        info.supervisor_start_s,
        "the supervisor",
    )
    .await;
    until_gone(command, info.pid_start_s, "the command").await;

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let failed = await_task(&mut alice, "both", "end", |t| t.state.is_final()).await;
    assert_eq!(failed.state, TaskState::Failed, "the sweep fails the job");
    assert_eq!(failed.status, None, "no status was ever recorded");
    assert_eq!(
        failed.note,
        ended_without_status_note(info.supervisor_pid),
        "the note names the dead supervisor"
    );
}
