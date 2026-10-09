use super::*;

/// O-22: the daemon stops after a job is claimed and requested but before its supervisor reports (job.start not yet
/// recorded). The supervisor then comes up during the downtime. The restarted daemon adopts that supervisor and
/// records its start, and the command runs once, not a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_started_job_whose_supervisor_came_up_during_downtime_is_adopted_not_run_again() {
    // The gate lives in its own directory, declared first, so it outlives the guard that opens it on any exit path.
    let gates = tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("a gate dir under /tmp");
    let gate = gates.path().join("gate");
    let _open_gate = GateOnDrop(gate.clone());
    let home = new_home();
    let root = home.path().to_path_buf();
    let mark = root.join("ran.txt");
    let supervisor = env!("CARGO_BIN_EXE_agora-supervise").to_owned();
    let script = format!("echo ran >> '{}'; /bin/sleep 3", mark.display());
    let gated = {
        let gate = gate.display().to_string();
        move |cfg: &mut DaemonConfig| {
            cfg.supervisor_argv = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                GATE_WRAPPER.into(),
                "sh".into(),
                gate.clone(),
                supervisor.clone(),
            ]);
        }
    };
    let held = gates.path().join("gate.held");
    let d = start_on(home, gated).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "gated",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    poll("the supervisor is spawned and held at the gate", || {
        held.exists().then_some(())
    })
    .await;
    let before = show(&mut alice, "gated").await;
    assert_eq!(
        before.state,
        TaskState::Claimed,
        "the job is requested but its supervisor has not reported, so it is not started"
    );

    let home = stop(d).await;
    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let dir = job_dir(home.path(), "gated");
    let info = poll("the supervisor reports its command", || {
        dir.read_info().ok().flatten()
    })
    .await;
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "gated", "end", |t| t.state.is_final()).await;
    assert_eq!(done.state, TaskState::Done, "the adopted job ends as done");
    assert_eq!(done.status, Some(0), "the adopted job records its status 0");
    assert_eq!(
        done.supervisor_pid,
        Some(i64::from(info.supervisor_pid)),
        "the record names the supervisor that came up during the downtime"
    );
    let runs = std::fs::read_to_string(&mark).expect("the command wrote its mark");
    assert_eq!(runs, "ran\n", "the command ran exactly once");
}

/// C1-M1: the daemon stops after a job is claimed and requested but before job.start is recorded. Its supervisor starts a
/// short command during the downtime, and the command finishes before the daemon returns, so status.json says started
/// while the task is still claimed. The restarted daemon records the start and finishes the job as done with status 0.
/// The command runs exactly once: nothing re-admits the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claimed_job_whose_command_finished_during_downtime_is_finished_once_not_run_again() {
    // The gate lives in its own directory, declared first, so it outlives the guard that opens it on any exit path.
    let gates = tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("a gate dir under /tmp");
    let gate = gates.path().join("gate");
    let _open_gate = GateOnDrop(gate.clone());
    let home = new_home();
    let root = home.path().to_path_buf();
    let mark = root.join("ran.txt");
    let supervisor = env!("CARGO_BIN_EXE_agora-supervise").to_owned();
    let script = format!("echo once >> '{}'", mark.display());
    let gated = {
        let gate = gate.display().to_string();
        move |cfg: &mut DaemonConfig| {
            cfg.supervisor_argv = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                GATE_WRAPPER.into(),
                "sh".into(),
                gate.clone(),
                supervisor.clone(),
            ]);
        }
    };
    let held = gates.path().join("gate.held");
    let d = start_on(home, gated).await;
    let mut alice = world(&d).await.alice;
    run_inline(
        &mut alice,
        &root,
        "once",
        &["/bin/sh", "-c", script.as_str()],
    )
    .await;
    poll("the supervisor is spawned and held at the gate", || {
        held.exists().then_some(())
    })
    .await;
    let before = show(&mut alice, "once").await;
    assert_eq!(
        before.state,
        TaskState::Claimed,
        "the job is requested but its supervisor has not reported, so it is not started"
    );

    let home = stop(d).await;
    std::fs::write(&gate, b"").expect("open the gate while the daemon is down");
    let dir = job_dir(home.path(), "once");
    let status = poll("the command finishes and status.json is written", || {
        dir.read_status().ok().flatten()
    })
    .await;
    assert!(
        status.started,
        "the supervisor reports that the command started"
    );
    let info = job_info(home.path(), "once");
    let mut cleanup = Cleanup::default();
    cleanup.track(&info);

    let d = start_on(home, |_| {}).await;
    let mut alice = connect(&d, ident(3)).await;
    let done = await_task(&mut alice, "once", "end", |t| t.state.is_final()).await;
    assert_eq!(
        done.state,
        TaskState::Done,
        "the job that finished during the downtime is done"
    );
    assert_eq!(
        done.status,
        Some(0),
        "the status from status.json is recorded"
    );
    assert_eq!(
        done.supervisor_pid,
        Some(i64::from(info.supervisor_pid)),
        "the recorded start names the supervisor that reported it"
    );

    // A re-admitted job would start within moments of the restart; a few seconds of quiet show that none does.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = show(&mut alice, "once").await;
    assert_eq!(
        later.state,
        TaskState::Done,
        "nothing started the job again"
    );
    let runs = std::fs::read_to_string(&mark).expect("the command wrote its mark");
    assert_eq!(runs, "once\n", "the command ran exactly once");
}

/// Opens a gate file when dropped, so a supervisor held at the gate never outlives its test.
struct GateOnDrop(std::path::PathBuf);

impl Drop for GateOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"");
    }
}
