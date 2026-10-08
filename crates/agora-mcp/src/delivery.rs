//! Delivery bookkeeping: which unacked messages were seen / pushed, and which rung of the
//! harness's delivery ladder fires next (`agora_deliver`). A `ChannelPush` escalation is
//! recorded only when a push was actually sent (`on_pushed`); when its rung comes due with
//! nothing sent, `tick` advances past it silently. The other fired rungs are recorded by `tick`
//! and are passive (`executed: false`) in M3.

use crate::clients::ladder_json;
use agora_deliver::{Action, Attempt, Mechanism, next_action, patience_ms};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A fired ladder rung.
#[derive(Debug, Clone, Serialize)]
pub struct Escalation {
    pub channel: String,
    pub seq: i64,
    /// `Mechanism::as_str()`.
    pub mechanism: &'static str,
    pub at_ms: i64,
    /// `true` only for a `channel_push` recorded by `on_pushed` (a push was sent).
    pub executed: bool,
}

/// Append `escalation`, dropping the oldest beyond `MAX_ESCALATIONS`.
fn record_escalation(log: &mut VecDeque<Escalation>, escalation: Escalation) {
    if log.len() >= MAX_ESCALATIONS {
        log.pop_front();
    }
    log.push_back(escalation);
}

/// Escalations kept (oldest dropped).
const MAX_ESCALATIONS: usize = 200;

/// Escalations shown by `status_json` (the most recent ones, oldest first).
const STATUS_ESCALATIONS: usize = 50;

/// One pending (seen, unacked) message.
#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    attempt: Attempt,
    /// The ladder is used up; kept until the message is acked.
    exhausted: bool,
}

/// Per-peer delivery state over an `agora_deliver` ladder.
pub struct Tracker {
    ladder: Vec<Mechanism>,
    push_enabled: bool,
    /// Keyed by (channel, seq); the ordering groups a channel's messages by seq.
    pending: BTreeMap<(String, i64), Entry>,
    escalations: VecDeque<Escalation>,
    pushed: u64,
    coalesced: u64,
    acked: u64,
}

impl Tracker {
    /// An empty tracker over `ladder`. With push disabled the ladder of a ClaudeCode client is
    /// just `[WaitTool]` (`clients::ladder_for`), so everything is passive.
    pub fn new(ladder: Vec<Mechanism>, push_enabled: bool) -> Self {
        Self {
            ladder,
            push_enabled,
            pending: BTreeMap::new(),
            escalations: VecDeque::new(),
            pushed: 0,
            coalesced: 0,
            acked: 0,
        }
    }

    /// Replace the ladder / push flag (pending entries keep their attempts). Exhaustion is
    /// re-derived from the new ladder on the next `tick`.
    pub fn reconfigure(&mut self, ladder: Vec<Mechanism>, push_enabled: bool) {
        self.ladder = ladder;
        self.push_enabled = push_enabled;
        for entry in self.pending.values_mut() {
            entry.exhausted = false;
        }
    }

    /// The message was seen unacked: insert `Attempt::default()` if absent.
    pub fn on_seen(&mut self, channel: &str, seq: i64) {
        self.pending.entry((channel.to_string(), seq)).or_default();
    }

    /// A channel notification carrying this message was sent: the ChannelPush rung was
    /// executed, so `rungs_tried = max(1, rungs_tried)` and `last_ms = now_ms`; counts `pushed`
    /// and records an executed `channel_push` escalation.
    pub fn on_pushed(&mut self, channel: &str, seq: i64, now_ms: i64) {
        let entry = self.pending.entry((channel.to_string(), seq)).or_default();
        entry.attempt.rungs_tried = entry.attempt.rungs_tried.max(1);
        entry.attempt.last_ms = now_ms;
        self.pushed = self.pushed.saturating_add(1);
        record_escalation(
            &mut self.escalations,
            Escalation {
                channel: channel.to_string(),
                seq,
                mechanism: Mechanism::ChannelPush.as_str(),
                at_ms: now_ms,
                executed: true,
            },
        );
    }

    /// Drop every pending entry whose (channel, seq) is not in `keep`. Used after a backlog
    /// load: entries the server no longer reports as unacked are not delivered any more. Does
    /// not count as acked.
    pub fn retain_only(&mut self, keep: &BTreeSet<(String, i64)>) {
        self.pending.retain(|key, _| keep.contains(key));
    }

