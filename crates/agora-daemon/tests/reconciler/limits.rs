use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_above_its_wip_limit_notifies_each_supervisor() {
    let mut r = rig().await;
    grant_supervisor(&mut r.w.owner, &r.w.carol).await;
    doing_card(&mut r.w, "c1", "dev").await;
    doing_card(&mut r.w, "c2", "dev").await;
    // The limit goes in after both cards are in progress: a claim into a lane over its limit is refused.
    let op = r.w.carol.op(
        "policy.set",
        "dev/l",
        &json!({"team": "dev", "lane": "l", "policy": {"wip_limit": 1}}).to_string(),
    );
    r.w.carol.ok(Req::PolicySet { op }).await;

    r.clock.pass_at(r.start + 30 * SECOND).await;

    assert_eq!(
        notices(&mut r.w.carol).await,
        vec![notice(
            "wip",
            "dev/l",
            "lane dev/l has 2 cards in progress, above its WIP limit of 1",
        )],
        "the supervisor is told once that the lane is over its limit"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_active_sprint_past_its_end_notifies_the_supervisors() {
    let mut r = rig().await;
    grant_supervisor(&mut r.w.owner, &r.w.carol).await;
    let sprint_arg = json!({
        "name": "S1",
        "goal": "g",
        "starts": r.start,
        "ends": r.start + 10 * MINUTE,
    });
    let op = r.w.carol.op("sprint.create", "s1", &sprint_arg.to_string());
    r.w.carol.ok(Req::SprintCreate { op }).await;
    let op = r.w.carol.op("sprint.start", "s1", "");
    r.w.carol.ok(Req::SprintStart { op }).await;

    r.clock.pass_at(r.start + 60 * MINUTE).await;

    assert_eq!(
        notices(&mut r.w.carol).await,
        vec![notice(
            "sprint_end",
            "s1",
            "sprint S1 ended 50 min 0 s ago with 0 open cards; close it and roll them over",
        )],
    );
    r.d.stop().await;
}
