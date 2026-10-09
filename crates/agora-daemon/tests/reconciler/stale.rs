use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_claimed_card_notifies_its_owner_at_thirty_minutes() {
    let mut r = stale_rig().await;
    passes_at_minutes(&r, &[31]).await;

    assert_eq!(
        notices(&mut r.w.alice).await,
        vec![notice("stale", "c1", STALE_31)],
        "the owner gets one stale notice once the card is idle past 30 min"
    );
    assert!(
        notices(&mut r.w.carol).await.is_empty(),
        "the supervisors hear nothing before the escalation threshold"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_later_pass_does_not_repeat_a_stale_notice() {
    let mut r = stale_rig().await;
    passes_at_minutes(&r, &[31, 32]).await;

    assert_eq!(
        notices(&mut r.w.alice).await,
        vec![notice("stale", "c1", STALE_31)],
        "the stale notice is keyed by the card's last update, so a second pass sends no copy"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_card_is_escalated_to_the_supervisors_at_forty_five_minutes() {
    let mut r = stale_rig().await;
    passes_at_minutes(&r, &[31, 46]).await;

    assert_eq!(
        notices(&mut r.w.carol).await,
        vec![notice("escalation", "c1", &escalation_46(&r.w.alice))],
        "with no escalation chain, every supervisor is told at 45 min idle"
    );
    assert_eq!(
        notices(&mut r.w.alice).await,
        vec![notice("stale", "c1", STALE_31)],
        "the owner keeps only the first notice; escalation goes to the supervisors"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_card_is_reclaimed_to_ready_at_sixty_minutes_with_a_notice() {
    let mut r = stale_rig().await;
    passes_at_minutes(&r, &[31, 46, 61]).await;

    assert_eq!(
        notices(&mut r.w.alice).await,
        vec![
            notice("stale", "c1", STALE_31),
            notice("reclaimed", "c1", RECLAIMED_61),
        ],
        "the owner is told the card was reclaimed"
    );
    let ready = cards_in(&mut r.w.owner, "ready").await;
    assert_eq!(ready.len(), 1, "the reclaimed card is back in ready");
    assert_eq!(ready[0].id, "c1");
    assert_eq!(ready[0].owner, None, "the reclaim clears the owner");
    assert!(
        cards_in(&mut r.w.owner, "doing").await.is_empty(),
        "nothing is left in doing"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_after_the_reclaim_applies_nothing_new() {
    let mut r = stale_rig().await;
    passes_at_minutes(&r, &[31, 46, 61]).await;
    let owner_before = notices(&mut r.w.alice).await;
    let carol_before = notices(&mut r.w.carol).await;
    let ready_before = cards_in(&mut r.w.owner, "ready").await;

    passes_at_minutes(&r, &[62]).await;

    assert_eq!(notices(&mut r.w.alice).await, owner_before);
    assert_eq!(notices(&mut r.w.carol).await, carol_before);
    assert_eq!(cards_in(&mut r.w.owner, "ready").await, ready_before);
    r.d.stop().await;
}