    /// Remove every pending entry of `channel` with seq <= `seq`; counts `acked`.
    pub fn on_acked(&mut self, channel: &str, seq: i64) {
        // Keys sort by channel then seq, so one channel's entries <= seq are a contiguous range.
        let doomed: Vec<(String, i64)> = self
            .pending
            .range((channel.to_string(), i64::MIN)..=(channel.to_string(), seq))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &doomed {
            self.pending.remove(key);
        }
        let removed = u64::try_from(doomed.len()).unwrap_or(u64::MAX);
        self.acked = self.acked.saturating_add(removed);
    }

    /// Apply `agora_deliver::next_action` to each pending entry. `Fire(Mechanism::ChannelPush)`
    /// advances `rungs_tried` / `last_ms` silently (nothing was sent, nothing is recorded).
    /// `Fire(m)` for any other rung records a passive `Escalation{executed: false}` and advances
    /// the same way; `Hold` does nothing; `Exhausted` marks the entry exhausted. Returns the
    /// escalations recorded by this call (never a `channel_push`).
    pub fn tick(&mut self, now_ms: i64) -> Vec<Escalation> {
        let mut fired = Vec::new();
        for ((channel, seq), entry) in self.pending.iter_mut() {
            match next_action(&self.ladder, entry.attempt, now_ms) {
                Action::Fire(m) => {
                    entry.attempt.rungs_tried += 1;
                    entry.attempt.last_ms = now_ms;
                    entry.exhausted = false;
                    if m != Mechanism::ChannelPush {
                        let escalation = Escalation {
                            channel: channel.clone(),
                            seq: *seq,
                            mechanism: m.as_str(),
                            at_ms: now_ms,
                            executed: false,
                        };
                        record_escalation(&mut self.escalations, escalation.clone());
                        fired.push(escalation);
                    }
                }
                Action::Hold { .. } => {}
                Action::Exhausted => entry.exhausted = true,
            }
        }
        fired
    }

    /// The earliest time `tick` next has work: 0 ("due now") for an entry that has not been
    /// tried yet, else `last_ms + patience` of the rung it is waiting on. `None` when nothing
    /// is waiting on a rung.
    pub fn next_due(&self) -> Option<i64> {
        self.pending
            .values()
            .filter(|entry| !entry.exhausted)
            .filter_map(|entry| {
                let tried = entry.attempt.rungs_tried;
                if tried == 0 {
                    // Rung 0 fires at the next tick, but only if the ladder has one.
                    return (!self.ladder.is_empty()).then_some(0);
                }
                if tried >= self.ladder.len() {
                    return None;
                }
                let prev = *self.ladder.get(tried - 1)?;
                Some(entry.attempt.last_ms.saturating_add(patience_ms(prev)))
            })
            .min()
    }

    /// Number of pending (seen, unacked) messages.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Count `n` notifications that were merged into a later batch.
    pub fn note_coalesced(&mut self, n: u64) {
        self.coalesced = self.coalesced.saturating_add(n);
    }

    /// `{"ladder":[mechanism strings],"push_enabled":b,"pending":[{channel,seq,rungs_tried,
    /// last_ms,exhausted}],"escalations":[last 50],"pushed":n,"coalesced":n,"acked":n}`.
    pub fn status_json(&self) -> Value {
        let pending: Vec<Value> = self
            .pending
            .iter()
            .map(|((channel, seq), entry)| {
                json!({
                    "channel": channel,
                    "seq": seq,
                    "rungs_tried": entry.attempt.rungs_tried,
                    "last_ms": entry.attempt.last_ms,
                    "exhausted": entry.exhausted,
                })
            })
            .collect();
        let skip = self.escalations.len().saturating_sub(STATUS_ESCALATIONS);
        let escalations: Vec<Value> = self
            .escalations
            .iter()
            .skip(skip)
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect();
        json!({
            "ladder": ladder_json(&self.ladder),
            "push_enabled": self.push_enabled,
            "pending": pending,
            "escalations": escalations,
            "pushed": self.pushed,
            "coalesced": self.coalesced,
            "acked": self.acked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::ladder_for;
    use agora_deliver::Harness;

    fn push_tracker() -> Tracker {
        Tracker::new(ladder_for(Harness::ClaudeCode, true), true)
    }

    #[test]
    fn seen_message_advances_silently_past_channel_push_at_the_next_tick() {
        let ladder = ladder_for(Harness::ClaudeCode, true);
        let patience = patience_ms(ladder[0]);
        let mut t = Tracker::new(ladder, true);
        t.on_seen("general", 1);
        assert_eq!(t.pending_len(), 1);
        assert_eq!(t.next_due(), Some(0));
        // Nothing was sent, so the ChannelPush rung is passed over without a record.
        assert!(t.tick(1_000).is_empty());
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["last_ms"], json!(1_000));
        assert_eq!(status["escalations"], json!([]));
        // The next rung is WaitTool, due one ChannelPush patience after the silent advance.
        assert_eq!(t.next_due(), Some(1_000 + patience));
    }

    #[test]
    fn a_pushed_message_records_an_executed_channel_push_escalation() {
        let mut t = push_tracker();
        t.on_seen("general", 4);
        t.on_pushed("general", 4, 77);
        let status = t.status_json();
        let shown = status["escalations"].as_array().expect("escalations array");
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0]["channel"], json!("general"));
        assert_eq!(shown[0]["seq"], json!(4));
        assert_eq!(shown[0]["mechanism"], json!("channel_push"));
        assert_eq!(shown[0]["executed"], json!(true));
        assert_eq!(shown[0]["at_ms"], json!(77));
        // Recording it does not make tick record one too.
        assert!(t.tick(0).is_empty());
    }

