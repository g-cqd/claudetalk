use super::*;

#[test]
fn replaying_a_signed_operation_is_refused() {
    let (mut s, c) = fixture();
    let signed = op(&c.alice, "work.add", "", r#"{"id":"r1","title":"once"}"#);
    s.work_add(NOW, &signed).unwrap();
    assert!(matches!(
        s.work_add(NOW, &signed),
        Err(StoreError::Invalid("replayed operation"))
    ));
}

#[test]
fn an_operation_stamped_over_five_minutes_from_the_clock_is_refused() {
    let (mut s, c) = fixture();
    let arg = r#"{"id":"s1","title":"skew"}"#;
    let late = op_at(&c.alice, "work.add", "", arg, NOW + FIVE_MINUTES_MS + 1);
    let early = op_at(&c.alice, "work.add", "", arg, NOW - FIVE_MINUTES_MS - 1);
    assert!(matches!(
        s.work_add(NOW, &late),
        Err(StoreError::Invalid("timestamp outside allowed clock skew"))
    ));
    assert!(matches!(
        s.work_add(NOW, &early),
        Err(StoreError::Invalid("timestamp outside allowed clock skew"))
    ));
}

#[test]
fn an_operation_stamped_exactly_five_minutes_away_is_accepted() {
    let (mut s, c) = fixture();
    let late = op_at(
        &c.alice,
        "work.add",
        "",
        r#"{"id":"s2","title":"late"}"#,
        NOW + FIVE_MINUTES_MS,
    );
    let early = op_at(
        &c.alice,
        "work.add",
        "",
        r#"{"id":"s3","title":"early"}"#,
        NOW - FIVE_MINUTES_MS,
    );
    s.work_add(NOW, &late).unwrap();
    s.work_add(NOW, &early).unwrap();
}

#[test]
fn an_operation_signed_for_another_kind_is_refused_by_the_method() {
    let (mut s, c) = fixture();
    let signed = op(&c.alice, "work.add", "", r#"{"id":"k2","title":"kind"}"#);
    assert!(matches!(
        s.work_claim(NOW, &signed),
        Err(StoreError::Invalid("wrong operation kind"))
    ));
}

#[test]
fn a_revoked_identity_cannot_sign_work_operations() {
    let (mut s, c) = fixture();
    s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.alice), ""))
        .unwrap();
    assert!(matches!(
        s.work_add(
            NOW,
            &op(&c.alice, "work.add", "", r#"{"id":"v1","title":"v"}"#)
        ),
        Err(StoreError::Invalid("identity revoked"))
    ));
}

