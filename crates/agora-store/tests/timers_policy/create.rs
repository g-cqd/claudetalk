//! Timer creation: the schedule and template rules, the per-identity quota, and the id limit (D11).

use super::*;

// ---- timer creation --------------------------------------------------------------------------------------------

#[test]
fn a_worker_creates_an_enabled_timer_due_at_its_first_fire() {
    let mut f = fixture();
    let first = NOW + MINUTE_MS;
    let timer = create(
        &mut f.s,
        &f.alice,
        NOW,
        "nightly",
        every(60, Some(first)),
        plain_template(),
    )
    .expect("create");
    assert_eq!(timer.id, "nightly");
    assert_eq!(timer.owner, pk_hex(&f.alice));
    assert!(timer.enabled);
    assert_eq!(timer.next_fire_ms, Some(first));
    assert_eq!(timer.last_fire_ms, None);
    assert_eq!(timer.fired, 0);
    assert_eq!(timer.created_ms, NOW);
    assert_eq!(timer.template.title, TITLE);
}

#[test]
fn an_observer_cannot_create_a_timer() {
    let mut f = fixture();
    let err = create(
        &mut f.s,
        &f.olive,
        NOW,
        "watch",
        every(60, None),
        plain_template(),
    );
    assert_eq!(forbidden(err), Role::Observer);
    assert!(f.s.timers(None).expect("timers").is_empty());
}

#[test]
fn a_timer_id_is_limited_to_100_characters() {
    let mut f = fixture();
    let too_long = "t".repeat(101);
    assert_eq!(
        invalid(create(
            &mut f.s,
            &f.alice,
            NOW,
            &too_long,
            every(60, None),
            plain_template(),
        )),
        "timer id is longer than 100 characters"
    );
    let longest = "t".repeat(100);
    let timer = create(
        &mut f.s,
        &f.alice,
        NOW,
        &longest,
        every(60, None),
        plain_template(),
    )
    .expect("a 100-character id is accepted");
    assert_eq!(timer.id, longest);
}

#[test]
fn an_every_timer_shorter_than_60_seconds_is_refused_and_60_is_accepted() {
    let mut f = fixture();
    assert_eq!(
        refusal(create(
            &mut f.s,
            &f.alice,
            NOW,
            "fast",
            every(59, None),
            plain_template(),
        )),
        "every must be at least 60 seconds"
    );
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "minute",
        every(60, None),
        plain_template(),
    )
    .expect("60 seconds is the minimum period");
}

#[test]
fn a_cron_schedule_that_never_fires_is_refused() {
    let mut f = fixture();
    let spec = json!({"kind": "cron", "expr": "0 0 30 2 *"});
    assert_eq!(
        refusal(create(
            &mut f.s,
            &f.alice,
            NOW,
            "never",
            spec,
            plain_template(),
        )),
        "the schedule never fires"
    );
}

#[test]
fn the_timer_quota_refuses_a_timer_beyond_the_limit_with_the_quota_message() {
    let mut f = fixture();
    set_quota(&mut f, "2");
    for id in ["a1", "a2"] {
        create(
            &mut f.s,
            &f.alice,
            NOW,
            id,
            every(60, None),
            plain_template(),
        )
        .expect("within quota");
    }
    assert_eq!(
        refusal(create(
            &mut f.s,
            &f.alice,
            NOW,
            "a3",
            every(60, None),
            plain_template(),
        )),
        "timer quota reached: 2 of 2 enabled timers"
    );
    assert_eq!(f.s.timers(None).expect("timers").len(), 2);
}

#[test]
fn the_timer_quota_is_counted_for_each_identity_separately() {
    let mut f = fixture();
    set_quota(&mut f, "1");
    create(
        &mut f.s,
        &f.alice,
        NOW,
        "a1",
        every(60, None),
        plain_template(),
    )
    .expect("alice's one timer");
    create(
        &mut f.s,
        &f.bob,
        NOW,
        "b1",
        every(60, None),
        plain_template(),
    )
    .expect("bob has his own quota");
    assert_eq!(
        refusal(create(
            &mut f.s,
            &f.alice,
            NOW,
            "a2",
            every(60, None),
            plain_template(),
        )),
        "timer quota reached: 1 of 1 enabled timers"
    );
}

#[test]
fn a_command_timer_needs_a_working_directory() {
    let mut f = fixture();
    let template = json!({"title": TITLE, "command": ["touch", "/tmp/x"]});
    assert_eq!(
        invalid(create(
            &mut f.s,
            &f.alice,
            NOW,
            "touch",
            every(60, None),
            template,
        )),
        "a command needs a working directory"
    );
}

#[test]
fn a_timer_working_directory_must_be_absolute() {
    let mut f = fixture();
    let template = json!({"title": TITLE, "command": ["touch", "/tmp/x"], "cwd": "relative/dir"});
    assert_eq!(
        invalid(create(
            &mut f.s,
            &f.alice,
            NOW,
            "touch",
            every(60, None),
            template,
        )),
        "the working directory must be an absolute path"
    );
}

#[test]
fn a_timer_template_takes_at_most_1000_files() {
    let mut f = fixture();
    let files = |n: usize| -> Vec<String> { (0..n).map(|i| format!("/tmp/f{i}")).collect() };
    let over = json!({"title": TITLE, "files": files(1001)});
    assert_eq!(
        invalid(create(
            &mut f.s,
            &f.alice,
            NOW,
            "many",
            every(60, None),
            over,
        )),
        "too many files or prerequisites"
    );
    let at_limit = json!({"title": TITLE, "files": files(1000)});
    let timer = create(&mut f.s, &f.alice, NOW, "many", every(60, None), at_limit)
        .expect("1000 files is the limit");
    assert_eq!(timer.template.files.len(), 1000);
}

#[test]
fn a_timer_template_takes_at_most_1000_prerequisites() {
    let mut f = fixture();
    let after: Vec<String> = (0..1001).map(|i| format!("t{i}")).collect();
    let template = json!({"title": TITLE, "after": after});
    assert_eq!(
        invalid(create(
            &mut f.s,
            &f.alice,
            NOW,
            "chain",
            every(60, None),
            template,
        )),
        "too many files or prerequisites"
    );
}
