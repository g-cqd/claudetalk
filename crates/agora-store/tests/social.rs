//! Public-API tests for social: reactions on messages, files and cards (counting,
//! removal, targets, signatures, clock, identity and channel membership), and
//! knowledge notes (signatures, authorship, supersession, and hostile search input).

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_store::*;

/// Channel "c" with alice (admin) and bob (member), message "m" posted by alice,
/// card "c1", and a blob whose hash is returned.
fn chat() -> (World, String) {
    let mut w = world();
    let members = format!(
        r#"{{"kind":"group","members":[{{"pk":"{}","role":"member"}}]}}"#,
        pk_hex(&w.bob)
    );
    w.s.create_channel(NOW, &op(&w.alice, "channel.create", "c", &members), &[])
        .unwrap();
    let msg = SignedMessage {
        id: "m".into(),
        channel: "c".into(),
        thread_of: None,
        sender: [0; 32],
        ts: NOW,
        encrypted: false,
        epoch: 0,
        body: b"hello".to_vec(),
        attachments: vec![],
        sig: vec![],
    };
    w.s.post_message(NOW, &msg.sign(&w.alice)).unwrap();
    let file = w.s.put_blob(b"run output").unwrap();
    make_card(&mut w, "c1");
    (w, file)
}

/// Unsigned reaction; callers sign it with `.sign(&id)`.
fn reaction(kind: &str, target: &str, emoji: &str, remove: bool, ts: i64) -> SignedReaction {
    SignedReaction {
        kind: kind.into(),
        target: target.into(),
        by: [0; 32],
        emoji: emoji.into(),
        remove,
        ts,
        sig: vec![],
    }
}

/// Unsigned note; callers sign it with `.sign(&id)`.
fn note(id: &str, title: &str, body: &str, supersedes: Option<&str>, ts: i64) -> SignedNote {
    SignedNote {
        id: id.into(),
        author: [0; 32],
        ts,
        title: title.into(),
        body: body.into(),
        tags: String::new(),
        supersedes: supersedes.map(Into::into),
        sig: vec![],
    }
}

#[test]
fn reactions_are_counted_per_emoji_on_message_file_and_card() {
    let (mut w, file) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.bob),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("message", "m", "🎉", false, NOW).sign(&w.bob),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("file", &file, "👀", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("card", "c1", "🔥", false, NOW).sign(&w.alice),
    )
    .unwrap();

    assert_eq!(
        w.s.reaction_counts("message", "m").unwrap(),
        vec![("👍".to_string(), 2), ("🎉".to_string(), 1)]
    );
    assert_eq!(
        w.s.reaction_counts("file", &file).unwrap(),
        vec![("👀".to_string(), 1)]
    );
    assert_eq!(
        w.s.reaction_counts("card", "c1").unwrap(),
        vec![("🔥".to_string(), 1)]
    );
}

#[test]
fn removing_a_reaction_drops_only_that_reactors_count() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.bob),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("message", "m", "🎉", false, NOW).sign(&w.bob),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", true, NOW).sign(&w.alice),
    )
    .unwrap();

    assert_eq!(
        w.s.reaction_counts("message", "m").unwrap(),
        vec![("🎉".to_string(), 1), ("👍".to_string(), 1)]
    );
}

#[test]
fn reacting_to_a_missing_message_is_not_found() {
    let (mut w, _) = chat();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "nope", "👍", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "nope"));
}

