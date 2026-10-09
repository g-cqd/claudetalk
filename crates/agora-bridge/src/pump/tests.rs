//! Pump tests over a scripted `InboxSource`: no daemon and no identity. Time is paused, so the
//! backoffs and pacing are checked on the virtual clock.

use super::*;
use crate::PREAMBLE;
use agora_client::session_state::try_advance;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn msg(seq: i64, text: &str) -> Value {
    json!({
        "seq": seq,
        "id": format!("m-{seq}"),
        "channel": "c",
        "sender": "ab",
        "pseudonym": "p",
        "ts": 1,
        "encrypted": false,
        "epoch": 0,
        "verified": true,
        "text": text,
        "attachments": [],
    })
}

/// A temp dir under /tmp: daemon socket paths under it must stay short.
fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ap")
        .tempdir_in("/tmp")
        .expect("temp dir")
}

/// A temp home holding a machine seed, so an identity can be derived for any folder.
fn seeded_home() -> tempfile::TempDir {
    let home = temp_dir();
    agora_client::identity::load_or_create_machine_seed(&agora_daemon::Paths::new(home.path()))
        .expect("machine seed");
    home
}

/// A pump config over a fresh home, for a session `key`.
fn config(key: &str) -> (tempfile::TempDir, PumpConfig) {
    let home = temp_dir();
    let cfg = PumpConfig::new(
        home.path().to_path_buf(),
        PathBuf::from("/unused"),
        key.to_string(),
    );
    (home, cfg)
}

/// One scripted long poll, taken before the scripted inbox answers.
enum Step {
    /// The poll answers with no effect.
    Quiet,
    /// The poll fails.
    Fail,
    /// The hook advances the session cursor to `seq` during the poll, then the poll answers.
    HookAdvance(i64),
}

/// A daemon double. Its inbox is `msgs`, served as the daemon serves it (seq above `after`, at
/// most `FETCH_LIMIT`); the scripted steps are taken one per poll before that.
struct Script {
    home: PathBuf,
    session: String,
    msgs: Vec<Value>,
    steps: Mutex<VecDeque<Step>>,
    fetches: Mutex<Vec<Instant>>,
    ensures: AtomicUsize,
    beats: AtomicUsize,
}

impl Script {
    fn new(cfg: &PumpConfig, msgs: Vec<Value>, steps: Vec<Step>) -> Self {
        Self {
            home: cfg.home.clone(),
            session: cfg.session_key.clone(),
            msgs,
            steps: Mutex::new(steps.into()),
            fetches: Mutex::new(Vec::new()),
            ensures: AtomicUsize::new(0),
            beats: AtomicUsize::new(0),
        }
    }

    fn fetch_count(&self) -> usize {
        self.fetches.lock().expect("fetch log").len()
    }
}

