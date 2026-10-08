//! Unit tests for the D7 job sync: the done, failed and cancelled edges, the skip notice,
//! the refusal of a non-system actor, the author check (R3-2) and the job match (R3-4).

use super::testkit::*;
use super::*;
use crate::hex;
use agora_crypto::Identity;
use agora_sched::model::TaskState;
use rusqlite::params;

/// A task `id` linked to `card` and authored by `author` (testkit's `add_task` authors it as "x").
fn add_task_by(c: &Cast, author: &Identity, id: &str, card: Option<&str>, state: &str) {
    c.s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state, card) VALUES (?,?,?,?,?)",
            params![id, hex(&author.public()), 0, state, card],
        )
        .unwrap();
}

/// Runs `card_job_sync_tx` for job `task` ending in `final_state`, as the system identity,
/// committing only on success.
fn sync_job(
    c: &mut Cast,
    task: &str,
    final_state: TaskState,
    arg: serde_json::Value,
) -> Result<(), StoreError> {
    let op = sign(&c.sys, "card.job_sync", task, &arg.to_string(), NOW);
    let tx = c.s.conn.transaction().unwrap();
    let res = card_job_sync_tx(&tx, NOW, &op, task, final_state);
    if res.is_ok() {
        tx.commit().unwrap();
    }
    res
}

/// `sync_job` for the job the sync names, in the state its outcome corresponds to.
fn sync(c: &mut Cast, arg: serde_json::Value) -> Result<(), StoreError> {
    let task = arg["task"].as_str().unwrap_or_default().to_owned();
    let state = match arg["outcome"].as_str() {
        Some("failed") => TaskState::Failed,
        Some("cancelled") => TaskState::Cancelled,
        _ => TaskState::Done,
    };
    sync_job(c, &task, state, arg)
}

#[test]
fn job_sync_done_moves_a_doing_card_to_review_with_evidence() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    let done = serde_json::json!({"task": "t1", "outcome": "done", "status": 0, "log": "/logs/t1.log", "note": ""});
    sync(&mut c, done).unwrap();
    assert_eq!(state_of(&c, "a"), "review");
    assert_eq!(ext(&c, "a").jobs, ["t1"]);
    let (reference, by): (String, String) =
        c.s.conn
            .query_row("SELECT ref, by FROM evidence WHERE card='a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
    assert_eq!(reference, "job:t1 status=0 log=/logs/t1.log");
    assert_eq!(by, hex(&c.sys.public()));
    let actor: String = c
        .s
        .conn
        .query_row(
            "SELECT actor FROM events WHERE kind='card.moved' AND subject='a' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, hex(&c.sys.public()));
}

#[test]
fn job_sync_done_from_claimed_passes_through_doing() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    sync(
        &mut c,
        serde_json::json!({"task": "t1", "outcome": "done", "status": 0, "log": null, "note": ""}),
    )
    .unwrap();
    assert_eq!(state_of(&c, "a"), "review");
    let mut st =
        c.s.conn
            .prepare("SELECT data FROM events WHERE kind='card.moved' AND subject='a' ORDER BY seq")
            .unwrap();
    let moves: Vec<String> = st
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let last_two: Vec<&str> = moves[moves.len() - 2..]
        .iter()
        .map(String::as_str)
        .collect();
    assert!(last_two[0].contains(r#""to":"doing""#), "{last_two:?}");
    assert!(last_two[1].contains(r#""to":"review""#), "{last_two:?}");
}

#[test]
fn job_sync_failed_blocks_the_card_and_asks_its_owner() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    let failed = serde_json::json!({"task": "t1", "outcome": "failed", "status": 3, "log": "/logs/t1.log", "note": "boom"});
    sync(&mut c, failed).unwrap();
    assert_eq!(state_of(&c, "a"), "blocked");
    let e = ext(&c, "a");
    assert!(e.needs_owner);
    assert_eq!(
        e.owner_action,
        "job t1 failed with status 3: boom; inspect /logs/t1.log and re-run or cancel"
    );
    let owner: Option<String> =
        c.s.conn
            .query_row("SELECT owner FROM cards WHERE id='a'", [], |r| r.get(0))
            .unwrap();
    assert_eq!(owner, Some(hex(&c.alice.public())));
    let refs: i64 = c
        .s
        .conn
        .query_row("SELECT COUNT(*) FROM evidence WHERE card='a' AND ref='job:t1 status=3 log=/logs/t1.log'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(refs, 1);
}

#[test]
fn job_sync_cancelled_returns_the_card_to_ready_without_an_owner() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "cancelled");
    sync(&mut c, serde_json::json!({"task": "t1", "outcome": "cancelled", "status": 143, "log": null, "note": ""})).unwrap();
    assert_eq!(state_of(&c, "a"), "ready");
    let owner: Option<String> =
        c.s.conn
            .query_row("SELECT owner FROM cards WHERE id='a'", [], |r| r.get(0))
            .unwrap();
    assert_eq!(owner, None);
    let evidence: i64 =
        c.s.conn
            .query_row("SELECT COUNT(*) FROM evidence WHERE card='a'", [], |r| {
                r.get(0)
            })
            .unwrap();
    assert_eq!(evidence, 0);
}

#[test]
fn job_sync_skips_an_illegal_state_and_notifies_the_owner() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "blocked").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    sync(
        &mut c,
        serde_json::json!({"task": "t1", "outcome": "done", "status": 0, "log": null, "note": ""}),
    )
    .unwrap();
    assert_eq!(state_of(&c, "a"), "blocked");
    let notes = notices_for(&c, "a");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].0, hex(&c.alice.public()));
    assert_eq!(notes[0].1, "job_sync");
    assert_eq!(
        notes[0].2,
        "job t1 ended done but card a (a) is blocked, so the card was not moved"
    );
}