#[test]
fn reaction_with_an_unknown_target_kind_is_invalid() {
    let (mut w, _) = chat();
    let err =
        w.s.react(
            NOW,
            &reaction("bogus", "m", "👍", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn reaction_emoji_must_be_one_to_sixty_four_bytes() {
    let (mut w, _) = chat();
    let empty =
        w.s.react(
            NOW,
            &reaction("message", "m", "", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(empty, StoreError::Invalid(_)));

    let too_long = "x".repeat(65);
    let err =
        w.s.react(
            NOW,
            &reaction("message", "m", &too_long, false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));

    let longest = "x".repeat(64);
    w.s.react(
        NOW,
        &reaction("message", "m", &longest, false, NOW).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts("message", "m").unwrap(),
        vec![(longest, 1)]
    );
}

#[test]
fn reaction_signature_covers_the_reactor_and_the_emoji() {
    let (mut w, _) = chat();

    let mut spoofed = reaction("message", "m", "👍", false, NOW).sign(&w.alice);
    spoofed.by = w.bob.public();
    let err = w.s.react(NOW, &spoofed).unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)));

    let mut tampered = reaction("message", "m", "👍", false, NOW).sign(&w.alice);
    tampered.emoji = "🎉".into();
    let err = w.s.react(NOW, &tampered).unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)));

    assert!(w.s.reaction_counts("message", "m").unwrap().is_empty());
}

#[test]
fn reaction_outside_clock_skew_is_rejected_and_the_limit_is_accepted() {
    let (mut w, _) = chat();
    let late = reaction("message", "m", "👍", false, NOW + MAX_SKEW_MS + 1).sign(&w.alice);
    assert!(matches!(w.s.react(NOW, &late), Err(StoreError::Invalid(_))));
    let early = reaction("message", "m", "👍", false, NOW - MAX_SKEW_MS - 1).sign(&w.alice);
    assert!(matches!(
        w.s.react(NOW, &early),
        Err(StoreError::Invalid(_))
    ));

    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW + MAX_SKEW_MS).sign(&w.alice),
    )
    .unwrap();
}

#[test]
fn reactions_from_unknown_or_revoked_identities_are_rejected() {
    let (mut w, _) = chat();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "m", "👍", false, NOW).sign(&w.eve),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));

    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.bob), ""))
        .unwrap();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "m", "👍", false, NOW).sign(&w.bob),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));

    assert!(w.s.reaction_counts("message", "m").unwrap().is_empty());
}

#[test]
fn observers_can_react_to_cards() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("card", "c1", "🔥", false, NOW).sign(&w.olive),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts("card", "c1").unwrap(),
        vec![("🔥".to_string(), 1)]
    );
}

/// Channel "vault-7" with alice as its only member, and message "m" posted by alice in it.
fn vault() -> World {
    let mut w = world();
    w.s.create_channel(
        NOW,
        &op(&w.alice, "channel.create", "vault-7", r#"{"kind":"group"}"#),
        &[],
    )
    .unwrap();
    let msg = SignedMessage {
        id: "m".into(),
        channel: "vault-7".into(),
        thread_of: None,
        sender: [0; 32],
        ts: NOW,
        encrypted: false,
        epoch: 0,
        body: b"hello".to_vec(),
        attachments: vec![],
        sig: vec![],
    };
    w.s.post_message(NOW, &msg.sign(&w.alice)).unwrap();
    w
}

#[test]
fn non_members_cannot_react_to_messages_but_can_react_to_cards() {
    let (mut w, _) = chat();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "m", "👍", false, NOW).sign(&w.carol),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "m"));
    assert!(w.s.reaction_counts("message", "m").unwrap().is_empty());

    w.s.react(
        NOW,
        &reaction("card", "c1", "🔥", false, NOW).sign(&w.carol),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts("card", "c1").unwrap(),
        vec![("🔥".to_string(), 1)]
    );
}

#[test]
fn non_member_reaction_to_a_message_is_not_found_with_the_message_id() {
    let mut w = vault();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "m", "👍", false, NOW).sign(&w.carol),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "m"));
}

#[test]
fn reaction_to_a_missing_message_is_not_found_with_its_own_id_for_a_non_member() {
    let mut w = vault();
    let err =
        w.s.react(
            NOW,
            &reaction("message", "nope", "👍", false, NOW).sign(&w.carol),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "nope"));
}