impl InboxSource for Script {
    async fn ensure(&self) -> Result<(), BridgeError> {
        self.ensures.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn fetch(&self, after: i64, _wait_ms: u64) -> Result<Inbox, BridgeError> {
        self.fetches.lock().expect("fetch log").push(Instant::now());
        let step = self.steps.lock().expect("steps").pop_front();
        match step {
            Some(Step::Quiet) | None => {}
            Some(Step::Fail) => {
                return Err(BridgeError::Protocol("scripted poll failure".to_string()));
            }
            Some(Step::HookAdvance(seq)) => {
                try_advance(&self.home, &self.session, seq).expect("hook advance");
            }
        }
        let messages = self
            .msgs
            .iter()
            .filter(|m| seq_of(m).is_some_and(|seq| seq > after))
            .take(FETCH_LIMIT as usize)
            .cloned()
            .collect();
        Ok(Inbox {
            messages,
            cursor: after,
            timed_out: false,
        })
    }

    fn beat(&self) {
        self.beats.fetch_add(1, Ordering::SeqCst);
    }
}

/// One delivery as the scripted driver saw it.
#[derive(Debug)]
struct Seen {
    text: String,
    max_seq: i64,
    count: usize,
    /// The session cursor on disk when the batch arrived.
    cursor_before: i64,
    at: Instant,
}

/// Plays the driver: records each delivery and answers with the next verdict (`true` once the
/// verdicts run out). Returns when the pump drops its sender.
async fn drive(
    mut rx: mpsc::Receiver<Delivery>,
    mut verdicts: VecDeque<bool>,
    home: PathBuf,
    key: String,
) -> Vec<Seen> {
    let mut seen = Vec::new();
    while let Some(delivery) = rx.recv().await {
        seen.push(Seen {
            text: delivery.text.clone(),
            max_seq: delivery.max_seq,
            count: delivery.count,
            cursor_before: load(&home, &key).cursor,
            at: Instant::now(),
        });
        let verdict = verdicts.pop_front().unwrap_or(true);
        let _ = delivery.reply.send(verdict);
    }
    seen
}

/// Runs the pump over `source` with the scripted driver until `done` holds (checked every 100 ms
/// of virtual time, for at most ten virtual minutes), then shuts it down. Returns the deliveries.
async fn run_until(
    cfg: &PumpConfig,
    source: &Script,
    verdicts: Vec<bool>,
    done: impl Fn() -> bool,
) -> Vec<Seen> {
    let (tx, rx) = mpsc::channel(1);
    let driver = tokio::spawn(drive(
        rx,
        verdicts.into(),
        cfg.home.clone(),
        cfg.session_key.clone(),
    ));
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let watch = async move {
        for _ in 0..6_000 {
            if done() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        stop.cancel();
    };
    let (res, ()) = tokio::join!(run_loop(cfg, source, tx, shutdown), watch);
    res.expect("the pump ends cleanly");
    driver.await.expect("driver task")
}

/// The session cursor on disk for `cfg`.
fn cursor_of(cfg: &PumpConfig) -> i64 {
    load(&cfg.home, &cfg.session_key).cursor
}

#[test]
fn select_fresh_keeps_seq_above_cursor_in_ascending_order() {
    let msgs = vec![msg(5, "e"), msg(2, "b"), msg(3, "c"), msg(1, "a")];
    let seqs: Vec<i64> = select_fresh(msgs, 2).iter().filter_map(seq_of).collect();
    assert_eq!(seqs, vec![3, 5]);
}

#[test]
fn select_fresh_drops_messages_without_an_integer_seq() {
    let mut no_seq = msg(9, "x");
    no_seq.as_object_mut().expect("object").remove("seq");
    let mut text_seq = msg(9, "y");
    text_seq["seq"] = json!("9");
    let fresh = select_fresh(vec![no_seq, text_seq, msg(4, "ok")], 0);
    assert_eq!(fresh.len(), 1);
    assert_eq!(seq_of(&fresh[0]), Some(4));
}

#[test]
fn batch_of_empty_is_none() {
    assert_eq!(batch_of(&[], 9_000), None);
}

#[test]
fn batch_of_wraps_the_render_in_the_preamble_with_max_seq_and_count() {
    let msgs = [msg(1, "alpha"), msg(2, "beta"), msg(3, "gamma")];
    let (text, max_seq, count) = batch_of(&msgs, 9_000).expect("a batch");
    assert!(
        text.starts_with(PREAMBLE),
        "prompt starts with the preamble"
    );
    assert!(text.contains("alpha") && text.contains("gamma"));
    assert_eq!(max_seq, 3);
    assert_eq!(count, 3);
}

#[test]
fn batch_of_counts_only_the_messages_it_shows() {
    let body = "x".repeat(900);
    let msgs: Vec<Value> = (1..=20).map(|s| msg(s, &body)).collect();
    let (text, max_seq, count) = batch_of(&msgs, 2_000).expect("a batch");
    assert!((1..20).contains(&count), "render caps the batch: {count}");
    assert_eq!(max_seq, i64::try_from(count).expect("fits"));
    assert!(text.contains(&format!("seq=\"{max_seq}\"")));
    assert!(!text.contains(&format!("seq=\"{}\"", max_seq + 1)));
    assert!(text.chars().count() <= 2_000 + PREAMBLE.chars().count());
}

#[test]
fn round_pause_tops_a_round_up_to_one_second() {
    assert_eq!(round_pause(Duration::ZERO), Duration::from_secs(1));
    assert_eq!(
        round_pause(Duration::from_millis(400)),
        Duration::from_millis(600)
    );
    assert_eq!(round_pause(Duration::from_secs(1)), Duration::ZERO);
    assert_eq!(round_pause(Duration::from_secs(55)), Duration::ZERO);
}

#[test]
fn next_backoff_doubles_from_one_second_and_caps_at_thirty() {
    let mut d = BACKOFF_FIRST;
    let mut seen = vec![d.as_secs()];
    for _ in 0..6 {
        d = next_backoff(d);
        seen.push(d.as_secs());
    }
    assert_eq!(seen, vec![1, 2, 4, 8, 16, 30, 30]);
}

#[test]
fn refusal_backoff_doubles_from_five_seconds_and_caps_at_five_minutes() {
    let mut d = REFUSAL_FIRST;
    let mut seen = vec![d.as_secs()];
    for _ in 0..7 {
        d = next_refusal(d);
        seen.push(d.as_secs());
    }
    assert_eq!(seen, vec![5, 10, 20, 40, 80, 160, 300, 300]);
}

#[test]
fn beat_window_is_twice_the_wait_and_at_least_one_beat_period() {
    assert_eq!(beat_window(55_000), Duration::from_secs(110));
    assert_eq!(beat_window(1_000), BEAT_EVERY);
    assert_eq!(beat_window(0), BEAT_EVERY);
}

#[tokio::test]
async fn commit_advances_the_cursor_and_never_moves_it_back() {
    let home = temp_dir();
    commit(home.path(), "sess-commit", 5)
        .await
        .expect("commit 5");
    assert_eq!(load(home.path(), "sess-commit").cursor, 5);
    commit(home.path(), "sess-commit", 3)
        .await
        .expect("commit 3");
    assert_eq!(load(home.path(), "sess-commit").cursor, 5);
    commit(home.path(), "sess-commit", 9)
        .await
        .expect("commit 9");
    assert_eq!(load(home.path(), "sess-commit").cursor, 9);
}

#[tokio::test(start_paused = true)]
async fn an_accepted_batch_advances_the_cursor_to_its_max_seq() {
    let (_home, cfg) = config("sess-accept");
    let script = Script::new(
        &cfg,
        vec![msg(1, "alpha"), msg(2, "beta"), msg(3, "gamma")],
        vec![],
    );
    let seen = run_until(&cfg, &script, vec![true], || cursor_of(&cfg) >= 3).await;
    assert_eq!(seen.len(), 1, "one batch, then nothing new");
    assert_eq!(
        (seen[0].max_seq, seen[0].count, seen[0].cursor_before),
        (3, 3, 0)
    );
    assert_eq!(cursor_of(&cfg), 3);
}

#[tokio::test(start_paused = true)]
async fn a_refused_batch_keeps_the_cursor_and_is_offered_again_after_the_backoff() {
    let (_home, cfg) = config("sess-refuse");
    let script = Script::new(&cfg, vec![msg(1, "alpha"), msg(2, "beta")], vec![]);
    let seen = run_until(&cfg, &script, vec![false, true], || cursor_of(&cfg) >= 2).await;
    assert_eq!(seen.len(), 2, "refused, then offered again");
    assert_eq!(seen[0].text, seen[1].text);
    assert_eq!(
        seen.iter().map(|s| s.cursor_before).collect::<Vec<_>>(),
        vec![0, 0],
        "the refusal left the cursor where it was"
    );
    assert!(seen[1].at.duration_since(seen[0].at) >= REFUSAL_FIRST);
    assert_eq!(cursor_of(&cfg), 2);
}

#[tokio::test(start_paused = true)]
async fn a_render_cut_advances_the_cursor_only_to_the_last_shown_message() {
    let (_home, mut cfg) = config("sess-cut");
    cfg.max_chars = 2_000;
    let body = "x".repeat(900);
    let msgs: Vec<Value> = (1..=20).map(|s| msg(s, &body)).collect();
    let script = Script::new(&cfg, msgs, vec![]);
    let seen = run_until(&cfg, &script, vec![true; 4], || cursor_of(&cfg) >= 20).await;
    assert!(seen.len() >= 2, "the cut batch, then the rest: {seen:?}");
    let first = &seen[0];
    assert_eq!(first.cursor_before, 0);
    assert!(first.count < 20, "the render cap cut the batch");
    assert_eq!(first.max_seq, i64::try_from(first.count).expect("fits"));
    assert_eq!(seen[1].cursor_before, first.max_seq);
    assert_eq!(seen.last().map(|s| s.max_seq), Some(20));
}

#[tokio::test(start_paused = true)]
async fn empty_rounds_are_paced_at_least_a_second_apart() {
    let (_home, cfg) = config("sess-empty");
    let script = Script::new(&cfg, vec![], vec![]);
    let seen = run_until(&cfg, &script, vec![], || script.fetch_count() >= 4).await;
    assert!(seen.is_empty());
    let times = script.fetches.lock().expect("fetch log").clone();
    assert!(times.len() >= 4, "several rounds: {}", times.len());
    for pair in times.windows(2) {
        assert!(pair[1].duration_since(pair[0]) >= Duration::from_secs(1));
    }
}

#[tokio::test(start_paused = true)]
async fn messages_a_hook_delivered_during_the_poll_are_not_offered() {
    let (_home, cfg) = config("sess-hook");
    let msgs = vec![
        msg(1, "alpha-one"),
        msg(2, "beta-two"),
        msg(3, "gamma-three"),
    ];
    let script = Script::new(&cfg, msgs, vec![Step::HookAdvance(2)]);
    let seen = run_until(&cfg, &script, vec![true], || cursor_of(&cfg) >= 3).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].count, seen[0].max_seq, seen[0].cursor_before),
        (1, 3, 2)
    );
    assert!(seen[0].text.contains("gamma-three"));
    assert!(!seen[0].text.contains("alpha-one"));
    assert!(!seen[0].text.contains("beta-two"));
}

