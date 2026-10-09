use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cards_wait_wakes_when_the_claim_owner_moves_the_card_to_review() {
    let d = daemon().await;
    let mut w = world(&d).await;
    card_in_doing(&mut w, "c1").await;
    let mut alice = w.alice;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.bob,
        WaitSpec::Cards {
            ids: vec!["c1".to_string()],
            states: vec!["review".to_string()],
        },
        WAIT_MS,
        None,
    );
    let mover = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        let op = alice.op("card.move", "c1", "review");
        call(&mut alice, Req::CardMove { op })
            .await
            .expect("card moves to review");
    });

    let (_bob, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    join(mover).await;
    let result = result.expect("the cards wait is accepted");

    assert_eq!(result.condition, "cards");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec!["c1".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_messages_wait_wakes_when_a_message_is_posted_to_its_channel() {
    let d = daemon().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let mut alice = w.alice;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.bob,
        WaitSpec::Messages {
            channel: "g".to_string(),
            after: 0,
        },
        WAIT_MS,
        None,
    );
    let poster = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        alice.post("g", "wake up").await
    });

    join(poster).await;
    let (_bob, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    let result = result.expect("the messages wait is accepted");

    assert_eq!(result.condition, "messages");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    assert_eq!(result.targets, vec!["g".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_messages_wait_does_not_return_for_a_message_at_or_before_its_after_seq() {
    let d = daemon().await;
    let mut w = world(&d).await;
    w.owner.create_group("g", &[&w.alice, &w.bob]).await;
    let seq = w.alice.post("g", "already read").await;

    let result = wait_on(
        &mut w.bob,
        WaitSpec::Messages {
            channel: "g".to_string(),
            after: seq,
        },
        300,
        None,
    )
    .await;

    assert!(result.timed_out);
    assert_eq!(result.status, 124);
    assert_eq!(result.pending, vec!["g".to_string()]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notices_wait_from_now_does_not_return_a_notice_posted_before_it_started() {
    let d = daemon().await;
    let mut w = world(&d).await;
    create_card(&mut w.owner, "n1").await;
    let alice_pk = w.alice.pk_hex();
    let notified = escalate(&mut w.owner, "n1", "first decision", vec![alice_pk]).await;
    assert_eq!(notified.len(), 1, "the escalation notified alice");

    let result = wait_on(&mut w.alice, WaitSpec::Notices { after: -1 }, 300, None).await;

    assert_eq!(result.condition, "notices");
    assert!(
        result.timed_out,
        "the notice posted before the wait is not new"
    );
    assert_eq!(result.status, 124);
    assert!(result.cursor.is_some());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_notices_wait_returns_on_a_notice_posted_while_it_waits() {
    let d = daemon().await;
    let mut w = world(&d).await;
    create_card(&mut w.owner, "n1").await;
    let alice_pk = w.alice.pk_hex();
    escalate(&mut w.owner, "n1", "first decision", vec![alice_pk.clone()]).await;
    let first = wait_on(&mut w.alice, WaitSpec::Notices { after: -1 }, 300, None).await;
    assert!(first.timed_out);

    let mut owner = w.owner;
    let started = Instant::now();
    let waiter = spawn_wait(
        w.alice,
        WaitSpec::Notices { after: -1 },
        WAIT_MS,
        first.cursor,
    );
    let escalator = tokio::spawn(async move {
        tokio::time::sleep(LATER).await;
        escalate(&mut owner, "n1", "second decision", vec![alice_pk]).await;
    });

    let (_alice, result) = join(waiter).await;
    assert_waited_for_the_change(started);
    join(escalator).await;
    let result = result.expect("the resumed notices wait is accepted");

    assert_eq!(result.condition, "notices");
    assert!(!result.timed_out);
    assert_eq!(result.status, 0);
    d.stop().await;
}
