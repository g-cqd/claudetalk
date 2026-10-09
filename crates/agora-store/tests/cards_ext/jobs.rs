use super::*;

#[test]
fn a_done_job_moves_its_doing_card_to_review_and_records_the_job_as_evidence() {
    let db = TempDb::new("done");
    let (mut s, c) = on_disk(&db.0);
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Review);
    assert_eq!(
        evidence_refs(&db.0, "c1"),
        vec!["job:t1 status=0 log=/var/agora/logs/t1.log".to_string()]
    );
}

/// The evidence references of a card, read from the store's database: the API has no reader for them.
fn evidence_refs(path: &Path, card: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("open database");
    let mut st = conn
        .prepare("SELECT ref FROM evidence WHERE card=? ORDER BY ref")
        .expect("prepare");
    st.query_map([card], |r| r.get(0))
        .expect("query")
        .collect::<Result<Vec<String>, _>>()
        .expect("rows")
}

#[test]
fn a_done_job_on_a_claimed_card_passes_through_doing_to_review() {
    let (mut s, c) = in_memory();
    new_card(&mut s, &c.sup, "c1", json!({}));
    claim_card(&mut s, &c.sup, &c.alice, "c1");
    queue_task(&mut s, &c.alice, "t1", Some("c1"));
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Review);
}

#[test]
fn a_failed_job_blocks_its_card_and_gives_the_owner_an_action() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Failed, Some(1), "boom");
    finish_with_sync(&mut s, &c.sys, "t1", Some(1), &sync).expect("finish");
    assert_eq!(s.card_state("c1").expect("state"), CardState::Blocked);
    let ext = ext_of(&s, "c1");
    assert!(ext.needs_owner);
    assert_eq!(
        ext.owner_action,
        "job t1 failed with status 1: boom; inspect /var/agora/logs/t1.log and re-run or cancel"
    );
}

#[test]
fn a_cancelled_job_returns_its_card_to_ready_without_an_owner() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    s.work_cancel(
        NOW,
        &op(&c.carol, "work.cancel", "t1", r#"{"note":"stop"}"#),
    )
    .expect("cancel running job");
    let sync = sync_args("t1", JobOutcome::Cancelled, Some(143), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(143), &sync).expect("finish");
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Cancelled);
    assert_eq!(owner_of(&s, "c1", CardState::Ready), None);
}

#[test]
fn a_job_ending_after_its_card_changed_owner_leaves_the_card_and_tells_the_new_owner() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    s.reclaim_stale(NOW, &op(&c.sup, "card.reclaim", "c1", ""))
        .expect("reclaim");
    move_card(&mut s, &c.bob, "c1", "claimed");
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Done, Some(0), "");
    finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync).expect("finish");
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Done);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Claimed);
    assert_eq!(owner_of(&s, "c1", CardState::Claimed), Some(pk_hex(&c.bob)));
    let notice = only_notice(&s, &c.bob);
    assert_eq!(notice.kind, "job_sync");
    assert_eq!(notice.subject, "c1");
    assert!(
        notice.text.contains("job t1 ended done") && notice.text.contains("was not moved"),
        "{}",
        notice.text
    );
}

#[test]
fn a_sync_naming_another_job_refuses_the_whole_finish_and_the_job_keeps_running() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t9", JobOutcome::Done, Some(0), "");
    assert_eq!(
        failure(finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync)),
        "card.job_sync does not match the job it finishes"
    );
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Running);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}

#[test]
fn a_sync_whose_outcome_differs_from_the_final_state_refuses_the_whole_finish() {
    let (mut s, c) = in_memory();
    doing_card_with_task(&mut s, &c);
    start_as_carol(&mut s, &c, "t1");
    let sync = sync_args("t1", JobOutcome::Failed, Some(0), "");
    assert_eq!(
        failure(finish_with_sync(&mut s, &c.sys, "t1", Some(0), &sync)),
        "card.job_sync does not match the job it finishes"
    );
    assert_eq!(s.work_task("t1").expect("task").state, TaskState::Running);
    assert_eq!(s.card_state("c1").expect("state"), CardState::Doing);
}