#[test]
fn an_unregistered_identity_cannot_sign_work_operations() {
    let (mut s, c) = fixture();
    assert!(matches!(
        s.work_add(
            NOW,
            &op(&c.eve, "work.add", "", r#"{"id":"e1","title":"e"}"#)
        ),
        Err(StoreError::Invalid("unknown identity"))
    ));
}

// ---- system identity ----

#[test]
fn system_identity_starts_notes_and_finishes_a_job() {
    let (mut s, c) = fixture();
    start_job(&mut s, &c, "j1", None);
    s.job_note(NOW, &op(&c.sys, "job.note", "j1", r#"{"note":"halfway"}"#))
        .unwrap();
    let done = s
        .job_finish(
            NOW,
            &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        )
        .unwrap();
    assert_eq!(done.state, TaskState::Done);
}

#[test]
fn system_identity_fires_a_timer_into_a_task_its_owner_authored() {
    let (mut s, c) = fixture();
    let created = s
        .timer_create(NOW, &op(&c.alice, "timer.create", "", &timer_arg("tm1")))
        .unwrap();
    let due = created.next_fire_ms.expect("the timer has a next fire");
    let fire = json!({"fire_ms": due}).to_string();
    let task = s
        .timer_fire(due, &op(&c.sys, "timer.fire", "tm1", &fire))
        .unwrap()
        .expect("the timer fires a task");
    assert_eq!(task.id, "tm1-1");
    assert_eq!(task.author, pk_hex(&c.alice));
}

#[test]
fn system_identity_applies_each_reconcile_action_kind() {
    let (mut s, c) = fixture();
    reach(&mut s, &c, CardState::Claimed);
    let alice = pk_hex(&c.alice);
    let actions = [
        json!({"action": "expire_lease", "key": "expire:l1", "lease": "l1"}),
        json!({"action": "close_quiet", "key": "quiet:q1", "lease": "q1"}),
        json!({"action": "notice", "key": "notice:1", "target": alice, "kind": "stale", "subject": "c1", "text": "c1 is stale"}),
        json!({"action": "reclaim_card", "key": "reclaim:c1", "card": "c1", "owner": alice}),
        json!({"action": "blocker", "key": "blocker:disk", "open": true, "title": "disk low", "owner_action": "free disk"}),
        json!({"action": "admission", "key": "admission:pause", "paused": "disk low"}),
    ];
    for action in actions {
        let kind = match action["action"].as_str().unwrap() {
            "expire_lease" => "reconcile.expire",
            "close_quiet" => "reconcile.quiet_close",
            "notice" => "reconcile.notice",
            "reclaim_card" => "reconcile.reclaim",
            "blocker" => "reconcile.blocker",
            _ => "reconcile.admission",
        };
        let got = s.apply_reconcile(NOW, &op(&c.sys, kind, "", &action.to_string()));
        assert!(matches!(got, Ok(true)), "{kind}: {got:?}");
    }
    assert_eq!(s.card_state("c1").unwrap(), CardState::Ready);
}

#[test]
fn system_identity_cannot_move_a_card_through_a_transition() {
    let (mut s, c) = fixture();
    create_card(&mut s, &c, "c1");
    assert!(matches!(
        s.transition(NOW, &op(&c.sys, "card.move", "c1", "ready"), false),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer
        )))
    ));
    assert_eq!(s.card_state("c1").unwrap(), CardState::Backlog);
}

