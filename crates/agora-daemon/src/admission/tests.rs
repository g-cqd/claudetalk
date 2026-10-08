//! Unit tests of the admission pass's pure input conversion (`build_input`) and its helpers.

use super::*;
use agora_sched::model::TaskState;
use agora_sched::predict::Prediction;

fn task(id: &str) -> TaskRecord {
    TaskRecord {
        seq: 1,
        id: id.into(),
        title: String::new(),
        kind: "other".into(),
        tier: 1,
        scope: None,
        author: "author".into(),
        author_name: "author".into(),
        state: TaskState::Claimed,
        agent: None,
        agent_name: None,
        accepted_by: None,
        needs_acceptance: false,
        after: vec![],
        size: 1.0,
        value: 1.0,
        files: vec![],
        weight: 1.0,
        command: None,
        cwd: None,
        digest: None,
        queued_ms: 0,
        claimed_ms: None,
        requested_ms: Some(0),
        started_ms: None,
        ended_ms: None,
        status: None,
        pid: None,
        supervisor_pid: None,
        log: None,
        qos: None,
        prediction: None,
        cpu_percent: None,
        rss_kib: None,
        rss_peak_kib: None,
        cpu_seconds: None,
        sampled_ms: None,
        note: String::new(),
        card: None,
        origin: None,
        exclusive: false,
        quiet_until_ms: None,
        cancel_requested: false,
    }
}

fn running(id: &str) -> TaskRecord {
    TaskRecord {
        state: TaskState::Running,
        started_ms: Some(0),
        ..task(id)
    }
}

fn prediction(level: Level, cores: f64) -> Prediction {
    Prediction {
        cores,
        rss_kib: None,
        rss_p90_kib: None,
        duration_s: 10.0,
        duration_p90_s: None,
        level,
        runs: 0,
    }
}

fn machine() -> MachineSnapshot {
    MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: None,
        disk_free_kib: None,
        signals: None,
    }
}

fn snapshot(running: Vec<TaskRecord>, waiting: Vec<TaskRecord>) -> Snapshot {
    Snapshot {
        settings: WorkSettings::default(),
        running,
        waiting,
        window_closes: vec![],
        paused: None,
        leases: vec![],
    }
}

fn quiet_lease(id: &str, expected_end_ms: Option<i64>) -> LeaseRecord {
    LeaseRecord {
        id: id.into(),
        kind: LeaseKind::Quiet,
        resource: String::new(),
        amount: 0.0,
        holder: "holder".into(),
        holder_name: "bob".into(),
        card: None,
        task: None,
        created_ms: 0,
        expires_ms: None,
        heartbeat_ms: 0,
        expected_end_ms,
        released_ms: None,
        note: "deploy".into(),
    }
}

#[test]
fn pred_cores_uses_a_known_prediction_over_the_cpu_sample() {
    let mut t = task("t");
    t.weight = 4.0;
    t.cpu_percent = Some(50.0);
    t.prediction = Some(prediction(Level::Full, 3.0));
    assert_eq!(pred_cores(&t), Some(3.0));
}

#[test]
fn pred_cores_estimates_from_the_cpu_sample_when_the_level_is_none() {
    let mut t = task("t");
    t.weight = 2.0;
    t.prediction = Some(prediction(Level::None, 2.0));
    t.cpu_percent = Some(250.0);
    assert_eq!(pred_cores(&t), Some(2.0), "capped at the weight");
    t.weight = 4.0;
    t.cpu_percent = Some(40.0);
    assert_eq!(pred_cores(&t), Some(0.4));
    t.cpu_percent = Some(1.0);
    assert_eq!(pred_cores(&t), Some(0.1), "floored at 0.1 cores");
}

#[test]
fn pred_cores_estimates_from_the_cpu_sample_when_there_is_no_prediction() {
    let mut t = task("t");
    t.weight = 3.0;
    t.cpu_percent = Some(100.0);
    assert_eq!(pred_cores(&t), Some(1.0));
}

#[test]
fn pred_cores_falls_back_to_the_stored_cores_or_none() {
    let mut t = task("t");
    assert_eq!(
        pred_cores(&t),
        None,
        "no prediction and no sample: the declared weight counts"
    );
    t.prediction = Some(prediction(Level::None, 2.5));
    assert_eq!(
        pred_cores(&t),
        Some(2.5),
        "level none without a sample keeps the stored cores"
    );
}

