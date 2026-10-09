//! Tests of the notification's whole-message count and of the session cursor mark that follows a
//! channel send: what the notification shows is marked, and what it cuts, or what was never
//! queued, stays unmarked for the hooks.

use super::*;
use crate::engine::{EngineConfig, Transport};
use agora_client::Ctx;
use agora_client::session_state;
use agora_crypto::Identity;
use agora_daemon::Paths;
use std::path::Path;
use tokio::sync::mpsc;

const SID: &str = "push-cursor-test-session";

fn msg(seq: i64, text: &str) -> Value {
    json!({
        "seq": seq, "id": format!("m-{seq}"), "channel": "general", "thread_of": null,
        "sender": "ab", "pseudonym": format!("p{seq}"), "ts": 1, "encrypted": false,
        "epoch": 0, "verified": true, "text": text, "attachments": [],
    })
}

fn small(seq: i64) -> Value {
    msg(seq, &format!("message {seq}"))
}

/// A 6000-byte message: the content cap (8192 bytes) leaves room for one such block, not two.
fn large(seq: i64) -> Value {
    msg(seq, &"x".repeat(6000))
}

/// A 6000-char message of two-byte chars (12000 bytes): no block of it fits the content cap.
fn wide(seq: i64) -> Value {
    msg(seq, &"é".repeat(6000))
}

fn cfg() -> PushConfig {
    PushConfig::default()
}

/// A stdio push task whose session is `SID` and whose home is `home`, with the notifications it
/// sends on the returned receiver. The task is not run: the tests drive `absorb` and `send_pending`.
fn push_in(home: &Path) -> (Push, mpsc::Receiver<Value>) {
    let ctx = Ctx {
        paths: Paths::new(home),
        identity: Identity::from_seed([7; 32]),
        label: "push-cursor-test".to_owned(),
    };
    let peer = Arc::new(Peer::new(Arc::new(ctx), Transport::Stdio, None));
    let (tx, rx) = mpsc::channel(16);
    let mut push = Push::new(Engine::new(EngineConfig::stdio()), peer, Notifier::new(tx));
    push.session = Some(SID.to_owned());
    (push, rx)
}

/// The content of the one notification waiting on `rx`.
fn only_content(rx: &mut mpsc::Receiver<Value>) -> String {
    let note = rx.try_recv().expect("one channel notification");
    assert!(rx.try_recv().is_err(), "more than one notification");
    note["params"]["content"]
        .as_str()
        .expect("content string")
        .to_owned()
}

fn cursor_of(home: &Path) -> i64 {
    session_state::load(home, SID).cursor
}

#[test]
fn two_wide_messages_leave_no_whole_block() {
    let (_, whole) = build_notification_counted(&[wide(1), wide(2)], &cfg());
    assert_eq!(whole, 0);
}

#[test]
fn a_6000_byte_message_is_whole_but_the_second_one_is_cut() {
    let (_, whole) = build_notification_counted(&[large(1), large(2)], &cfg());
    assert_eq!(whole, 1);
}

#[test]
fn two_small_messages_are_both_whole() {
    let (_, whole) = build_notification_counted(&[small(1), small(2)], &cfg());
    assert_eq!(whole, 2);
}

#[test]
fn five_small_messages_fill_the_frame_and_are_all_whole() {
    let msgs: Vec<Value> = (1..=5).map(small).collect();
    let (_, whole) = build_notification_counted(&msgs, &cfg());
    assert_eq!(whole, 5);
}

#[test]
fn messages_past_max_messages_are_not_whole() {
    let msgs: Vec<Value> = (1..=7).map(small).collect();
    let (_, whole) = build_notification_counted(&msgs, &cfg());
    assert_eq!(whole, 5);
}

