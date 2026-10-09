use super::*;
use agora_core::TransitionError;
use agora_crypto::Identity;

mod lifecycle;
mod notes_prune;

const NOW: i64 = 1_700_000_000_000;

/// A store with the system identity registered, and a worker key that is not the system.
fn setup() -> (Store, Identity, Identity) {
    let mut s = Store::open_in_memory().unwrap();
    let system = Identity::from_seed([9; 32]);
    let worker = Identity::from_seed([3; 32]);
    s.register_system_identity(NOW, &system.public()).unwrap();
    s.register_identity(NOW, &worker.public(), "worker")
        .unwrap();
    (s, system, worker)
}

/// Nonces are at most 128 bytes, so they name the op by kind, subject and time, not by its arg.
fn signed(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
    let nonce = format!("{kind}/{subject}/{now}");
    SignedOp::new(kind, subject, arg, now, &nonce).sign(id)
}

/// A claimed, requested, not-yet-started task, as the work queue leaves it.
fn insert_waiting(s: &Store, id: &str) {
    s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state, claimed, requested) \
             VALUES (?1, 'author', ?2, 'claimed', ?2, ?2)",
            params![id, NOW],
        )
        .unwrap();
}

fn start_arg() -> String {
    serde_json::to_string(&JobStartArgs {
        pid: 4242,
        supervisor_pid: 4241,
        supervisor_start_s: Some(1_700_000_000),
        qos: "utility".into(),
        exclusive: false,
        quiet_until_ms: None,
        log: "/logs/t.log".into(),
    })
    .unwrap()
}

fn finish_arg(status: Option<i32>, before_start: bool) -> String {
    serde_json::to_string(&JobFinishArgs {
        status,
        note: String::new(),
        cpu_seconds: Some(1.5),
        rss_peak_kib: Some(2048),
        ended_ms: NOW + 5_000,
        before_start,
    })
    .unwrap()
}

fn start(s: &mut Store, system: &Identity, id: &str) -> TaskRecord {
    let op = signed(system, "job.start", id, &start_arg(), NOW);
    s.job_start(NOW, &op).unwrap()
}

fn finish(
    s: &mut Store,
    system: &Identity,
    id: &str,
    status: Option<i32>,
    before_start: bool,
) -> Result<TaskRecord, StoreError> {
    let arg = finish_arg(status, before_start);
    let op = signed(system, "job.finish", id, &arg, NOW + 5_000);
    s.job_finish(NOW + 5_000, &op, None)
}

fn state_of(s: &Store, id: &str) -> String {
    s.conn
        .query_row("SELECT state FROM work_tasks WHERE id=?", [id], |r| {
            r.get(0)
        })
        .unwrap()
}

fn history_statuses(s: &Store, id: &str) -> Vec<Option<i64>> {
    let mut st = s
        .conn
        .prepare("SELECT status FROM job_history WHERE task=? ORDER BY seq")
        .unwrap();
    st.query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn event_count(s: &Store, kind: &str, subject: &str) -> i64 {
    s.conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind=? AND subject=?",
            params![kind, subject],
            |r| r.get(0),
        )
        .unwrap()
}