#[tokio::test(start_paused = true)]
async fn the_daemon_is_ensured_at_start_and_after_each_failed_poll() {
    let (_home, cfg) = config("sess-ensure");
    let script = Script::new(&cfg, vec![msg(1, "alpha")], vec![Step::Fail, Step::Fail]);
    let seen = run_until(&cfg, &script, vec![true], || cursor_of(&cfg) >= 1).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        script.ensures.load(Ordering::SeqCst),
        3,
        "start + 2 failures"
    );
}

#[tokio::test(start_paused = true)]
async fn the_heartbeat_stops_once_the_inbox_has_been_unreachable_for_its_window() {
    let (_home, mut cfg) = config("sess-dead");
    cfg.wait_ms = 1_000;
    let script = Script::new(&cfg, vec![], (0..10).map(|_| Step::Fail).collect());
    let start = Instant::now();
    run_until(&cfg, &script, vec![], || {
        start.elapsed() >= Duration::from_secs(40)
    })
    .await;
    assert_eq!(
        script.beats.load(Ordering::SeqCst),
        1,
        "only the start beat; the 30 s tick found the inbox unreachable"
    );
}

#[tokio::test(start_paused = true)]
async fn the_heartbeat_keeps_beating_while_the_inbox_answers() {
    let (_home, mut cfg) = config("sess-alive");
    cfg.wait_ms = 1_000;
    let script = Script::new(&cfg, vec![], vec![]);
    let start = Instant::now();
    run_until(&cfg, &script, vec![], || {
        start.elapsed() >= Duration::from_secs(35)
    })
    .await;
    assert!(
        script.beats.load(Ordering::SeqCst) >= 2,
        "the start beat and the one at 30 s"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_fetch_backoff_ends_the_pump_cleanly() {
    let (_home, cfg) = config("sess-backoff");
    let script = Script::new(&cfg, vec![], (0..100).map(|_| Step::Fail).collect());
    let seen = run_until(&cfg, &script, vec![], || script.fetch_count() >= 3).await;
    assert!(seen.is_empty());
}

/// Two messages whose bodies are half the render cap each, so one batch shows exactly one of them.
fn one_message_per_batch(cfg: &mut PumpConfig) -> Vec<Value> {
    cfg.max_chars = 1_000;
    let body = "x".repeat(500);
    vec![
        msg(1, &format!("poison {body}")),
        msg(2, &format!("next {body}")),
    ]
}

#[tokio::test(start_paused = true)]
async fn a_batch_refused_up_to_the_cap_is_skipped_and_the_next_batch_flows() {
    let (_home, mut cfg) = config("sess-poison");
    let msgs = one_message_per_batch(&mut cfg);
    let script = Script::new(&cfg, msgs, vec![]);
    let seen = run_until(&cfg, &script, vec![false; 5], || cursor_of(&cfg) >= 2).await;
    let offered: Vec<i64> = seen.iter().map(|s| s.max_seq).collect();
    assert_eq!(
        offered,
        vec![1, 1, 1, 1, 1, 2],
        "five refusals of the poison batch, then the next batch"
    );
    assert_eq!(
        seen[5].cursor_before, 1,
        "the poison batch was skipped: the cursor moved past it"
    );
    assert!(seen[5].text.contains("next"));
    assert_eq!(cursor_of(&cfg), 2);
}

#[tokio::test(start_paused = true)]
async fn the_refusal_count_restarts_once_a_batch_is_accepted() {
    let (_home, mut cfg) = config("sess-reset-accept");
    let msgs = one_message_per_batch(&mut cfg);
    let script = Script::new(&cfg, msgs, vec![]);
    // Four refusals, an accept, then four refusals of the next batch: no skip, since the count
    // restarted at the accept.
    let verdicts = vec![
        false, false, false, false, true, false, false, false, false, true,
    ];
    let seen = run_until(&cfg, &script, verdicts, || cursor_of(&cfg) >= 2).await;
    let offered: Vec<i64> = seen.iter().map(|s| s.max_seq).collect();
    assert_eq!(offered, vec![1, 1, 1, 1, 1, 2, 2, 2, 2, 2]);
    assert_eq!(cursor_of(&cfg), 2);
}

#[tokio::test(start_paused = true)]
async fn the_refusal_count_restarts_when_the_offered_batch_changes() {
    let (_home, mut cfg) = config("sess-reset-batch");
    let msgs = one_message_per_batch(&mut cfg);
    // Four polls leave the first batch unchanged; the fifth hook-advances the cursor to 1, so the
    // batch offered next ends at seq 2.
    let steps = vec![
        Step::Quiet,
        Step::Quiet,
        Step::Quiet,
        Step::Quiet,
        Step::HookAdvance(1),
    ];
    let script = Script::new(&cfg, msgs, steps);
    // Four refusals of seq 1, then one of seq 2: a count carried over would make that one the
    // fifth and skip it. The seq 2 batch is accepted on its second offer.
    let verdicts = vec![false, false, false, false, false, true];
    let seen = run_until(&cfg, &script, verdicts, || cursor_of(&cfg) >= 2).await;
    let offered: Vec<i64> = seen.iter().map(|s| s.max_seq).collect();
    assert_eq!(offered, vec![1, 1, 1, 1, 2, 2]);
    assert_eq!(cursor_of(&cfg), 2);
}

#[tokio::test(start_paused = true)]
async fn the_heartbeat_keeps_beating_through_a_slow_driver_turn() {
    let (_home, mut cfg) = config("sess-busy");
    cfg.wait_ms = 1_000;
    let script = Script::new(&cfg, vec![msg(1, "alpha")], vec![]);
    let (tx, mut rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    // A driver that takes 100 s, past three beat periods, to answer.
    let slow_driver = async {
        let delivery: Delivery = rx.recv().await.expect("one delivery");
        tokio::time::sleep(Duration::from_secs(100)).await;
        let _ = delivery.reply.send(true);
    };
    let watch = async {
        for _ in 0..6_000 {
            if cursor_of(&cfg) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        shutdown.cancel();
    };
    let (res, (), ()) = tokio::join!(
        run_loop(&cfg, &script, tx, shutdown.clone()),
        slow_driver,
        watch
    );
    res.expect("the pump ends cleanly");
    assert_eq!(cursor_of(&cfg), 1, "the slow batch was accepted");
    assert!(
        script.beats.load(Ordering::SeqCst) >= 4,
        "the start beat and the ticks at 30, 60 and 90 s of the driver wait: {}",
        script.beats.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn run_without_an_identity_fails_fast() {
    let home = temp_dir();
    let folder = temp_dir();
    let cfg = PumpConfig::new(
        home.path().to_path_buf(),
        folder.path().to_path_buf(),
        "sess-missing".to_string(),
    );
    let (tx, _rx) = mpsc::channel(1);
    let res = tokio::time::timeout(
        Duration::from_secs(2),
        run(cfg, tx, CancellationToken::new()),
    )
    .await
    .expect("run returns within 2 s");
    let err = res.expect_err("no identity is an error");
    assert!(
        err.to_string().contains("agora whoami"),
        "the error names the fix: {err}"
    );
}

#[tokio::test]
async fn run_returns_ok_when_shutdown_is_already_cancelled() {
    let home = seeded_home();
    let folder = temp_dir();
    let cfg = PumpConfig::new(
        home.path().to_path_buf(),
        folder.path().to_path_buf(),
        "sess-cancelled".to_string(),
    );
    let (tx, _rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let res = tokio::time::timeout(Duration::from_secs(2), run(cfg, tx, shutdown))
        .await
        .expect("run returns within 2 s");
    assert!(res.is_ok());
}

#[test]
fn the_beat_is_filed_under_the_bridged_harness() {
    let home = seeded_home();
    let folder = temp_dir();
    let ctx = ctx_for(home.path(), folder.path()).expect("identity derived from the machine seed");
    let source = LiveDaemon {
        ctx,
        harness: Harness::Codex,
    };
    source.beat();
    let heartbeats = home.path().join("delivery").join("heartbeats");
    let keys: Vec<PathBuf> = std::fs::read_dir(&heartbeats)
        .expect("heartbeats directory")
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    assert_eq!(keys.len(), 1, "one identity directory: {keys:?}");
    let beat = keys[0]
        .join("codex")
        .join(Mechanism::OwnedSessionApi.as_str());
    assert!(beat.is_file(), "codex beat at {}", beat.display());
    assert!(
        !keys[0].join("claude").exists(),
        "a codex bridge must not beat under claude"
    );
}