#[test]
fn a_message_cut_by_the_content_cap_is_not_whole() {
    // The first two blocks fit under the cap; the third is cut inside its body.
    let (note, whole) = build_notification_counted(&[small(1), large(2), large(3)], &cfg());
    assert_eq!(whole, 2);
    let content = note["params"]["content"].as_str().expect("content string");
    assert!(content.contains(TRUNCATION_MARKER), "{content}");
}

#[test]
fn build_notification_is_the_counted_notification_without_its_count() {
    let msgs = [small(1), large(2)];
    let (counted, _) = build_notification_counted(&msgs, &cfg());
    assert_eq!(build_notification(&msgs, &cfg()), counted);
}

#[test]
fn mark_target_is_the_shown_max_when_nothing_is_unsent() {
    let mut floor = None;
    assert_eq!(mark_target(5, 2, &mut floor), Some(5));
    assert_eq!(floor, None);
}

#[test]
fn mark_target_stays_below_an_unsent_floor() {
    let mut floor = Some(4);
    assert_eq!(mark_target(9, 0, &mut floor), Some(3));
    assert_eq!(
        floor,
        Some(4),
        "the floor stands until the hook cursor passes it"
    );
}

#[test]
fn mark_target_clears_a_floor_the_hook_cursor_has_passed() {
    let mut floor = Some(4);
    assert_eq!(mark_target(9, 4, &mut floor), Some(9));
    assert_eq!(floor, None);
}

#[test]
fn mark_target_is_none_when_the_clamp_does_not_pass_the_cursor() {
    let mut floor = Some(1);
    assert_eq!(mark_target(9, 0, &mut floor), None);
    assert_eq!(floor, Some(1));
}

#[test]
fn a_message_at_or_below_the_session_cursor_is_not_sent() {
    let home = tempfile::tempdir().expect("tempdir");
    assert!(session_state::try_advance(home.path(), SID, 5).expect("advance"));
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![msg(4, "stale-4"), msg(6, "fresh-6")], true);

    push.send_pending();

    let content = only_content(&mut rx);
    assert!(content.contains("fresh-6"), "{content}");
    assert!(!content.contains("stale-4"), "{content}");
    assert_eq!(cursor_of(home.path()), 6);
}

#[test]
fn after_a_send_the_cursor_is_the_max_seq_of_the_messages_sent() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![small(1), small(2)], true);

    push.send_pending();

    let _ = only_content(&mut rx);
    assert_eq!(cursor_of(home.path()), 2);
}

#[test]
fn a_batch_cut_by_the_cap_does_not_move_the_cursor_past_its_last_whole_message() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![small(1), large(2), large(3)], true);

    push.send_pending();

    let content = only_content(&mut rx);
    assert!(content.contains("message 1"), "{content}");
    assert_eq!(
        cursor_of(home.path()),
        2,
        "seq 3 was cut and must stay for the hooks"
    );
}

#[test]
fn a_message_never_queued_on_the_first_link_holds_the_mark_below_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    // The first link's backlog: read, seen, not pushed.
    push.absorb(vec![small(1)], false);
    assert_eq!(push.unsent_floor, Some(1));
    push.absorb(vec![small(2)], true);

    push.send_pending();

    let content = only_content(&mut rx);
    assert!(content.contains("message 2"), "{content}");
    assert_eq!(
        cursor_of(home.path()),
        0,
        "a mark at 2 would hide seq 1 from the hooks"
    );
    assert_eq!(push.unsent_floor, Some(1));
}

#[test]
fn a_floor_clamps_the_mark_and_is_cleared_once_the_hook_cursor_passes_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![small(3)], false);
    push.absorb(vec![small(4), small(5)], true);

    push.send_pending();
    let _ = only_content(&mut rx);
    assert_eq!(
        cursor_of(home.path()),
        2,
        "clamped to the floor (3) minus one"
    );
    assert_eq!(push.unsent_floor, Some(3));

    // The hooks deliver up to the floor; the next send is no longer held below it.
    assert!(session_state::try_advance(home.path(), SID, 3).expect("advance"));
    push.absorb(vec![small(6)], true);
    push.send_pending();
    let _ = only_content(&mut rx);
    assert_eq!(cursor_of(home.path()), 6);
    assert_eq!(push.unsent_floor, None);
}

