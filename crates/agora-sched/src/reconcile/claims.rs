//! R7 claim expiry: a work task that stays `claimed` (no run request yet) for `claim_ttl_s`
//! is released back to the queue, and its claimer is told.

use super::{Planner, ReconcileAction};

/// A work task in state `claimed` with no run request yet, as the store reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimSnap {
    /// Task id.
    pub task: String,
    /// Hex public key of the claiming agent.
    pub agent: String,
    /// When the claim was made, ms since epoch.
    pub claimed_ms: i64,
    pub title: String,
}

impl Planner<'_> {
    /// R7 claim expiry: for claims sorted by task id with `claimed_ms + (claim_ttl_s * 1000) <= now_ms`,
    /// push `ExpireClaim { key: "claim:{task}:{claimed_ms}", task, agent, claimed_ms }` and then
    /// `Notice { key: "claim-notice:{task}:{claimed_ms}", target: agent, kind: "claim_expired",
    /// subject: task, text: "claim on task {task} ({title}) expired after {ttl}s without a run
    /// request; it is back in the queue" }`, with `ttl` the setting in whole seconds. A non-finite or
    /// non-positive ttl turns the rule off: NaN would cast to 0 and make every claim due at once.
    pub(super) fn expire_claims(&mut self) {
        let input = self.input;
        let ttl_s = input.settings.claim_ttl_s;
        if !ttl_s.is_finite() || ttl_s <= 0.0 {
            return;
        }
        let ttl_ms = (ttl_s * 1000.0) as i64;
        let mut due: Vec<&ClaimSnap> = input
            .claims
            .iter()
            .filter(|c| c.claimed_ms.saturating_add(ttl_ms) <= input.now_ms)
            .collect();
        due.sort_by(|a, b| a.task.cmp(&b.task));
        let ttl = format!("{ttl_s:.0}");
        for claim in due {
            self.push(ReconcileAction::ExpireClaim {
                key: format!("claim:{}:{}", claim.task, claim.claimed_ms),
                task: claim.task.clone(),
                agent: claim.agent.clone(),
                claimed_ms: claim.claimed_ms,
            });
            self.push(ReconcileAction::Notice {
                key: format!("claim-notice:{}:{}", claim.task, claim.claimed_ms),
                target: claim.agent.clone(),
                kind: "claim_expired".to_owned(),
                subject: claim.task.clone(),
                text: format!(
                    "claim on task {} ({}) expired after {ttl}s without a run request; it is back in the queue",
                    claim.task, claim.title
                ),
            });
        }
    }
}