#[test]
fn reaction_errors_never_name_the_channel_the_message_is_in() {
    let mut w = vault();
    let existing =
        w.s.react(
            NOW,
            &reaction("message", "m", "👍", false, NOW).sign(&w.carol),
        )
        .unwrap_err();
    let missing =
        w.s.react(
            NOW,
            &reaction("message", "nope", "👍", false, NOW).sign(&w.carol),
        )
        .unwrap_err();
    for err in [&existing, &missing] {
        assert!(!err.to_string().contains("vault-7"), "{err}");
        assert!(!format!("{err:?}").contains("vault-7"), "{err:?}");
    }
}

#[test]
fn published_note_is_found_by_its_title_terms() {
    let mut w = world();
    let n = note(
        "k1",
        "Wine prefix",
        "Use the wine prefix for games",
        None,
        NOW,
    )
    .sign(&w.alice);
    w.s.publish_note(NOW, &n).unwrap();
    assert_eq!(
        w.s.search_notes("prefix", 10).unwrap(),
        vec![("k1".to_string(), "Wine prefix".to_string())]
    );
}

#[test]
fn superseding_note_replaces_the_original_in_search() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "old advice", None, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.publish_note(
        NOW,
        &note("k2", "Wine prefix revised", "new advice", Some("k1"), NOW).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(
        w.s.search_notes("prefix", 10).unwrap(),
        vec![("k2".to_string(), "Wine prefix revised".to_string())]
    );
}

#[test]
fn hostile_search_queries_return_ok_instead_of_an_fts_error() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "body", None, NOW).sign(&w.alice),
    )
    .unwrap();
    let long = "a".repeat(5000);
    let many_terms = (0..100)
        .map(|i| format!("t{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let fixed = [
        "\" OR *",
        "NEAR(",
        "*",
        "a AND",
        "\"",
        "title:wine",
        "NOT",
        "((",
        "wine*",
        "-prefix",
        "^x",
        "{",
        "a\u{0}b",
        "Wine) OR (x",
        "wine \"",
        "\" wine",
        "\"\" \"\"",
    ];
    for q in fixed
        .iter()
        .copied()
        .chain([long.as_str(), many_terms.as_str()])
    {
        let res = w.s.search_notes(q, 10);
        assert!(res.is_ok(), "query {q:?} gave {res:?}");
    }
}

#[test]
fn blank_search_returns_no_notes() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "body", None, NOW).sign(&w.alice),
    )
    .unwrap();
    assert!(w.s.search_notes("   ", 10).unwrap().is_empty());
}

#[test]
fn search_drops_nul_and_ignores_terms_past_16_and_chars_past_1024() {
    let mut w = world();
    w.s.publish_note(NOW, &note("k1", "ab", "body", None, NOW).sign(&w.alice))
        .unwrap();
    assert_eq!(
        w.s.search_notes("a\u{0}b", 10).unwrap(),
        vec![("k1".to_string(), "ab".to_string())]
    );
    // "absentterm" starts at char 1024, so it is cut before tokenizing and the query matches "ab".
    let past_1024 = format!("ab{}absentterm", " ".repeat(1022));
    assert_eq!(
        w.s.search_notes(&past_1024, 10).unwrap(),
        vec![("k1".to_string(), "ab".to_string())]
    );

    let title = (0..16)
        .map(|i| format!("t{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    w.s.publish_note(NOW, &note("k2", &title, "body", None, NOW).sign(&w.alice))
        .unwrap();
    // Seventeen terms: only the first sixteen are kept, so the absent 17th term cannot block the match.
    assert_eq!(
        w.s.search_notes(&format!("{title} absentterm"), 10)
            .unwrap(),
        vec![("k2".to_string(), title.clone())]
    );
}

#[test]
fn note_signature_covers_its_body() {
    let mut w = world();
    let mut n = note("k1", "Wine prefix", "body", None, NOW).sign(&w.alice);
    n.body = "forged".into();
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Crypto(_))
    ));
    assert!(w.s.search_notes("prefix", 10).unwrap().is_empty());
}

#[test]
fn observers_cannot_publish_notes() {
    let mut w = world();
    let n = note("k1", "Wine prefix", "body", None, NOW).sign(&w.olive);
    let err = w.s.publish_note(NOW, &n).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
}

