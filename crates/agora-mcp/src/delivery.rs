//! Delivery bookkeeping: which unacked messages were seen / pushed, and which rung of the
//! harness's delivery ladder fires next (`agora_deliver`).
//!
//! A `ChannelPush` escalation is recorded only when a push was actually sent (`on_pushed`); when
//! its rung comes due with nothing sent, the tracker advances past it silently. Every other fired
//! rung is passive (`executed: false`): the tracker never delivers, the hooks and the mod do.
//! M4 pull rungs (`ModSubmit`, `AsyncRewakeHook`, `StopContinue`, `HookInject`) are recorded with
//! `armed` set from the session's heartbeats. An armed rung waits its patience for an ack; an
//! unarmed one has no fresh heartbeat, so it is recorded and the next rung is tried in the same
//! tick. `WaitTool` is recorded armed and waits its patience; `OwnedSessionApi` is recorded
//! unarmed and passed over at once.

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
    /// The rung's heartbeat was fresh when it fired (`wait_tool` and `channel_push` are always
    /// armed; `owned_session_api` never is), so the tracker waited its patience for an ack.
    pub armed: bool,
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
    /// Heartbeats of the pull rungs (ms), set by `set_heartbeats` and shown by `status_json`.
    heartbeats: BTreeMap<Mechanism, i64>,
    /// Rungs that were armed at the last `tick_with`.
    armed: BTreeSet<Mechanism>,
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
            heartbeats: BTreeMap::new(),
            armed: BTreeSet::new(),
        }
    }

    /// Replace the ladder / push flag. Each pending entry's `rungs_tried` is rebased by rung
    /// identity: it becomes the count of the new ladder's leading rungs that the old ladder had
    /// already tried, so a rung inserted ahead of the others is still due. `last_ms` is kept and
    /// `exhausted` is cleared; the next `tick` re-derives exhaustion.
    pub fn reconfigure(&mut self, ladder: Vec<Mechanism>, push_enabled: bool) {
        let old = std::mem::replace(&mut self.ladder, ladder);
        self.push_enabled = push_enabled;
        for entry in self.pending.values_mut() {
            let n = entry.attempt.rungs_tried.min(old.len());
            let tried = old.get(..n).unwrap_or_default();
            entry.attempt.rungs_tried =
                self.ladder.iter().take_while(|m| tried.contains(m)).count();
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
                armed: true,
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

    /// Advance every pending entry with no pull rung armed: `tick_with(now_ms, &BTreeSet::new())`.
    /// The M3 ladder `[ChannelPush, WaitTool]` behaves as it did before M4. Returns the
    /// escalations recorded by this call (never a `channel_push`).
    pub fn tick(&mut self, now_ms: i64) -> Vec<Escalation> {
        self.tick_with(now_ms, &BTreeSet::new())
    }

    /// Advance every pending entry along the ladder, given which rungs are `armed`. Per entry,
    /// at most `ladder.len() + 1` decisions of `agora_deliver::next_action`:
    /// - `Fire(ChannelPush)`: rungs_tried + 1, last_ms = now, nothing recorded; stop for the entry.
    /// - `Fire(WaitTool)`: record `{executed: false, armed: true}`; rungs_tried + 1, last_ms = now;
    ///   stop for the entry.
    /// - `Fire(OwnedSessionApi)`: record `{executed: false, armed: false}`; rungs_tried + 1, then
    ///   passed over (below).
    /// - `Fire(m)` for a pull rung: record `{executed: false, armed: armed.contains(m)}`;
    ///   rungs_tried + 1. Armed: last_ms = now and stop for the entry (the rung's patience is
    ///   spent waiting for an ack). Unarmed: passed over (below).
    ///
    /// Passed over: last_ms = now - patience_ms(m), so the next rung is due at this same `now`
    /// and the loop continues in the same tick.
    /// `Hold`: stop. `Exhausted`: mark the entry exhausted and stop; any fire clears `exhausted`,
    /// and an entry whose `rungs_tried` reaches the ladder length is marked exhausted at once.
    ///
    /// Stores `armed` for `status_json`. Returns the escalations recorded by this call.
    pub fn tick_with(&mut self, now_ms: i64, armed: &BTreeSet<Mechanism>) -> Vec<Escalation> {
        self.armed = armed.clone();
        let bound = self.ladder.len() + 1;
        let ladder = &self.ladder;
        let mut fired = Vec::new();
        for ((channel, seq), entry) in self.pending.iter_mut() {
            for _ in 0..bound {
                let m = match next_action(ladder, entry.attempt, now_ms) {
                    Action::Fire(m) => m,
                    Action::Hold { .. } => break,
                    Action::Exhausted => {
                        entry.exhausted = true;
                        break;
                    }
                };
                entry.attempt.rungs_tried += 1;
                entry.exhausted = false;
                // Whether the entry waits its patience on this rung, and the armed flag of the
                // record (`None`: nothing recorded, for a silent ChannelPush).
                let (waits, recorded) = match m {
                    Mechanism::ChannelPush => (true, None),
                    Mechanism::WaitTool => (true, Some(true)),
                    Mechanism::OwnedSessionApi => (false, Some(false)),
                    // The four pull rungs.
                    _ => {
                        let is_armed = armed.contains(&m);
                        (is_armed, Some(is_armed))
                    }
                };
                if let Some(is_armed) = recorded {
                    let escalation = Escalation {
                        channel: channel.clone(),
                        seq: *seq,
                        mechanism: m.as_str(),
                        at_ms: now_ms,
                        executed: false,
                        armed: is_armed,
                    };
                    record_escalation(&mut self.escalations, escalation.clone());
                    fired.push(escalation);
                }
                if waits {
                    entry.attempt.last_ms = now_ms;
                    break;
                }
                entry.attempt.last_ms = now_ms.saturating_sub(patience_ms(m));
            }
            // Eager: an entry that has used every rung is exhausted now, not at the next tick.
            if entry.attempt.rungs_tried >= ladder.len() {
                entry.exhausted = true;
            }
        }
        fired
    }

    /// Store the pull-rung heartbeats (ms timestamps by mechanism) for `status_json`.
    pub fn set_heartbeats(&mut self, beats: BTreeMap<Mechanism, i64>) {
        self.heartbeats = beats;
    }

    /// Store the heartbeats and the armed rungs for `status_json` without advancing any entry,
    /// so an idle session shows live values.
    pub fn refresh(&mut self, beats: BTreeMap<Mechanism, i64>, armed: BTreeSet<Mechanism>) {
        self.heartbeats = beats;
        self.armed = armed;
    }

    /// The ladder in use.
    pub fn ladder(&self) -> &[Mechanism] {
        &self.ladder
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
    /// last_ms,exhausted}],"escalations":[last 50],"pushed":n,"coalesced":n,"acked":n,
    /// "armed":[mechanism strings of the last tick_with],"heartbeats":{"<mechanism>": ms}}`.
    /// A pending entry's `last_ms` may be synthetic: after an unarmed rung was passed over it is
    /// `now - patience_ms(rung)` so the next rung is due at once, not the time of an event.
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
            "armed": self.armed.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
            "heartbeats": self
                .heartbeats
                .iter()
                .map(|(m, t)| (m.as_str().to_owned(), json!(t)))
                .collect::<serde_json::Map<String, Value>>(),
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
        assert_eq!(shown[0]["armed"], json!(true));
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
        // The old ladder's WaitTool was tried, but the new ladder's ChannelPush (first rung) was
        // not, so nothing is counted as tried and ChannelPush is due again.
        assert_eq!(status["pending"][0]["rungs_tried"], json!(0));
    }

    #[test]
    fn reconfigure_inserted_rung_is_still_due_after_a_walk() {
        let mut t = Tracker::new(vec![Mechanism::ChannelPush, Mechanism::WaitTool], true);
        t.on_seen("general", 1);
        t.on_pushed("general", 1, 0);
        let wait_at = patience_ms(Mechanism::ChannelPush);
        t.tick_with(wait_at, &BTreeSet::new());
        t.tick_with(wait_at + patience_ms(Mechanism::WaitTool), &BTreeSet::new());
        assert_eq!(t.status_json()["pending"][0]["rungs_tried"], json!(2));
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));

        t.reconfigure(
            vec![
                Mechanism::ChannelPush,
                Mechanism::ModSubmit,
                Mechanism::WaitTool,
            ],
            true,
        );
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["exhausted"], json!(false));
        assert_eq!(status["pending"][0]["last_ms"], json!(wait_at));

        // Nothing is armed and the patience is long past: ModSubmit is passed over, then
        // WaitTool fires; each exactly once, then the ladder is used up.
        let now = 1_000_000;
        let fired = t.tick_with(now, &BTreeSet::new());
        assert_eq!(
            summary(&fired),
            [("mod_submit", false), ("wait_tool", true)]
        );
        assert_eq!(t.status_json()["pending"][0]["rungs_tried"], json!(3));
        assert!(t.tick_with(now * 10, &BTreeSet::new()).is_empty());
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
    }

    #[test]
    fn reconfigure_to_a_shorter_ladder_keeps_rungs_tried_within_its_length() {
        let mut t = Tracker::new(
            vec![
                Mechanism::ChannelPush,
                Mechanism::ModSubmit,
                Mechanism::WaitTool,
            ],
            true,
        );
        t.pending.insert(
            ("general".to_string(), 1),
            Entry {
                attempt: Attempt {
                    rungs_tried: 3,
                    last_ms: 5,
                },
                exhausted: true,
            },
        );
        t.reconfigure(vec![Mechanism::ChannelPush], true);
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["last_ms"], json!(5));
        assert_eq!(status["pending"][0]["exhausted"], json!(false));
        // The single rung was tried: the next tick marks the entry exhausted and records nothing.
        assert!(t.tick_with(1_000_000, &BTreeSet::new()).is_empty());
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
    }

    #[test]
    fn reconfigure_drops_a_removed_rung_and_fires_the_next_one() {
        let mut t = Tracker::new(
            vec![
                Mechanism::ModSubmit,
                Mechanism::StopContinue,
                Mechanism::WaitTool,
            ],
            true,
        );
        t.pending.insert(
            ("general".to_string(), 1),
            Entry {
                attempt: Attempt {
                    rungs_tried: 2,
                    last_ms: 0,
                },
                exhausted: false,
            },
        );
        // ModSubmit is gone; StopContinue was tried, WaitTool was not, so one rung counts as tried.
        t.reconfigure(vec![Mechanism::StopContinue, Mechanism::WaitTool], true);
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["exhausted"], json!(false));
        // WaitTool is due (StopContinue's patience has long passed) and fires once, then the
        // ladder is used up.
        let fired = t.tick_with(1_000_000, &BTreeSet::new());
        assert_eq!(summary(&fired), [("wait_tool", true)]);
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
        assert!(t.tick_with(2_000_000, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_tick_that_uses_every_rung_marks_the_entry_exhausted_at_once() {
        let mut t = Tracker::new(vec![Mechanism::StopContinue, Mechanism::WaitTool], true);
        t.on_seen("general", 1);
        let fired = t.tick_with(0, &BTreeSet::new());
        assert_eq!(
            summary(&fired),
            [("stop_continue", false), ("wait_tool", true)]
        );
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(2));
        assert_eq!(status["pending"][0]["exhausted"], json!(true));
        assert_eq!(t.next_due(), None);
    }

    #[test]
    fn refresh_stores_beats_and_armed_without_advancing_entries() {
        let mut t = pushed_tracker(full_pull_ladder(), 0);
        assert_eq!(t.ladder(), full_pull_ladder().as_slice());
        t.refresh(
            BTreeMap::from([(Mechanism::ModSubmit, 42)]),
            armed_set(&[Mechanism::ModSubmit]),
        );
        let status = t.status_json();
        assert_eq!(status["armed"], json!(["mod_submit"]));
        assert_eq!(status["heartbeats"], json!({"mod_submit": 42}));
        // No entry moved and nothing was recorded.
        assert_eq!(status["pending"][0]["rungs_tried"], json!(1));
        assert_eq!(status["pending"][0]["last_ms"], json!(0));
        assert_eq!(status["escalations"].as_array().map(Vec::len), Some(1));
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
                "armed",
                "coalesced",
                "escalations",
                "heartbeats",
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

    /// The M4 Claude Code ladder: every pull rung, then the wait tool.
    fn full_pull_ladder() -> Vec<Mechanism> {
        vec![
            Mechanism::ChannelPush,
            Mechanism::ModSubmit,
            Mechanism::AsyncRewakeHook,
            Mechanism::StopContinue,
            Mechanism::HookInject,
            Mechanism::WaitTool,
        ]
    }

    /// A tracker over `ladder` with one message seen and pushed at `pushed_ms`, so its
    /// ChannelPush rung is spent and it is due for the next rung at `pushed_ms + 15_000`.
    fn pushed_tracker(ladder: Vec<Mechanism>, pushed_ms: i64) -> Tracker {
        let mut t = Tracker::new(ladder, true);
        t.on_seen("general", 1);
        t.on_pushed("general", 1, pushed_ms);
        t
    }

    fn armed_set(ms: &[Mechanism]) -> BTreeSet<Mechanism> {
        ms.iter().copied().collect()
    }

    /// `(mechanism, armed)` of each escalation, in order.
    fn summary(fired: &[Escalation]) -> Vec<(&'static str, bool)> {
        fired.iter().map(|e| (e.mechanism, e.armed)).collect()
    }

    #[test]
    fn unarmed_pull_rungs_all_fire_in_one_tick_and_wait_tool_is_last() {
        let mut t = pushed_tracker(full_pull_ladder(), 0);
        let fired = t.tick_with(15_000, &BTreeSet::new());
        assert_eq!(
            summary(&fired),
            [
                ("mod_submit", false),
                ("async_rewake_hook", false),
                ("stop_continue", false),
                ("hook_inject", false),
                ("wait_tool", true),
            ]
        );
        assert!(
            fired
                .iter()
                .all(|e| !e.executed && e.at_ms == 15_000 && e.seq == 1 && e.channel == "general")
        );
        let status = t.status_json();
        assert_eq!(status["pending"][0]["rungs_tried"], json!(6));
        assert_eq!(status["pending"][0]["last_ms"], json!(15_000));
        // The ladder is used up by this very tick, so the entry is exhausted at once.
        assert_eq!(status["pending"][0]["exhausted"], json!(true));
        assert_eq!(status["armed"], json!([]));
        // The next tick records nothing.
        assert!(t.tick_with(30_000, &BTreeSet::new()).is_empty());
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
        assert_eq!(t.next_due(), None);
    }

    #[test]
    fn armed_async_rewake_hook_stops_the_chain_and_waits_its_patience() {
        let mut t = pushed_tracker(full_pull_ladder(), 0);
        let armed = armed_set(&[Mechanism::AsyncRewakeHook]);
        let fired = t.tick_with(15_000, &armed);
        assert_eq!(
            summary(&fired),
            [("mod_submit", false), ("async_rewake_hook", true)]
        );
        assert_eq!(t.status_json()["pending"][0]["rungs_tried"], json!(3));
        // Waiting for the rewake hook's ack: nothing is due before its 15 s patience ends.
        assert!(t.tick_with(29_999, &armed).is_empty());
        assert_eq!(t.next_due(), Some(30_000));
        // At +15 s the chain goes on: StopContinue and HookInject are unarmed, WaitTool ends it.
        let fired = t.tick_with(30_000, &armed);
        assert_eq!(
            summary(&fired),
            [
                ("stop_continue", false),
                ("hook_inject", false),
                ("wait_tool", true)
            ]
        );
        assert_eq!(t.status_json()["armed"], json!(["async_rewake_hook"]));
    }

    #[test]
    fn armed_stop_continue_waits_sixty_seconds() {
        let mut t = pushed_tracker(full_pull_ladder(), 0);
        let armed = armed_set(&[Mechanism::StopContinue]);
        let fired = t.tick_with(15_000, &armed);
        assert_eq!(
            summary(&fired),
            [
                ("mod_submit", false),
                ("async_rewake_hook", false),
                ("stop_continue", true)
            ]
        );
        assert!(t.tick_with(74_999, &armed).is_empty());
        let fired = t.tick_with(75_000, &armed);
        assert_eq!(
            summary(&fired),
            [("hook_inject", false), ("wait_tool", true)]
        );
    }

    #[test]
    fn tick_is_tick_with_nothing_armed() {
        let mut a = pushed_tracker(full_pull_ladder(), 0);
        let mut b = pushed_tracker(full_pull_ladder(), 0);
        let fired_a = a.tick(15_000);
        let fired_b = b.tick_with(15_000, &BTreeSet::new());
        assert_eq!(summary(&fired_a), summary(&fired_b));
        assert_eq!(a.status_json(), b.status_json());
    }

    #[test]
    fn status_json_shows_armed_rungs_and_heartbeats() {
        let mut t = pushed_tracker(full_pull_ladder(), 0);
        assert_eq!(t.status_json()["armed"], json!([]));
        assert_eq!(t.status_json()["heartbeats"], json!({}));
        t.set_heartbeats(BTreeMap::from([
            (Mechanism::ModSubmit, 42),
            (Mechanism::AsyncRewakeHook, 43),
        ]));
        t.tick_with(15_000, &armed_set(&[Mechanism::AsyncRewakeHook]));
        let status = t.status_json();
        assert_eq!(status["armed"], json!(["async_rewake_hook"]));
        assert_eq!(
            status["heartbeats"],
            json!({"mod_submit": 42, "async_rewake_hook": 43})
        );
    }

    #[test]
    fn pull_only_ladder_with_nothing_armed_terminates_in_one_tick() {
        let ladder = vec![
            Mechanism::ModSubmit,
            Mechanism::AsyncRewakeHook,
            Mechanism::StopContinue,
            Mechanism::HookInject,
        ];
        let mut t = Tracker::new(ladder, false);
        t.on_seen("general", 1);
        // Rung 0 fires at once; each later rung is due at once; then the ladder is exhausted.
        let fired = t.tick_with(0, &BTreeSet::new());
        assert_eq!(
            summary(&fired),
            [
                ("mod_submit", false),
                ("async_rewake_hook", false),
                ("stop_continue", false),
                ("hook_inject", false),
            ]
        );
        assert_eq!(t.status_json()["pending"][0]["exhausted"], json!(true));
        assert!(t.tick_with(1_000_000, &BTreeSet::new()).is_empty());
        assert_eq!(t.next_due(), None);
    }

    #[test]
    fn owned_session_api_is_recorded_unarmed_and_passed_over() {
        let mut t = Tracker::new(vec![Mechanism::OwnedSessionApi, Mechanism::WaitTool], false);
        t.on_seen("general", 1);
        // Even when listed as armed, the owned session never waits: WaitTool is due at once.
        let fired = t.tick_with(0, &armed_set(&[Mechanism::OwnedSessionApi]));
        assert_eq!(
            summary(&fired),
            [("owned_session_api", false), ("wait_tool", true)]
        );
    }

    #[test]
    fn next_due_is_now_after_an_unarmed_pull_rung() {
        let ladder = vec![
            Mechanism::ChannelPush,
            Mechanism::ModSubmit,
            Mechanism::WaitTool,
        ];
        let mut t = Tracker::new(ladder, true);
        // ModSubmit was tried at 1_000 and passed over: its last_ms is now minus its patience,
        // so the rung after it (WaitTool) is due at 1_000 itself.
        t.pending.insert(
            ("general".to_string(), 1),
            Entry {
                attempt: Attempt {
                    rungs_tried: 2,
                    last_ms: 1_000 - patience_ms(Mechanism::ModSubmit),
                },
                exhausted: false,
            },
        );
        assert_eq!(t.next_due(), Some(1_000));
        let fired = t.tick_with(1_000, &BTreeSet::new());
        assert_eq!(summary(&fired), [("wait_tool", true)]);
        // WaitTool was the last rung: nothing is due after it.
        assert_eq!(t.next_due(), None);
    }
}