    #[test]
    fn on_pushed_escalations_respect_the_200_cap() {
        let mut t = push_tracker();
        for seq in 0..250 {
            t.on_pushed("general", seq, seq);
        }
        assert_eq!(t.escalations.len(), MAX_ESCALATIONS);
        let status = t.status_json();
        let shown = status["escalations"].as_array().expect("escalations array");
        assert_eq!(shown.len(), STATUS_ESCALATIONS);
        assert_eq!(shown[49]["seq"], json!(249));
    }

    #[test]
    fn retain_only_drops_pending_entries_outside_the_keep_set() {
        let mut t = push_tracker();
        t.on_seen("a", 1);
        t.on_seen("a", 2);
        t.on_seen("b", 1);
        let keep: BTreeSet<(String, i64)> = [("a".to_string(), 2), ("b".to_string(), 1)]
            .into_iter()
            .collect();
        t.retain_only(&keep);
        assert_eq!(t.pending_len(), 2);
        let status = t.status_json();
        let left: Vec<(String, i64)> = status["pending"]
            .as_array()
            .expect("pending array")
            .iter()
            .map(|p| {
                (
                    p["channel"].as_str().unwrap_or_default().to_string(),
                    p["seq"].as_i64().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(left, [("a".to_string(), 2), ("b".to_string(), 1)]);
        // Dropping is not acking.
        assert_eq!(status["acked"], json!(0));
        // An empty keep set empties the pending map.
        t.retain_only(&BTreeSet::new());
        assert_eq!(t.pending_len(), 0);
    }

    #[test]
    fn push_only_ladder_escalates_to_wait_tool_after_patience() {
        let ladder = ladder_for(Harness::ClaudeCode, true);
        let patience = patience_ms(ladder[0]);
        let mut t = push_tracker();
        t.on_seen("general", 7);
        t.on_pushed("general", 7, 0);
        assert_eq!(t.next_due(), Some(patience));
        assert!(t.tick(patience - 1).is_empty());
        let fired = t.tick(patience);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].mechanism, ladder[1].as_str());
        assert!(!fired[0].executed, "WaitTool is passive, never executed");
        assert_eq!(fired[0].at_ms, patience);
        // The walk is over: the entry is exhausted, nothing is due, the next tick is silent.
        assert_eq!(t.next_due(), None);
        assert!(t.tick(patience * 10).is_empty());
        let status = t.status_json();
        assert_eq!(status["pending"][0]["exhausted"], json!(true));
    }

    #[test]
    fn push_disabled_ladder_only_records_wait_tool() {
        let mut t = Tracker::new(ladder_for(Harness::ClaudeCode, false), false);
        t.on_seen("general", 1);
        let fired = t.tick(0);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].mechanism, "wait_tool");
        assert!(!fired[0].executed);
        assert!(t.tick(1_000_000).is_empty());
        assert_eq!(t.next_due(), None);
    }

    #[test]
    fn push_before_seen_and_push_counter() {
        let mut t = push_tracker();
        t.on_pushed("general", 3, 500);
        t.on_pushed("general", 3, 900);
        let status = t.status_json();
        assert_eq!(status["pushed"], json!(2));
        // Pushed entries were tried once; the second push keeps the max and moves last_ms.
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["last_ms"], json!(900));
    }

    #[test]
    fn on_acked_removes_only_that_channel_up_to_seq() {
        let mut t = push_tracker();
        t.on_seen("a", 1);
        t.on_seen("a", 2);
        t.on_seen("a", 3);
        t.on_seen("b", 1);
        t.on_seen("b", 5);
        t.on_acked("a", 2);
        assert_eq!(t.pending_len(), 3);
        let status = t.status_json();
        let left: Vec<(String, i64)> = status["pending"]
            .as_array()
            .expect("pending array")
            .iter()
            .map(|p| {
                (
                    p["channel"].as_str().unwrap_or_default().to_string(),
                    p["seq"].as_i64().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            left,
            [
                ("a".to_string(), 3),
                ("b".to_string(), 1),
                ("b".to_string(), 5)
            ]
        );
        assert_eq!(status["acked"], json!(2));
        // Acking past everything of a channel leaves the other channel alone.
        t.on_acked("b", i64::MAX);
        assert_eq!(t.pending_len(), 1);
        assert_eq!(t.status_json()["acked"], json!(4));
        // Acking an unknown channel removes nothing and counts nothing.
        t.on_acked("zzz", i64::MAX);
        assert_eq!(t.status_json()["acked"], json!(4));
    }

    #[test]
    fn escalations_are_capped_at_200_and_status_shows_the_last_50() {
        let mut t = Tracker::new(ladder_for(Harness::ClaudeCode, false), false);
        for seq in 0..250 {
            t.on_seen("general", seq);
        }
        let fired = t.tick(0);
        assert_eq!(fired.len(), 250);
        assert_eq!(t.escalations.len(), MAX_ESCALATIONS);
        let status = t.status_json();
        let shown = status["escalations"].as_array().expect("escalations array");
        assert_eq!(shown.len(), STATUS_ESCALATIONS);
        // Oldest first: the first shown is seq 200 (250 fired, last 50 are 200..250).
        assert_eq!(shown[0]["seq"], json!(200));
        assert_eq!(shown[49]["seq"], json!(249));
    }

    #[test]
    fn next_due_values() {
        let ladder = ladder_for(Harness::ClaudeCode, true);
        let patience = patience_ms(ladder[0]);
        let mut t = push_tracker();
        assert_eq!(t.next_due(), None);
        // An untried entry is due now.
        t.on_seen("general", 1);
        assert_eq!(t.next_due(), Some(0));
        // Once it was pushed, the earliest escalation is last_ms + patience of ChannelPush.
        t.on_pushed("general", 1, 100);
        assert_eq!(t.next_due(), Some(100 + patience));
        // Seq 2 is due for rung 0 (ChannelPush); the tick passes it silently at this time, so
        // its own next deadline is one ChannelPush patience later.
        t.on_seen("general", 2);
        t.tick(100 + patience - 1);
        assert_eq!(t.next_due(), Some(100 + patience));
        // A message pushed earlier than the current earliest deadline moves it back.
        t.on_pushed("general", 3, 50);
        assert_eq!(t.next_due(), Some(50 + patience));
    }

    #[test]
    fn note_coalesced_is_counted() {
        let mut t = push_tracker();
        t.note_coalesced(3);
        t.note_coalesced(2);
        assert_eq!(t.status_json()["coalesced"], json!(5));
    }

    #[test]
    fn reconfigure_keeps_pending_and_rederives_exhaustion() {
        let mut t = Tracker::new(ladder_for(Harness::ClaudeCode, false), false);
        t.on_seen("general", 1);
        t.tick(0);
        t.tick(0);
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
        t.reconfigure(ladder_for(Harness::ClaudeCode, true), true);
        let status = t.status_json();
        assert_eq!(status["push_enabled"], json!(true));
        assert_eq!(status["ladder"], json!(["channel_push", "wait_tool"]));
        assert_eq!(status["pending"].as_array().map(Vec::len), Some(1));
        assert_eq!(status["pending"][0]["exhausted"], json!(false));
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
    }

    #[test]
    fn status_json_has_the_documented_shape() {
        let mut t = push_tracker();
        t.on_seen("general", 1);
        t.on_pushed("general", 1, 0);
        t.tick(0);
        let status = t.status_json();
        let obj = status.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "acked",
                "coalesced",
                "escalations",
                "ladder",
                "pending",
                "push_enabled",
                "pushed"
            ]
        );
        assert_eq!(status["pending"][0]["channel"], json!("general"));
        assert_eq!(status["pending"][0]["seq"], json!(1));
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["last_ms"], json!(0));
        assert_eq!(status["escalations"][0]["mechanism"], json!("channel_push"));
        assert_eq!(status["escalations"][0]["executed"], json!(true));
        assert_eq!(status["ladder"], json!(["channel_push", "wait_tool"]));
    }
}