#[test]
fn unknown_identities_cannot_publish_notes() {
    let mut w = world();
    let n = note("k1", "Wine prefix", "body", None, NOW).sign(&w.eve);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn notes_outside_clock_skew_are_rejected() {
    let mut w = world();
    let late = note("k1", "Wine prefix", "body", None, NOW + MAX_SKEW_MS + 1).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &late),
        Err(StoreError::Invalid(_))
    ));
    let early = note("k1", "Wine prefix", "body", None, NOW - MAX_SKEW_MS - 1).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &early),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn note_needs_a_non_empty_id_and_title() {
    let mut w = world();
    let untitled = note("k1", "", "body", None, NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &untitled),
        Err(StoreError::Invalid(_))
    ));
    let unnamed = note("", "Wine prefix", "body", None, NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &unnamed),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn superseding_a_missing_note_is_not_found() {
    let mut w = world();
    let n = note("k2", "Wine prefix", "body", Some("nope"), NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn note_supersedes_longer_than_256_bytes_is_invalid() {
    let mut w = world();
    let n = note("k2", "Wine prefix", "body", Some(&"s".repeat(257)), NOW).sign(&w.alice);
    let err = w.s.publish_note(NOW, &n).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Invalid("supersedes longer than 256 bytes")
    ));
}

#[test]
fn workers_cannot_supersede_another_authors_note() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "alice's advice", None, NOW).sign(&w.alice),
    )
    .unwrap();
    let hostile = note(
        "k2",
        "Wine prefix hijacked",
        "bob's advice",
        Some("k1"),
        NOW,
    )
    .sign(&w.bob);
    let err = w.s.publish_note(NOW, &hostile).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Worker))
    ));
    assert_eq!(
        w.s.search_notes("prefix", 10).unwrap(),
        vec![("k1".to_string(), "Wine prefix".to_string())]
    );
}

#[test]
fn supervisors_can_supersede_another_authors_note() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "alice's advice", None, NOW).sign(&w.alice),
    )
    .unwrap();
    let fix = note(
        "k2",
        "Wine prefix corrected",
        "corrected advice",
        Some("k1"),
        NOW,
    )
    .sign(&w.sup);
    w.s.publish_note(NOW, &fix).unwrap();
    assert_eq!(
        w.s.search_notes("prefix", 10).unwrap(),
        vec![("k2".to_string(), "Wine prefix corrected".to_string())]
    );
}

/// Publishes `count` notes by alice, each with "Wine" in its title so all of them match "wine".
fn publish_wine_notes(w: &mut World, count: usize) {
    for i in 0..count {
        let n = note(
            &format!("k{i}"),
            &format!("Wine note {i}"),
            "body",
            None,
            NOW,
        )
        .sign(&w.alice);
        w.s.publish_note(NOW, &n).unwrap();
    }
}

#[test]
fn resubmitting_an_add_after_its_removal_is_a_replay() {
    let (mut w, _) = chat();
    let add = reaction("message", "m", "👍", false, NOW).sign(&w.alice);
    let remove = reaction("message", "m", "👍", true, NOW + 1).sign(&w.alice);
    w.s.react(NOW, &add).unwrap();
    w.s.react(NOW + 1, &remove).unwrap();
    let err = w.s.react(NOW + 1, &add).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("replayed operation")));
    assert!(w.s.reaction_counts("message", "m").unwrap().is_empty());
}

#[test]
fn resubmitting_a_removal_is_a_replay() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.alice),
    )
    .unwrap();
    let remove = reaction("message", "m", "👍", true, NOW + 1).sign(&w.alice);
    w.s.react(NOW + 1, &remove).unwrap();
    w.s.react(
        NOW + 2,
        &reaction("message", "m", "👍", false, NOW + 2).sign(&w.alice),
    )
    .unwrap();
    let err = w.s.react(NOW + 2, &remove).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("replayed operation")));
    assert_eq!(
        w.s.reaction_counts("message", "m").unwrap(),
        vec![("👍".to_string(), 1)]
    );
}