#[test]
fn build_input_converts_milliseconds_to_seconds() {
    let mut run = running("r");
    run.started_ms = Some(2_000);
    run.quiet_until_ms = Some(9_500);
    let mut wait = task("w");
    wait.requested_ms = Some(3_000);
    let mut snap = snapshot(vec![run], vec![wait]);
    snap.window_closes = vec![("w0".into(), 4_000)];

    let input = build_input(12_345, &snap, machine(), &HashMap::new());

    assert_eq!(input.now_s, 12.345);
    assert_eq!(input.running[0].started_s, Some(2.0));
    assert_eq!(input.running[0].quiet_until_s, Some(9.5));
    assert_eq!(input.waiting[0].requested_s, 3.0);
    assert_eq!(input.window_closes, vec![("w0".to_string(), 4.0)]);
}

#[test]
fn build_input_waiting_requested_falls_back_to_queue_time() {
    let mut wait = task("w");
    wait.requested_ms = None;
    wait.queued_ms = 7_000;
    let input = build_input(
        10_000,
        &snapshot(vec![], vec![wait]),
        machine(),
        &HashMap::new(),
    );
    assert_eq!(input.waiting[0].requested_s, 7.0);
}

#[test]
fn build_input_maps_active_quiet_leases_and_skips_those_without_an_end() {
    let mut snap = snapshot(vec![], vec![]);
    snap.leases = vec![quiet_lease("l1", Some(90_000)), quiet_lease("l2", None)];

    let input = build_input(0, &snap, machine(), &HashMap::new());

    assert_eq!(input.manual_quiet.len(), 1);
    let q = &input.manual_quiet[0];
    assert_eq!(q.lease, "l1");
    assert_eq!(q.holder_name, "bob");
    assert_eq!(q.until_s, 90.0);
    assert_eq!(q.note, "deploy");
}

#[test]
fn build_input_carries_held_since_only_for_the_jobs_it_names() {
    let snap = snapshot(vec![], vec![task("t1"), task("t2")]);
    let mut held = HashMap::new();
    held.insert("t2".to_string(), 5.0);
    held.insert("gone".to_string(), 1.0);

    let input = build_input(10_000, &snap, machine(), &held);

    assert_eq!(input.waiting[0].machine_held_since_s, None);
    assert_eq!(input.waiting[1].machine_held_since_s, Some(5.0));
}

#[test]
fn build_input_passes_pause_settings_and_machine_through() {
    let mut snap = snapshot(vec![], vec![]);
    snap.paused = Some("disk low".into());
    let input = build_input(0, &snap, machine(), &HashMap::new());
    assert_eq!(input.paused.as_deref(), Some("disk low"));
    assert_eq!(input.settings, WorkSettings::default());
    assert_eq!(input.machine, machine());
}

#[test]
fn build_input_maps_the_prediction_p90_fields() {
    let mut wait = task("w");
    let mut p = prediction(Level::Head, 1.5);
    p.duration_s = 30.0;
    p.duration_p90_s = Some(60.0);
    p.rss_p90_kib = Some(2048);
    wait.prediction = Some(p);
    let mut run = running("r");
    run.rss_kib = Some(512);
    run.prediction = Some(prediction(Level::Full, 2.0));
    let input = build_input(
        0,
        &snapshot(vec![run], vec![wait]),
        machine(),
        &HashMap::new(),
    );

    let w = &input.waiting[0];
    assert_eq!(w.pred_cores, Some(1.5));
    assert_eq!(w.pred_duration_s, Some(30.0));
    assert_eq!(w.pred_duration_p90_s, Some(60.0));
    assert_eq!(w.pred_rss_p90_kib, Some(2048));
    let r = &input.running[0];
    assert_eq!(r.rss_kib, Some(512));
    assert_eq!(r.pred_cores, Some(2.0));
    assert_eq!(r.pred_duration_p90_s, None);
}

#[test]
fn built_input_lets_decide_start_a_light_job_on_an_idle_machine() {
    let input = build_input(
        10_000,
        &snapshot(vec![], vec![task("w")]),
        machine(),
        &HashMap::new(),
    );
    let d = agora_sched::quiet::decide(&input);
    assert_eq!(d.start.len(), 1);
    assert_eq!(d.start[0].id, "w");
    assert!(d.fail.is_empty());
}

#[test]
fn secs_and_ms_conversions_round_as_documented() {
    assert_eq!(s_to_ms_ceil(1.0001), 1001, "wake-ups round up");
    assert_eq!(
        secs_to_ms(1.2346),
        1235,
        "quiet ends round to the nearest ms"
    );
    assert_eq!(
        secs_to_ms(ms_to_s(1_699)),
        1_699,
        "a stored end survives the round trip"
    );
}