#[test]
fn job_sync_skip_on_an_unowned_card_notifies_the_supervisors() {
    let mut c = cast();
    make(&mut c, "a");
    add_task_by(&c, &c.alice, "t1", Some("a"), "cancelled");
    sync(&mut c, serde_json::json!({"task": "t1", "outcome": "cancelled", "status": null, "log": null, "note": ""})).unwrap();
    assert_eq!(state_of(&c, "a"), "backlog");
    assert_eq!(notices_for(&c, "a").len(), 2);
}

#[test]
fn job_sync_skips_when_the_owner_changed_since_the_task_was_added() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    // Alice gives the card up and bob claims it: the card is bob's, and alice no longer owns it.
    mv(&mut c.s, &c.alice, "a", "ready").unwrap();
    mv(&mut c.s, &c.bob, "a", "claimed").unwrap();
    sync(
        &mut c,
        serde_json::json!({"task": "t1", "outcome": "done", "status": 0, "log": null, "note": ""}),
    )
    .unwrap();
    assert_eq!(state_of(&c, "a"), "claimed");
    let notes = notices_for(&c, "a");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].0, hex(&c.bob.public()));
    assert_eq!(
        notes[0].2,
        "job t1 ended done but card a (a) is claimed, so the card was not moved"
    );
}

#[test]
fn job_sync_refuses_a_sync_that_names_another_job() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    let arg =
        serde_json::json!({"task": "t2", "outcome": "done", "status": 0, "log": null, "note": ""});
    let err = sync_job(&mut c, "t1", TaskState::Done, arg);
    assert!(matches!(
        err,
        Err(StoreError::Invalid(
            "card.job_sync does not match the job it finishes"
        ))
    ));
    assert_eq!(state_of(&c, "a"), "doing");
}

#[test]
fn job_sync_refuses_an_outcome_that_does_not_match_the_final_state() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    mv(&mut c.s, &c.alice, "a", "doing").unwrap();
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    // The job finished as failed, but the sync says done.
    let arg =
        serde_json::json!({"task": "t1", "outcome": "done", "status": 0, "log": null, "note": ""});
    let err = sync_job(&mut c, "t1", TaskState::Failed, arg);
    assert!(matches!(
        err,
        Err(StoreError::Invalid(
            "card.job_sync does not match the job it finishes"
        ))
    ));
    assert_eq!(state_of(&c, "a"), "doing");
}

#[test]
fn job_sync_refuses_a_non_system_actor_and_an_unknown_task() {
    let mut c = cast();
    claimed_by_alice(&mut c, "a");
    add_task_by(&c, &c.alice, "t1", Some("a"), "running");
    let arg = r#"{"task":"t1","outcome":"done","status":0,"log":null,"note":""}"#;
    let by_alice = sign(&c.alice, "card.job_sync", "t1", arg, NOW);
    let tx = c.s.conn.transaction().unwrap();
    assert!(card_job_sync_tx(&tx, NOW, &by_alice, "t1", TaskState::Done).is_err());
    drop(tx);
    assert_eq!(state_of(&c, "a"), "claimed");
    let err = sync(
        &mut c,
        serde_json::json!({"task": "nope", "outcome": "done", "status": 0, "log": null, "note": ""}),
    );
    assert!(matches!(err, Err(StoreError::NotFound(ref t)) if t == "task nope"));
}
