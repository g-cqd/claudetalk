use super::*;

#[test]
fn work_scope_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_scope_set(NOW, &op(a, "work.scope", "", r#"{"id":"proj"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_add_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_add(NOW, &op(a, "work.add", "", r#"{"id":"m1","title":"mine"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_next_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.work_next(NOW, &op(a, "work.next", "", "{}")).map(|_| ())
    });
}

#[test]
fn work_claim_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "k1", "title": "k"}));
        s.work_claim(NOW, &op(a, "work.claim", "k1", ""))
            .map(|_| ())
    });
}

#[test]
fn work_accept_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        claimed_command(s, c, "a1");
        s.work_accept(
            NOW,
            &op(
                a,
                "work.accept",
                "a1",
                &json!({"digest": digest()}).to_string(),
            ),
        )
        .map(|_| ())
    });
}

#[test]
fn work_run_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        accepted_task(s, c, "a1");
        s.work_request(
            NOW,
            &op(a, "work.run", "a1", &json!({"task": "a1"}).to_string()),
            1.0,
            LOG_DIR,
        )
        .map(|_| ())
    });
}

#[test]
fn work_finish_follows_the_role_matrix() {
    assert_matrix([No, Yes, Owned, Owned, Owned, Owned], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "f1", "title": "f1"}));
        s.work_finish(NOW, &op(a, "work.finish", "f1", r#"{"state":"done"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_cancel_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "x1", "title": "x1"}));
        s.work_cancel(NOW, &op(a, "work.cancel", "x1", r#"{"note":"stop"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_update_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "u1", "title": "u1"}));
        s.work_update(NOW, &op(a, "work.update", "u1", r#"{"title":"renamed"}"#))
            .map(|_| ())
    });
}

#[test]
fn work_reclaim_follows_the_role_matrix() {
    assert_matrix([No, No, No, Yes, Yes, Yes], |s, c, a| {
        add_task(s, &c.alice, json!({"id": "t1", "title": "t1"}));
        claim_task(s, &c.alice, "t1");
        s.work_reclaim(
            NOW,
            &op(a, "work.reclaim", "task:t1", r#"{"note":"stale"}"#),
        )
    });
}

#[test]
fn work_settings_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        s.work_settings_set(
            NOW,
            &op(a, "work.settings", "", r#"{"patch":{"keep_days":"30"}}"#),
        )
        .map(|_| ())
    });
}

#[test]
fn timer_create_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, _c, a| {
        s.timer_create(NOW, &op(a, "timer.create", "", &timer_arg("tm1")))
            .map(|_| ())
    });
}

#[test]
fn timer_delete_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        s.timer_create(NOW, &op(&c.alice, "timer.create", "", &timer_arg("tm1")))
            .expect("setup: timer.create");
        s.timer_delete(NOW, &op(a, "timer.delete", "tm1", ""))
    });
}

#[test]
fn pool_define_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        s.pool_define(
            NOW,
            &op(a, "pool.define", "gpu", r#"{"name":"gpu","capacity":2}"#),
        )
        .map(|_| ())
    });
}

#[test]
fn lease_acquire_follows_the_role_matrix() {
    assert_matrix([No, Yes, Yes, Yes, Yes, Yes], |s, c, a| {
        pool(s, c);
        s.lease_acquire(
            NOW,
            &op(
                a,
                "lease.acquire",
                "",
                r#"{"kind":"pool","resource":"gpu","amount":1}"#,
            ),
        )
        .map(|_| ())
    });
}

#[test]
fn lease_renew_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, No, No, No], |s, c, a| {
        let lease = alice_lease(s, c);
        s.lease_renew(NOW, &op(a, "lease.renew", &lease, "{}"))
            .map(|_| ())
    });
}

#[test]
fn lease_release_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        let lease = alice_lease(s, c);
        s.lease_release(NOW, &op(a, "lease.release", &lease, ""))
    });
}

#[test]
fn quiet_open_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, _c, a| {
        let arg = json!({"expected_end_ms": NOW + 3_600_000}).to_string();
        s.quiet_open(NOW, &op(a, "quiet.open", "", &arg))
            .map(|_| ())
    });
}

#[test]
fn quiet_close_follows_the_role_matrix() {
    assert_matrix([No, No, No, No, Yes, Yes], |s, c, a| {
        let arg = json!({"expected_end_ms": NOW + 3_600_000}).to_string();
        let window = s
            .quiet_open(NOW, &op(&c.ctl, "quiet.open", "", &arg))
            .expect("setup: quiet.open");
        s.quiet_close(NOW, &op(a, "quiet.close", &window.id, ""))
    });
}

#[test]
fn policy_set_follows_the_role_matrix() {
    assert_matrix([No, No, No, Yes, Yes, Yes], |s, _c, a| {
        let arg = r#"{"team":"core","lane":"backend","policy":{"wip_limit":3}}"#;
        s.policy_set(NOW, &op(a, "policy.set", "core/backend", arg))
            .map(|_| ())
    });
}

#[test]
fn card_update_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        s.card_update(NOW, &op(a, "card.update", "c1", r#"{"value":2}"#))
            .map(|_| ())
    });
}

#[test]
fn card_dep_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        create_card(s, c, "c2");
        s.card_dep(NOW, &op(a, "card.dep", "c1", r#"{"after":"c2"}"#))
    });
}

#[test]
fn card_link_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        create_card(s, c, "c2");
        s.card_link(
            NOW,
            &op(a, "card.link", "c1", r#"{"rel":"relates","target":"c2"}"#),
        )
    });
}

#[test]
fn card_escalate_follows_the_role_matrix() {
    assert_matrix([No, Yes, No, Yes, Yes, Yes], |s, c, a| {
        reach(s, c, CardState::Claimed);
        s.card_escalate(
            NOW,
            &op(
                a,
                "card.escalate",
                "c1",
                r#"{"owner_action":"needs a decision"}"#,
            ),
            false,
        )
        .map(|_| ())
    });
}

// ---- replay and envelope ----