#[test]
fn sync_outcome_mirrors_the_store_final_state() {
    assert_eq!(
        sync_outcome(true, true, None),
        JobOutcome::Cancelled,
        "a cancel wins over the before-start failure"
    );
    assert_eq!(sync_outcome(true, false, Some(0)), JobOutcome::Cancelled);
    assert_eq!(sync_outcome(false, true, None), JobOutcome::Failed);
    assert_eq!(
        sync_outcome(false, true, Some(0)),
        JobOutcome::Failed,
        "a job that never started is never done"
    );
    assert_eq!(sync_outcome(false, false, Some(0)), JobOutcome::Done);
    assert_eq!(sync_outcome(false, false, Some(3)), JobOutcome::Failed);
    assert_eq!(sync_outcome(false, false, None), JobOutcome::Failed);
}

#[test]
fn drain_queued_consumes_every_queued_event() {
    let (tx, mut rx) = broadcast::channel(16);
    for i in 0..5 {
        assert!(
            tx.send(Event::WorkChanged {
                task: format!("t{i}")
            })
            .is_ok()
        );
    }
    assert!(drain_queued(&mut rx));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn drain_queued_survives_a_lagged_receiver() {
    let (tx, mut rx) = broadcast::channel(2);
    for i in 0..10 {
        assert!(
            tx.send(Event::WorkChanged {
                task: format!("t{i}")
            })
            .is_ok()
        );
    }
    assert!(
        drain_queued(&mut rx),
        "lagged events are dropped, not fatal"
    );
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn drain_queued_on_an_empty_open_bus_returns_at_once() {
    let (_tx, mut rx) = broadcast::channel::<Event>(4);
    assert!(drain_queued(&mut rx));
}

#[test]
fn drain_queued_reports_a_closed_bus() {
    let (tx, mut rx) = broadcast::channel::<Event>(4);
    drop(tx);
    assert!(!drain_queued(&mut rx));
}

#[test]
fn a_card_sync_is_signed_with_the_card_as_its_subject() {
    let key = Identity::from_seed([7; 32]);
    let (finish, sync) = sign_before_start(
        &key,
        1_000,
        "task-1",
        "boom",
        JobOutcome::Failed,
        Some("card-9"),
    )
    .expect("sign");
    assert_eq!(finish.kind, "job.finish");
    assert_eq!(finish.subject, "task-1", "the job's own op names the task");
    let sync = sync.expect("a card sync");
    assert_eq!(sync.kind, "card.job_sync");
    assert_eq!(
        sync.subject, "card-9",
        "the card is the subject, as jobs::finish_attempt signs it"
    );
    assert_eq!(sync.actor, key.public());
    let args: CardSyncArgs = serde_json::from_str(&sync.arg).expect("sync args");
    assert_eq!(args.task, "task-1");
    assert_eq!(args.outcome, JobOutcome::Failed);
}

#[test]
fn an_unlinked_task_signs_no_card_sync() {
    let key = Identity::from_seed([7; 32]);
    let (finish, sync) =
        sign_before_start(&key, 1_000, "task-1", "boom", JobOutcome::Failed, None).expect("sign");
    assert_eq!(finish.subject, "task-1");
    assert!(sync.is_none());
}

#[test]
fn each_attempt_is_signed_with_a_fresh_nonce() {
    // A retry without the sync signs the job's op again: identical inputs never repeat a nonce.
    let key = Identity::from_seed([7; 32]);
    let first = sign_before_start(&key, 1_000, "t", "n", JobOutcome::Failed, None)
        .expect("sign")
        .0;
    let again = sign_before_start(&key, 1_000, "t", "n", JobOutcome::Failed, None)
        .expect("sign")
        .0;
    assert_ne!(first.nonce, again.nonce);
    assert_ne!(first.sig, again.sig);
}

#[test]
fn a_store_error_on_a_synced_write_is_retried_without_the_sync() {
    let refused = DaemonError::Store(StoreError::Invalid(
        "card.job_sync does not match the job it finishes",
    ));
    assert!(retry_without_sync(true, &refused));
    assert!(
        !retry_without_sync(false, &refused),
        "an attempt with no sync has nothing to shed"
    );
    assert!(
        !retry_without_sync(true, &DaemonError::ActorGone),
        "a dead writer is not retried"
    );
    assert!(!retry_without_sync(
        true,
        &DaemonError::Config("could not encode card.job_sync".into())
    ));
}

#[test]
fn a_failed_write_asks_for_a_retry_after_the_error_retry_interval() {
    assert_eq!(retry_at(10_000, true), Some(10_000 + RETRY_MS));
    assert_eq!(retry_at(10_000, false), None, "nothing failed, no retry");
    assert_eq!(retry_at(i64::MAX, true), Some(i64::MAX), "saturates");
}

#[test]
fn earliest_keeps_the_sooner_wake_up_and_ignores_a_missing_one() {
    assert_eq!(earliest(Some(5), Some(3)), Some(3));
    assert_eq!(earliest(None, Some(3)), Some(3));
    assert_eq!(earliest(Some(5), None), Some(5));
    assert_eq!(earliest(None, None), None);
}