/// Every system kind except card.job_sync (which only travels inside a job finish), each with the store method
/// that applies it. The arguments are inert: an agent must be refused before any of them is read.
fn system_attempts() -> [(&'static str, Attempt); 11] {
    [
        ("job.start", |s, _c, a| {
            s.job_start(NOW, &op(a, "job.start", "j1", &start_arg("j1")))
                .map(|_| ())
        }),
        ("job.note", |s, _c, a| {
            s.job_note(NOW, &op(a, "job.note", "j1", r#"{"note":"n"}"#))
        }),
        ("job.finish", |s, _c, a| {
            s.job_finish(
                NOW,
                &op(a, "job.finish", "j1", &finish_arg(Some(0), "")),
                None,
            )
            .map(|_| ())
        }),
        ("timer.fire", |s, _c, a| {
            s.timer_fire(NOW, &op(a, "timer.fire", "tm1", r#"{"fire_ms":1}"#))
                .map(|_| ())
        }),
        ("reconcile.expire", |s, _c, a| {
            let arg = r#"{"action":"expire_lease","key":"k","lease":"l1"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.expire", "", arg))
                .map(|_| ())
        }),
        ("reconcile.quiet_close", |s, _c, a| {
            let arg = r#"{"action":"close_quiet","key":"k","lease":"q1"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.quiet_close", "", arg))
                .map(|_| ())
        }),
        ("reconcile.notice", |s, _c, a| {
            let arg =
                r#"{"action":"notice","key":"k","target":"t","kind":"k","subject":"s","text":"x"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.notice", "", arg))
                .map(|_| ())
        }),
        ("reconcile.reclaim", |s, _c, a| {
            let arg = r#"{"action":"reclaim_card","key":"k","card":"c1","owner":"o"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.reclaim", "", arg))
                .map(|_| ())
        }),
        ("reconcile.blocker", |s, _c, a| {
            let arg =
                r#"{"action":"blocker","key":"k","open":true,"title":"t","owner_action":"o"}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.blocker", "", arg))
                .map(|_| ())
        }),
        ("reconcile.admission", |s, _c, a| {
            let arg = r#"{"action":"admission","key":"k","paused":null}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.admission", "", arg))
                .map(|_| ())
        }),
        ("reconcile.claim_expire", |s, _c, a| {
            let arg =
                r#"{"action":"expire_claim","key":"k","task":"t1","agent":"a","claimed_ms":1}"#;
            s.apply_reconcile(NOW, &op(a, "reconcile.claim_expire", "", arg))
                .map(|_| ())
        }),
    ]
}

#[test]
fn agents_cannot_sign_any_system_kind() {
    let mut failures = Vec::new();
    for (kind, attempt) in system_attempts() {
        for actor in [Actor::WorkerOwn, Actor::Owner, Actor::Observer] {
            let (mut s, c) = fixture();
            let got = attempt(&mut s, &c, actor.of(&c));
            if let Some(problem) = judge(actor.role(), Gate::Forbidden, &got, &c.alice.pseudonym())
            {
                failures.push(format!("{kind} signed by {actor:?}: {problem}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn an_agent_signed_card_sync_inside_a_system_job_finish_is_refused_and_the_job_keeps_running() {
    let (mut s, c) = fixture();
    reach(&mut s, &c, CardState::Claimed);
    start_job(&mut s, &c, "j1", Some("c1"));
    let forged = op(
        &c.alice,
        "card.job_sync",
        "j1",
        &sync_arg("j1", JobOutcome::Done, Some(0)),
    );
    let got = s.job_finish(
        NOW,
        &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
        Some(&forged),
    );
    assert!(matches!(
        got,
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Worker
        )))
    ));
    assert_eq!(s.work_task("j1").unwrap().state, TaskState::Running);
    assert_eq!(s.card_state("c1").unwrap(), CardState::Claimed);
}

#[test]
fn a_replaced_system_key_is_refused_after_re_registration() {
    let (mut s, c) = fixture();
    start_job(&mut s, &c, "j1", None);
    let new = Identity::from_seed([10; 32]);
    s.register_system_identity(NOW, &new.public()).unwrap();
    assert!(matches!(
        s.job_finish(
            NOW,
            &op(&c.sys, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        ),
        Err(StoreError::Transition(TransitionError::Forbidden(
            Role::Observer
        )))
    ));
    let done = s
        .job_finish(
            NOW,
            &op(&new, "job.finish", "j1", &finish_arg(Some(0), "")),
            None,
        )
        .unwrap();
    assert_eq!(done.state, TaskState::Done);
}

#[test]
fn an_owner_cannot_grant_a_role_to_the_system_identity() {
    let (mut s, c) = fixture();
    let got = s.grant_role(
        NOW,
        &op(&c.owner, "role.grant", &pk_hex(&c.sys), "controller"),
    );
    assert!(matches!(
        got,
        Err(StoreError::Refused(m)) if m == "the system identity cannot be granted roles or revoked by an agent"
    ));
    assert_eq!(
        s.identity_status(&c.sys.public()).unwrap(),
        IdentityStatus::Active(Role::Observer)
    );
}

#[test]
fn an_owner_cannot_revoke_the_system_identity() {
    let (mut s, c) = fixture();
    let got = s.revoke_identity(NOW, &op(&c.owner, "identity.revoke", &pk_hex(&c.sys), ""));
    assert!(matches!(
        got,
        Err(StoreError::Refused(m)) if m == "the system identity cannot be granted roles or revoked by an agent"
    ));
    assert_eq!(s.system_identity().unwrap(), Some(c.sys.public()));
}

// ---- card.job_sync: only the D7 edges ----
