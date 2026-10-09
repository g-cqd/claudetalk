use super::*;

/// D7: the state a job outcome moves a card to from `from`, or None when the edge is not a sync edge.
fn d7_target(outcome: JobOutcome, from: CardState) -> Option<CardState> {
    match (outcome, from) {
        (JobOutcome::Done, CardState::Claimed | CardState::Doing) => Some(CardState::Review),
        (JobOutcome::Failed, CardState::Claimed | CardState::Doing | CardState::Review) => {
            Some(CardState::Blocked)
        }
        (JobOutcome::Cancelled, CardState::Claimed | CardState::Doing | CardState::Review) => {
            Some(CardState::Ready)
        }
        _ => None,
    }
}

const ALL_STATES: [CardState; 8] = [
    CardState::Backlog,
    CardState::Ready,
    CardState::Claimed,
    CardState::Doing,
    CardState::Review,
    CardState::Blocked,
    CardState::Done,
    CardState::Cancelled,
];

/// Ends job `job` with `outcome` as the daemon would: a cancelled job is flagged first, and the finish carries
/// the card sync signed by the system in the same call.
fn finish_job(s: &mut Store, c: &Cast, job: &str, outcome: JobOutcome) -> Result<(), StoreError> {
    let status = match outcome {
        JobOutcome::Done => Some(0),
        JobOutcome::Failed => Some(1),
        JobOutcome::Cancelled => Some(143),
    };
    if outcome == JobOutcome::Cancelled {
        s.work_cancel(NOW, &op(&c.sup, "work.cancel", job, r#"{"note":"stop"}"#))
            .expect("setup: cancel a running job");
    }
    let sync = op(
        &c.sys,
        "card.job_sync",
        job,
        &sync_arg(job, outcome, status),
    );
    s.job_finish(
        NOW,
        &op(&c.sys, "job.finish", job, &finish_arg(status, "boom")),
        Some(&sync),
    )
    .map(|_| ())
}

/// For every card state, a job that ends with `outcome` must leave the card where D7 says. An illegal edge
/// leaves the card unchanged and tells the owner (or the supervisors, when unowned) why.
fn assert_job_sync(outcome: JobOutcome) {
    let mut failures = Vec::new();
    for from in ALL_STATES {
        let (mut s, c) = fixture();
        reach(&mut s, &c, from);
        let before = s.list_cards(None, None, 100).unwrap();
        let owner_before = before
            .iter()
            .find(|r| r.id == "c1")
            .and_then(|r| r.owner.clone());
        start_job(&mut s, &c, "j1", Some("c1"));
        finish_job(&mut s, &c, "j1", outcome).expect("the job finish with its sync");

        let target = d7_target(outcome, from);
        let expected = target.unwrap_or(from);
        let landed = s.card_state("c1").unwrap();
        let mut problems = Vec::new();
        if landed != expected {
            problems.push(format!("card is {landed:?}, D7 says {expected:?}"));
        }
        let notify = owner_before.unwrap_or_else(|| pk_hex(&c.sup));
        let heard = s
            .notices(&notify, 0, 100)
            .unwrap()
            .iter()
            .any(|n| n.subject == "c1");
        if target.is_none() && !heard {
            problems.push(format!("no notice to {notify} for the skipped sync"));
        }
        if target.is_some() && heard {
            problems.push("a skip notice was sent for a legal sync".to_string());
        }
        let needs_owner = card_ext_of(&s, "c1").needs_owner;
        let flagged = outcome == JobOutcome::Failed && target.is_some();
        if needs_owner != flagged {
            problems.push(format!("needs_owner is {needs_owner}, expected {flagged}"));
        }
        if outcome == JobOutcome::Cancelled && target.is_some() {
            let owner_now = s
                .list_cards(None, None, 100)
                .unwrap()
                .into_iter()
                .find(|r| r.id == "c1")
                .and_then(|r| r.owner);
            if owner_now.is_some() {
                problems.push("a cancelled sync did not clear the owner".to_string());
            }
        }
        if !problems.is_empty() {
            failures.push(format!("from {from:?}: {}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn job_sync_done_moves_only_claimed_or_doing_cards_to_review() {
    assert_job_sync(JobOutcome::Done);
}

#[test]
fn job_sync_failed_moves_only_claimed_doing_or_review_cards_to_blocked() {
    assert_job_sync(JobOutcome::Failed);
}

#[test]
fn job_sync_cancelled_moves_only_claimed_doing_or_review_cards_to_ready() {
    assert_job_sync(JobOutcome::Cancelled);
}
