//! Path lease tests: conflicts at path boundaries, claimed task files, and expiry instants.

use super::*;

#[test]
fn a_path_lease_records_its_holder_and_expires_after_the_default_ttl() {
    let mut w = world();
    let lease = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    assert_eq!(
        lease,
        LeaseRecord {
            id: lease.id.clone(),
            kind: LeaseKind::Path,
            resource: "/proj/w".into(),
            amount: 0.0,
            holder: pk_hex(&w.alice),
            holder_name: pseudonym(&w.alice.public()),
            card: None,
            task: None,
            created_ms: NOW,
            expires_ms: Some(NOW + 900 * SEC),
            heartbeat_ms: NOW,
            expected_end_ms: None,
            released_ms: None,
            note: String::new(),
        }
    );
}

#[test]
fn overlapping_paths_are_refused_in_both_directions_and_the_message_names_the_holder() {
    let mut w = world();
    let held = acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW).expect("alice takes /w");
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w/x/y", None, NOW),
        &held_message(
            "/proj/w/x/y",
            &w.alice,
            &format!("lease {}", held.id),
            DEFAULT_EXPIRY,
        ),
    );
    let deeper = acquire_path(&mut w.s, &w.bob, "/proj/p/q", None, NOW).expect("bob takes /p/q");
    refused(
        acquire_path(&mut w.s, &w.carol, "/proj/p", None, NOW),
        &held_message(
            "/proj/p",
            &w.bob,
            &format!("lease {}", deeper.id),
            DEFAULT_EXPIRY,
        ),
    );
}

#[test]
fn sibling_paths_and_a_shared_string_prefix_do_not_conflict() {
    let mut w = world();
    acquire_path(&mut w.s, &w.alice, "/proj/w/a", None, NOW).expect("alice takes /w/a");
    acquire_path(&mut w.s, &w.bob, "/proj/w/b", None, NOW).expect("a sibling is free");
    acquire_path(&mut w.s, &w.carol, "/proj/w/ab", None, NOW)
        .expect("a shared string prefix is not a path overlap");
}

#[test]
fn the_holder_may_take_an_overlapping_path_again() {
    let mut w = world();
    acquire_path(&mut w.s, &w.alice, "/proj/w/x", None, NOW).expect("alice takes /w/x");
    acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW)
        .expect("alice may widen her own lease to its parent");
    let mine = pk_hex(&w.alice);
    assert_eq!(
        listed(&w.s, NOW, Some(LeaseKind::Path), Some(mine.as_str()), true).len(),
        2
    );
}

#[test]
fn a_path_lease_holds_until_its_expiry_instant_and_not_after() {
    let mut w = world();
    let held =
        acquire_path(&mut w.s, &w.alice, "/proj/w", Some(10.0), NOW).expect("alice takes /w");
    let just_before = NOW + 10 * SEC - 1;
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w", None, just_before),
        &held_message(
            "/proj/w",
            &w.alice,
            &format!("lease {}", held.id),
            "2023-11-14T22:13:30Z",
        ),
    );
    acquire_path(&mut w.s, &w.bob, "/proj/w", None, NOW + 10 * SEC)
        .expect("the path is free at the expiry instant");
}

#[test]
fn a_claimed_task_holds_its_files_against_other_identities() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    claim_task(&mut w.s, &w.alice, "t1");
    refused(
        acquire_path(&mut w.s, &w.bob, "/proj/w", None, NOW),
        &held_message("/proj/w", &w.alice, "task t1", "-"),
    );
}

#[test]
fn the_claimant_may_lease_the_files_of_its_own_claimed_task() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    claim_task(&mut w.s, &w.alice, "t1");
    acquire_path(&mut w.s, &w.alice, "/proj/w", None, NOW)
        .expect("the claimant is not blocked by its own task");
}

#[test]
fn a_queued_task_holds_no_files() {
    let mut w = world();
    queue_task(&mut w.s, &w.alice, "t1", &["/proj/w/x.rs"]);
    acquire_path(&mut w.s, &w.bob, "/proj/w/x.rs", None, NOW)
        .expect("only a claimed or running task holds files");
}

#[test]
fn a_relative_path_and_the_quiet_kind_are_refused_by_lease_acquire() {
    let mut w = world();
    let relative = serde_json::json!({"kind": "path", "resource": "w"}).to_string();
    invalid(
        w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &relative)),
        "path must be absolute",
    );
    let quiet = serde_json::json!({"kind": "quiet", "resource": "machine"}).to_string();
    invalid(
        w.s.lease_acquire(NOW, &op(&w.alice, "lease.acquire", "", &quiet)),
        "use quiet.open",
    );
}

// ---- renewal and release -----------------------------------------------------------------------------------