#[test]
fn reactions_are_idempotent_and_removal_works_for_file_and_card() {
    let (mut w, file) = chat();
    w.s.react(
        NOW,
        &reaction("file", &file, "👀", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW + 1,
        &reaction("file", &file, "👀", false, NOW + 1).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("card", "c1", "🔥", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW + 1,
        &reaction("card", "c1", "🔥", false, NOW + 1).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts("file", &file).unwrap(),
        vec![("👀".to_string(), 1)]
    );
    assert_eq!(
        w.s.reaction_counts("card", "c1").unwrap(),
        vec![("🔥".to_string(), 1)]
    );

    w.s.react(
        NOW + 2,
        &reaction("file", &file, "👀", true, NOW + 2).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW + 2,
        &reaction("card", "c1", "🔥", true, NOW + 2).sign(&w.alice),
    )
    .unwrap();
    assert!(w.s.reaction_counts("file", &file).unwrap().is_empty());
    assert!(w.s.reaction_counts("card", "c1").unwrap().is_empty());

    let events = w.s.event_count().unwrap();
    w.s.react(
        NOW + 3,
        &reaction("file", &file, "🎉", true, NOW + 3).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(w.s.event_count().unwrap(), events);
}

#[test]
fn reacting_to_a_missing_file_or_card_is_not_found() {
    let (mut w, _) = chat();
    let file =
        w.s.react(
            NOW,
            &reaction("file", "ghost", "👀", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(file, StoreError::NotFound(ref t) if t == "ghost"));
    let card =
        w.s.react(
            NOW,
            &reaction("card", "ghost", "🔥", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(card, StoreError::NotFound(ref t) if t == "ghost"));
}

#[test]
fn flipping_the_remove_flag_breaks_the_signature() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.alice),
    )
    .unwrap();
    let mut tampered = reaction("message", "m", "👍", false, NOW + 1).sign(&w.alice);
    tampered.remove = true;
    let err = w.s.react(NOW + 1, &tampered).unwrap_err();
    assert!(matches!(err, StoreError::Crypto(_)));
    assert_eq!(
        w.s.reaction_counts("message", "m").unwrap(),
        vec![("👍".to_string(), 1)]
    );
}

#[test]
fn reaction_target_longer_than_256_bytes_is_invalid() {
    let (mut w, _) = chat();
    let target = "x".repeat(257);
    let err =
        w.s.react(
            NOW,
            &reaction("card", &target, "🔥", false, NOW).sign(&w.alice),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn reaction_with_an_extreme_timestamp_is_invalid() {
    let (mut w, _) = chat();
    for ts in [i64::MIN, i64::MAX] {
        let r = reaction("message", "m", "👍", false, ts).sign(&w.alice);
        assert!(
            matches!(w.s.react(NOW, &r), Err(StoreError::Invalid(_))),
            "ts {ts}"
        );
    }
}

#[test]
fn reaction_counts_for_shows_counts_to_a_channel_member() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.bob),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts_for(&w.alice.public(), "message", "m")
            .unwrap(),
        vec![("👍".to_string(), 1)]
    );
}

#[test]
fn reaction_counts_for_refuses_a_non_member_with_not_found_of_the_message() {
    let (mut w, _) = chat();
    w.s.react(
        NOW,
        &reaction("message", "m", "👍", false, NOW).sign(&w.alice),
    )
    .unwrap();
    let err =
        w.s.reaction_counts_for(&w.carol.public(), "message", "m")
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "m"));
}

#[test]
fn reaction_counts_for_a_missing_message_is_not_found_with_its_id() {
    let (w, _) = chat();
    let err =
        w.s.reaction_counts_for(&w.alice.public(), "message", "nope")
            .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(ref t) if t == "nope"));
}

