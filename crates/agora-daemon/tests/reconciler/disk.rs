use super::*;

/// The rig with the disk below its floor and the pass that opens the breach run.
async fn breached_rig() -> Rig {
    let r = rig().await;
    r.machine.update(|s| s.disk_free_kib = Some(LOW_DISK_KIB));
    r.clock.pass_at(r.start + 30 * SECOND).await;
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disk_below_its_floor_opens_a_blocker_card_and_pauses_admission() {
    let mut r = breached_rig().await;

    assert_eq!(
        board_extras(&mut r.w.owner).await.paused,
        Some(DISK_BREACH.to_string()),
        "the board reports admission paused with the breach"
    );
    assert_eq!(
        status_paused(&mut r.w.owner).await,
        Some(DISK_BREACH.to_string()),
        "work.status reports the same pause"
    );
    let blocked = cards_in(&mut r.w.owner, "blocked").await;
    assert_eq!(blocked.len(), 1, "one blocker card is open");
    let blocker = blocked[0].id.clone();
    assert!(blocker.starts_with("blocker-"), "blocker id {blocker}");
    assert_eq!(
        board_extras(&mut r.w.owner).await.needs_owner,
        vec![NeedsOwner {
            card: blocker,
            title: BLOCKER_TITLE.to_string(),
            owner_action: BLOCKER_ACTION.to_string(),
        }],
        "the blocker needs an owner, with the action that frees the disk"
    );
    r.d.stop().await;
}

/// The breached rig with one inline job submitted by alice. The job is held, and its note says so.
async fn held_job(r: &mut Rig) -> TaskRecord {
    let cwd = r.d.home.path().to_path_buf();
    let task = run_inline(&mut r.w.alice, &cwd).await;
    await_hold_note(
        &mut r.w.alice,
        &task.id,
        &format!("admission is paused: {DISK_BREACH}"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_job_waits_with_the_pause_reason_while_admission_is_paused() {
    let mut r = breached_rig().await;

    let task = held_job(&mut r).await;

    assert!(task.started_ms.is_none(), "a held job has not started");
    assert_eq!(
        task.state,
        TaskState::Claimed,
        "a held job stays claimed, not running"
    );
    match r.w.alice.ok(Req::WorkStatus { scope: None }).await {
        Reply::WorkStatus { report } => assert!(
            report.waiting.iter().any(|t| t.id == task.id),
            "the job is in the waiting line"
        ),
        other => panic!("work.status: expected WorkStatus, got {other:?}"),
    }
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restoring_the_disk_resumes_admission_and_cancels_the_blocker_card() {
    let mut r = breached_rig().await;
    let task = held_job(&mut r).await;
    assert!(task.started_ms.is_none());

    r.machine.update(|s| s.disk_free_kib = Some(100 * GIB_KIB));
    r.clock.pass_at(r.start + 60 * SECOND).await;

    assert_eq!(
        board_extras(&mut r.w.owner).await.paused,
        None,
        "the board no longer reports a pause"
    );
    assert_eq!(status_paused(&mut r.w.owner).await, None);
    assert!(
        cards_in(&mut r.w.owner, "blocked").await.is_empty(),
        "the blocker is no longer blocked"
    );
    let cancelled = cards_in(&mut r.w.owner, "cancelled").await;
    assert_eq!(cancelled.len(), 1, "the blocker card is cancelled");
    assert!(cancelled[0].id.starts_with("blocker-"));
    assert!(
        board_extras(&mut r.w.owner).await.needs_owner.is_empty(),
        "a cancelled blocker needs no owner"
    );

    let result = wait_for_end(&mut r.w.alice, &task.id).await;
    assert_eq!(result.status, 0, "the held job runs once admission resumes");
    let finished = show(&mut r.w.alice, &task.id).await;
    assert!(finished.started_ms.is_some(), "the job started");
    assert_eq!(finished.state, TaskState::Done);
    r.d.stop().await;
}
