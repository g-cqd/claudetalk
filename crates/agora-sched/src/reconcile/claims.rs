//! R7 claim expiry: a work task that stays `claimed` (no run request yet) for `claim_ttl_s`
//! is released back to the queue, and its claimer is told.

use super::{MAX_CLAIM_EXPIRIES_PER_PASS, Planner, ReconcileAction};

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

/// The action key of a claim's expiry: it carries the claim time, so a re-made claim is a new key.
fn expiry_key(claim: &ClaimSnap) -> String {
    format!("claim:{}:{}", claim.task, claim.claimed_ms)
}

impl Planner<'_> {
    /// R7 claim expiry: claims with `claimed_ms + (claim_ttl_s * 1000) <= now_ms`, oldest first (by `claimed_ms`,
    /// then task id), at most `MAX_CLAIM_EXPIRIES_PER_PASS` of them. Each one is a single `ExpireClaim` that carries
    /// its notice: `notice_key` "claim-notice:{task}:{claimed_ms}", target `agent`, kind "claim_expired", subject
    /// `task`, and text "claim on task {task} ({title}) expired after {ttl}s without a run request; it is back in the
    /// queue", with `ttl` the setting in whole seconds. The store posts the notice only when it releases the claim.
    /// Claims whose key is already marked are skipped before the cap, so they do not use up its slots. A non-finite
    /// or non-positive ttl turns the rule off: NaN would cast to 0 and make every claim due at once.
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
            .filter(|c| !self.marks.contains(expiry_key(c).as_str()))
            .collect();
        due.sort_by(|a, b| (a.claimed_ms, &a.task).cmp(&(b.claimed_ms, &b.task)));
        let ttl = format!("{ttl_s:.0}");
        for claim in due.into_iter().take(MAX_CLAIM_EXPIRIES_PER_PASS) {
            self.push(ReconcileAction::ExpireClaim {
                key: expiry_key(claim),
                task: claim.task.clone(),
                agent: claim.agent.clone(),
                claimed_ms: claim.claimed_ms,
                notice_key: format!("claim-notice:{}:{}", claim.task, claim.claimed_ms),
                text: format!(
                    "claim on task {} ({}) expired after {ttl}s without a run request; it is back in the queue",
                    claim.task, claim.title
                ),
            });
        }
    }
}