#[test]
fn reaction_counts_for_does_not_gate_file_or_card_targets() {
    let (mut w, file) = chat();
    w.s.react(
        NOW,
        &reaction("file", &file, "👀", false, NOW).sign(&w.alice),
    )
    .unwrap();
    w.s.react(
        NOW,
        &reaction("card", "c1", "🔥", false, NOW).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(
        w.s.reaction_counts_for(&w.carol.public(), "file", &file)
            .unwrap(),
        vec![("👀".to_string(), 1)]
    );
    assert_eq!(
        w.s.reaction_counts_for(&w.carol.public(), "card", "c1")
            .unwrap(),
        vec![("🔥".to_string(), 1)]
    );
}

#[test]
fn reaction_counts_for_an_unknown_kind_is_invalid() {
    let (w, _) = chat();
    let err =
        w.s.reaction_counts_for(&w.alice.public(), "thread", "m")
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn reaction_counts_for_an_unknown_reader_is_rejected_before_the_membership_gate() {
    let (w, _) = chat();
    let err =
        w.s.reaction_counts_for(&w.eve.public(), "message", "m")
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid(_)));
}

#[test]
fn search_returns_at_most_200_notes_however_large_the_limit() {
    let mut w = world();
    publish_wine_notes(&mut w, 250);
    assert_eq!(w.s.search_notes("wine", i64::MAX).unwrap().len(), 200);
}

#[test]
fn negative_search_limit_returns_no_notes_without_error() {
    let mut w = world();
    publish_wine_notes(&mut w, 3);
    assert!(w.s.search_notes("wine", -1).unwrap().is_empty());
}

#[test]
fn duplicate_note_id_is_invalid_and_the_original_stands() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "original", None, NOW).sign(&w.alice),
    )
    .unwrap();
    let again = note("k1", "Wine prefix replaced", "replacement", None, NOW).sign(&w.alice);
    let err = w.s.publish_note(NOW, &again).unwrap_err();
    assert!(matches!(err, StoreError::Invalid("note already exists")));
    assert_eq!(
        w.s.search_notes("prefix", 10).unwrap(),
        vec![("k1".to_string(), "Wine prefix".to_string())]
    );
}

#[test]
fn note_id_longer_than_256_bytes_is_invalid() {
    let mut w = world();
    let n = note(&"k".repeat(257), "Wine prefix", "body", None, NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn note_title_longer_than_512_bytes_is_invalid() {
    let mut w = world();
    let n = note("k1", &"t".repeat(513), "body", None, NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn note_body_larger_than_max_body_is_invalid() {
    let mut w = world();
    let n = note("k1", "Wine prefix", &"b".repeat(MAX_BODY + 1), None, NOW).sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn note_tags_longer_than_1024_bytes_are_invalid() {
    let mut w = world();
    let n = SignedNote {
        tags: "g".repeat(1025),
        ..note("k1", "Wine prefix", "body", None, NOW)
    }
    .sign(&w.alice);
    assert!(matches!(
        w.s.publish_note(NOW, &n),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn note_fields_at_their_caps_are_accepted() {
    let mut w = world();
    let n = SignedNote {
        tags: "g".repeat(1024),
        ..note(
            &"k".repeat(256),
            &"t".repeat(512),
            &"b".repeat(MAX_BODY),
            None,
            NOW,
        )
    }
    .sign(&w.alice);
    w.s.publish_note(NOW, &n).unwrap();
}

#[test]
fn note_exactly_at_the_clock_skew_bound_is_accepted() {
    let mut w = world();
    w.s.publish_note(
        NOW,
        &note("k1", "Wine prefix", "early", None, NOW - MAX_SKEW_MS).sign(&w.alice),
    )
    .unwrap();
    w.s.publish_note(
        NOW,
        &note("k2", "Wine prefix", "late", None, NOW + MAX_SKEW_MS).sign(&w.alice),
    )
    .unwrap();
    assert_eq!(w.s.search_notes("prefix", 10).unwrap().len(), 2);
}

#[test]
fn note_with_an_extreme_timestamp_is_invalid() {
    let mut w = world();
    for ts in [i64::MIN, i64::MAX] {
        let n = note("k1", "Wine prefix", "body", None, ts).sign(&w.alice);
        assert!(
            matches!(w.s.publish_note(NOW, &n), Err(StoreError::Invalid(_))),
            "ts {ts}"
        );
    }
}
