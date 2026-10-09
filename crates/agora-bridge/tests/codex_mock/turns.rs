//! Turn tracking and setup of the Codex driver, against the scripted server in `codex_mock`.

use super::*;

/// Delivers `text`, checks the request `method` (`turn/start` or `turn/steer`) carries it, answers
/// it, and checks the delivery was accepted. Returns the request params.
async fn deliver_expecting(d: &mut Driver, method: &str, text: &str) -> Value {
    let answer = deliver(d, text).await;
    let (id, params) = d.server.expect_request(method).await;
    assert_eq!(params["input"], text_input(text));
    d.server
        .reply(&id, json!({"turnId": "turn-x", "turn": {"id": "turn-x"}}))
        .await;
    assert!(accepted(answer).await);
    params
}

#[tokio::test]
async fn initial_prompt_is_sent_as_turn_start_after_start() {
    let mut opts = options();
    opts.prompt = Some("begin".into());
    let mut d = start(opts);
    handshake(&mut d.server).await;
    let (id, _) = d.server.expect_request("thread/start").await;
    d.server.reply(&id, json!({"thread": {"id": THREAD}})).await;
    let (id, params) = d.server.expect_request("turn/start").await;
    assert_eq!(params["threadId"], THREAD);
    assert_eq!(params["input"], text_input("begin"));
    d.server
        .reply(
            &id,
            json!({"turn": {"id": "turn-1", "status": "inProgress"}}),
        )
        .await;
    assert_eq!(ready_id(&mut d).await, THREAD);
}

#[tokio::test]
async fn steer_that_times_out_replies_false() {
    let mut opts = options();
    opts.turn_timeout = Duration::from_millis(100);
    let mut d = start(opts);
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    barrier(&mut d.server).await;
    let answer = deliver(&d, "slow").await;
    let (_, params) = d.server.expect_request("turn/steer").await;
    assert_eq!(params["expectedTurnId"], "turn-1");
    assert!(!accepted(answer).await);
    barrier(&mut d.server).await;
}

#[tokio::test]
async fn idle_status_clears_the_active_turn() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    let idle = json!({"threadId": THREAD, "status": {"type": "idle"}});
    d.server
        .send(notification("thread/status/changed", idle))
        .await;
    barrier(&mut d.server).await;
    deliver_expecting(&mut d, "turn/start", "after idle").await;
}

#[tokio::test]
async fn completion_of_another_turn_keeps_the_active_one() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    d.server.send(turn_completed("turn-0")).await;
    barrier(&mut d.server).await;
    let params = deliver_expecting(&mut d, "turn/steer", "more").await;
    assert_eq!(params["expectedTurnId"], "turn-1");
}

#[tokio::test]
async fn notifications_for_another_thread_are_ignored() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    d.server.send(turn_started("turn-1")).await;
    let other = "thr-other";
    let completed = json!({"threadId": other, "turn": {"id": "turn-1", "status": "completed"}});
    let idle = json!({"threadId": other, "status": {"type": "idle"}});
    let started = json!({"threadId": other, "turn": {"id": "turn-9", "status": "inProgress"}});
    d.server
        .send(notification("turn/completed", completed))
        .await;
    d.server
        .send(notification("thread/status/changed", idle))
        .await;
    d.server.send(notification("turn/started", started)).await;
    barrier(&mut d.server).await;
    let params = deliver_expecting(&mut d, "turn/steer", "still mine").await;
    assert_eq!(params["expectedTurnId"], "turn-1");
}

#[tokio::test]
async fn permission_and_user_input_requests_get_empty_answers() {
    let mut d = start(options());
    open(&mut d, "thread/start").await;
    let reply = ask(
        &mut d.server,
        7,
        "item/permissions/requestApproval",
        json!({}),
    )
    .await;
    assert_eq!(reply["result"], json!({"permissions": {}}));
    let reply = ask(&mut d.server, 8, "item/tool/requestUserInput", json!({})).await;
    assert_eq!(reply["result"], json!({"answers": {}}));
}