#[test]
fn a_message_dropped_from_the_full_queue_sets_the_floor() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, _rx) = push_in(home.path());
    let fetched: Vec<Value> = (1..=(PENDING_CAP as i64 + 3)).map(small).collect();
    push.absorb(fetched, true);
    assert_eq!(push.pending.len(), PENDING_CAP);
    assert_eq!(push.unsent_floor, Some(1));
}

#[test]
fn a_send_whose_messages_the_hooks_delivered_takes_no_token() {
    let home = tempfile::tempdir().expect("tempdir");
    assert!(session_state::try_advance(home.path(), SID, 5).expect("advance"));
    let (mut push, mut rx) = push_in(home.path());
    // A bucket with no tokens at all: only a send that reaches the token can be held here.
    push.bucket = Bucket::new(0, 2000, now_ms());
    push.absorb(vec![small(3), small(4)], true);

    push.send_pending();

    assert!(
        push.pending.is_empty(),
        "delivered messages are dropped before the token check"
    );
    assert!(rx.try_recv().is_err());
}

/// How many times the tracker counts `seq` as pushed (0 when it was never pushed).
fn rungs_tried(push: &Push, seq: i64) -> u64 {
    let status = delivery(&push.peer).status_json();
    let entry = status["pending"]
        .as_array()
        .expect("pending array")
        .iter()
        .find(|e| e["seq"] == json!(seq))
        .expect("the message is tracked");
    entry["rungs_tried"].as_u64().expect("rungs_tried count")
}

#[test]
fn a_message_cut_from_a_send_holds_the_mark_until_the_hooks_pass_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![small(1), large(2), large(3)], true);

    push.send_pending();

    // Seqs 1 and 2 are framed whole; seq 3 is cut and leaves the queue unshown.
    let _ = only_content(&mut rx);
    assert_eq!(cursor_of(home.path()), 2);

    push.absorb(vec![small(4)], true);
    push.send_pending();

    let _ = only_content(&mut rx);
    assert_eq!(
        cursor_of(home.path()),
        2,
        "a mark past seq 3 would hide it from the hooks"
    );

    assert!(session_state::try_advance(home.path(), SID, 3).expect("advance"));
    push.absorb(vec![small(5)], true);
    push.send_pending();

    let _ = only_content(&mut rx);
    assert_eq!(cursor_of(home.path()), 5);
}

#[test]
fn messages_cut_from_an_all_wide_send_hold_the_mark_below_them() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![wide(3), wide(4)], true);

    push.send_pending();

    let _ = only_content(&mut rx);
    assert_eq!(cursor_of(home.path()), 0, "nothing was framed whole");

    push.absorb(vec![small(6)], true);
    push.send_pending();

    let _ = only_content(&mut rx);
    assert!(
        cursor_of(home.path()) < 3,
        "seqs 3 and 4 were never shown and must stay for the hooks"
    );
}

#[test]
fn the_tracker_counts_every_message_in_the_batch_as_pushed_even_when_one_was_cut() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut push, mut rx) = push_in(home.path());
    push.absorb(vec![small(1), large(2), large(3)], true);

    push.send_pending();

    let _ = only_content(&mut rx);
    assert_eq!(rungs_tried(&push, 1), 1, "seq 1 was shown");
    assert_eq!(rungs_tried(&push, 2), 1, "seq 2 was shown");
    // Seq 3 was cut but is named by the "more unread" line; it counts as pushed and escalates if
    // it is never acked. The hook mark floor (tests above) is what keeps the hooks delivering it.
    assert_eq!(
        rungs_tried(&push, 3),
        1,
        "seq 3 is named by the more-unread line"
    );
}
