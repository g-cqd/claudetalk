use super::*;

/// The daemon stops, and only the supervisor is killed; the command lives on. The restarted daemon keeps the task
/// running with the orphan note. Killing the command and then a WorkStatus (which runs the sweep) fails the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_whose_command_outlives_its_supervisor_stays_running_until_a_later_sweep() {
    let d = start_on(new_home(), |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(&mut alice, d.home.path(), "orphan", &["/bin/sleep", "30"]).await;
    await_task(&mut alice, "orphan", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "orphan");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);
    let command = info
        .pid
        .expect("the command's pid is recorded once it runs");

    signal_pid(info.supervisor_pid, Sig::Kill).expect("kill the supervisor only");
    until_gone(
        info.supervisor_pid,
        info.supervisor_start_s,
        "the supervisor",
    )
    .await;
    assert!(
        alive(command, info.pid_start_s),
        "the command {command} outlives its supervisor"
    );

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let orphaned = await_task(&mut alice, "orphan", "record the orphan note", |t| {
        t.note == orphan_note(info.supervisor_pid, command)
    })
    .await;
    assert_eq!(
        orphaned.state,
        TaskState::Running,
        "a job whose command lives on stays running"
    );
    assert_eq!(orphaned.status, None, "no status is recorded yet");

    signal_group(command, Sig::Kill).expect("kill the orphaned command");
    until_gone(command, info.pid_start_s, "the orphaned command").await;
    let still = show(&mut alice, "orphan").await;
    assert_eq!(
        still.state,
        TaskState::Running,
        "nothing has swept the job yet"
    );

    alice.ok(Req::WorkStatus { scope: None }).await;
    let failed = show(&mut alice, "orphan").await;
    assert_eq!(
        failed.state,
        TaskState::Failed,
        "a WorkStatus sweep fails the job"
    );
    assert_eq!(failed.status, None, "no status was ever recorded");
    assert_eq!(
        failed.note,
        ended_without_status_note(info.supervisor_pid),
        "the sweep names the dead supervisor"
    );
}

/// A job whose command ends while the daemon is down: its supervisor writes status.json during the downtime. The
/// restarted daemon finalizes it from that file as done with status 0.
///
/// The command waits for a gate file, not for `true`: the daemon must be stopped while the job is still running, and
/// a command that ends at once would finish before it could be stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_finished_while_the_daemon_was_down_is_recorded_done_on_restart() {
    let home = new_home();
    let root = home.path().to_path_buf();
    let gate = root.join("gate");
    let script = format!(
        "until [ -e '{}' ]; do /bin/sleep 0.05; done",
        gate.display()
    );
    let d = start_on(home, |_| {}).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "finish",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    await_task(&mut alice, "finish", "start running", |t| {
        t.state == TaskState::Running
    })
    .await;
    let home = stop(d).await;
    let info = job_info(home.path(), "finish");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let status_path = job_dir(home.path(), "finish");
    poll("the supervisor writes status.json", || {
        status_path.read_status().ok().flatten()
    })
    .await;

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "finish", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the finished job is done");
    assert_eq!(
        done.status,
        Some(0),
        "the status from status.json is recorded"
    );
}
