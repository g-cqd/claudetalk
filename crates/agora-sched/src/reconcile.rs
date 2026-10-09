//! Pure reconciler planning (D12, SPEC section 6): a snapshot in, idempotent rate-limited actions out.

use std::collections::{BTreeMap, BTreeSet};

use crate::admit::MachineSnapshot;
use crate::model::{LeaseKind, LeaseRecord, PolicyRecord};
use crate::policy::{EffectivePolicy, TeamPolicy, effective};
use crate::settings::WorkSettings;
use crate::textfmt::span;

mod claims;
mod floors;
#[cfg(test)]
mod tests;

pub use claims::ClaimSnap;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CardSnap {
    pub id: String,
    pub title: String,
    pub team: String,
    pub lane: String,
    pub state: String,
    pub owner: Option<String>,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SprintSnap {
    pub id: String,
    pub name: String,
    pub ends_ms: i64,
    pub state: String,
    pub open_cards: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Mark {
    pub key: String,
    pub last_ms: i64,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileInput {
    pub now_ms: i64,
    pub settings: WorkSettings,
    pub machine: Option<MachineSnapshot>,
    pub cards: Vec<CardSnap>,
    pub leases: Vec<LeaseRecord>,
    pub policies: Vec<PolicyRecord>,
    pub require_review_default: bool,
    pub marks: Vec<Mark>,
    /// hex pks with role >= supervisor (not revoked, not system), sorted.
    pub supervisors: Vec<String>,
    pub admission_paused: Option<String>,
    /// id of the open (not cancelled) blocker card created by the reconciler, if any.
    pub blocker_card: Option<String>,
    pub sprints: Vec<SprintSnap>,
    /// Work tasks in state `claimed` with no run request yet, oldest first (R7 claim expiry).
    pub claims: Vec<ClaimSnap>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReconcileAction {
    ExpireLease {
        key: String,
        lease: String,
    },
    /// R7: a claim that sat without a run request for `claim_ttl_s`; the task goes back to the queue. Its notice
    /// (kind "claim_expired", target `agent`, subject `task`) is posted only if the claim is released.
    ExpireClaim {
        key: String,
        task: String,
        agent: String,
        claimed_ms: i64,
        #[serde(default)]
        notice_key: String,
        #[serde(default)]
        text: String,
    },
    CloseQuiet {
        key: String,
        lease: String,
    },
    Notice {
        key: String,
        target: String,
        kind: String,
        subject: String,
        text: String,
    },
    ReclaimCard {
        key: String,
        card: String,
        owner: String,
    },
    Blocker {
        key: String,
        open: bool,
        title: String,
        owner_action: String,
    },
    Admission {
        key: String,
        paused: Option<String>,
    },
}

impl ReconcileAction {
    pub fn key(&self) -> &str {
        match self {
            ReconcileAction::ExpireLease { key, .. }
            | ReconcileAction::ExpireClaim { key, .. }
            | ReconcileAction::CloseQuiet { key, .. }
            | ReconcileAction::Notice { key, .. }
            | ReconcileAction::ReclaimCard { key, .. }
            | ReconcileAction::Blocker { key, .. }
            | ReconcileAction::Admission { key, .. } => key,
        }
    }

    /// system op kind: reconcile.expire | reconcile.quiet_close | reconcile.notice | reconcile.reclaim |
    /// reconcile.blocker | reconcile.admission | reconcile.claim_expire
    pub fn op_kind(&self) -> &'static str {
        match self {
            ReconcileAction::ExpireLease { .. } => "reconcile.expire",
            ReconcileAction::ExpireClaim { .. } => "reconcile.claim_expire",
            ReconcileAction::CloseQuiet { .. } => "reconcile.quiet_close",
            ReconcileAction::Notice { .. } => "reconcile.notice",
            ReconcileAction::ReclaimCard { .. } => "reconcile.reclaim",
            ReconcileAction::Blocker { .. } => "reconcile.blocker",
            ReconcileAction::Admission { .. } => "reconcile.admission",
        }
    }
}

pub const MAX_ACTIONS_PER_PASS: usize = 64;
/// Claim expiries per pass. Bounds the burst of releases and notices after a long daemon pause; the rest are
/// picked up on later passes.
pub const MAX_CLAIM_EXPIRIES_PER_PASS: usize = 16;

/// Owner action for a set of floor breaches (R5): one "free <resource> (<value> < floor <floor>)" clause per
/// breach, joined with "; ", then the pause that lasts until they are gone.
fn floor_owner_action(breaches: &[floors::Breach]) -> String {
    let clauses: Vec<String> = breaches
        .iter()
        .map(|b| format!("free {} ({} < floor {})", b.resource, b.value, b.floor))
        .collect();
    format!("{} — admission is paused until then", clauses.join("; "))
}

/// Plans one reconciler pass: the two expiries first (R1 leases, then R7 claims), then rules R2 to R6 in
/// order, each sorted internally, actions whose key is already marked or already emitted in this pass
/// dropped, and the whole pass capped at `MAX_ACTIONS_PER_PASS`.
pub fn plan(input: &ReconcileInput) -> Vec<ReconcileAction> {
    let mut planner = Planner::new(input);
    planner.expire_leases();
    planner.expire_claims();
    planner.close_quiet_windows();
    planner.stale_cards();
    planner.wip_limits();
    planner.machine_floors();
    planner.sprint_rollovers();
    planner.out
}

/// Pseudonym of a hex public key inside texts: its first eight characters.
fn pseudonym(pk: &str) -> &str {
    pk.get(..8).unwrap_or(pk)
}

/// Working state of one pass: the input, its marks, policies and supervisors in a fixed order, and the
/// actions emitted so far.
struct Planner<'a> {
    input: &'a ReconcileInput,
    marks: BTreeSet<&'a str>,
    /// Keys of the actions emitted so far; a second action with the same key is refused.
    emitted: BTreeSet<String>,
    /// Keyed by (team, lane). The store keeps one row per pair; should duplicates appear, the most
    /// recently updated one wins.
    policies: BTreeMap<(&'a str, &'a str), &'a TeamPolicy>,
    /// Sorted and de-duplicated, so notices go out in a fixed order.
    supervisors: Vec<&'a str>,
    out: Vec<ReconcileAction>,
}

impl<'a> Planner<'a> {
    fn new(input: &'a ReconcileInput) -> Self {
        let marks = input.marks.iter().map(|m| m.key.as_str()).collect();
        let mut by_layer: Vec<&'a PolicyRecord> = input.policies.iter().collect();
        by_layer.sort_by(|a, b| {
            (&a.team, &a.lane, a.updated_ms, &a.by).cmp(&(&b.team, &b.lane, b.updated_ms, &b.by))
        });
        let policies = by_layer
            .into_iter()
            .map(|p| ((p.team.as_str(), p.lane.as_str()), &p.policy))
            .collect();
        let mut supervisors: Vec<&'a str> = input.supervisors.iter().map(String::as_str).collect();
        supervisors.sort_unstable();
        supervisors.dedup();
        Planner {
            input,
            marks,
            emitted: BTreeSet::new(),
            policies,
            supervisors,
            out: Vec::new(),
        }
    }

    /// Emits an action unless its key is already marked, already emitted in this pass, or the pass is full.
    fn push(&mut self, action: ReconcileAction) {
        if self.out.len() >= MAX_ACTIONS_PER_PASS || self.marks.contains(action.key()) {
            return;
        }
        if self.emitted.insert(action.key().to_owned()) {
            self.out.push(action);
        }
    }

    /// Effective policy for a (team, lane): the four layers, most general first, missing layers skipped.
    fn effective_for(&self, team: &str, lane: &str) -> EffectivePolicy {
        let keys = [("*", "*"), (team, "*"), ("*", lane), (team, lane)];
        let layers: Vec<&TeamPolicy> = keys
            .into_iter()
            .filter_map(|k| self.policies.get(&k).copied())
            .collect();
        effective(&layers, self.input.require_review_default)
    }

    /// Global effective policy: the ("*", "*") layer only (R6).
    fn global_policy(&self) -> EffectivePolicy {
        let layers: Vec<&TeamPolicy> = self
            .policies
            .get(&("*", "*"))
            .copied()
            .into_iter()
            .collect();
        effective(&layers, self.input.require_review_default)
    }

    /// R1: path and pool leases whose expiry has passed. The key carries the deadline (O-36), so a lease
    /// renewed after an expiry was marked is expired again at its new deadline.
    fn expire_leases(&mut self) {
        let input = self.input;
        let mut due: Vec<&LeaseRecord> = input
            .leases
            .iter()
            .filter(|l| {
                matches!(l.kind, LeaseKind::Path | LeaseKind::Pool)
                    && l.released_ms.is_none()
                    && l.expires_ms.is_some_and(|at| at <= input.now_ms)
            })
            .collect();
        due.sort_by(|a, b| a.id.cmp(&b.id));
        for lease in due {
            let expires = lease.expires_ms.unwrap_or_default();
            self.push(ReconcileAction::ExpireLease {
                key: format!("expire:{}:{expires}", lease.id),
                lease: lease.id.clone(),
            });
        }
    }

    /// R2: quiet windows still open more than `quiet_max` seconds after their expected end. The key carries
    /// the expected end (O-36), as R1's does.
    fn close_quiet_windows(&mut self) {
        let input = self.input;
        let grace_ms = input.settings.quiet_max * 1000.0;
        let now = input.now_ms as f64;
        let mut due: Vec<&LeaseRecord> = input
            .leases
            .iter()
            .filter(|l| {
                l.kind == LeaseKind::Quiet
                    && l.released_ms.is_none()
                    && l.expected_end_ms
                        .is_some_and(|end| end as f64 + grace_ms <= now)
            })
            .collect();
        due.sort_by(|a, b| a.id.cmp(&b.id));
        for lease in due {
            let end = lease.expected_end_ms.unwrap_or_default();
            self.push(ReconcileAction::CloseQuiet {
                key: format!("quiet:{}:{end}", lease.id),
                lease: lease.id.clone(),
            });
        }
    }

    /// R3: claimed or doing cards with an owner that have been idle past the stale, escalation and reclaim
    /// thresholds of their effective policy.
    fn stale_cards(&mut self) {
        let input = self.input;
        let mut owned: Vec<(&CardSnap, &str)> = input
            .cards
            .iter()
            .filter(|c| matches!(c.state.as_str(), "claimed" | "doing"))
            .filter_map(|c| c.owner.as_deref().map(|o| (c, o)))
            .collect();
        owned.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        for (card, owner) in owned {
            let eff = self.effective_for(&card.team, &card.lane);
            let idle = input.now_ms.saturating_sub(card.updated_ms) as f64;
            let idle_text = span(Some(idle / 1000.0));
            if idle >= eff.stale_after_s * 1000.0 {
                self.push(ReconcileAction::Notice {
                    key: format!("stale1:{}:{}", card.id, card.updated_ms),
                    target: owner.to_owned(),
                    kind: "stale".to_owned(),
                    subject: card.id.clone(),
                    text: format!(
                        "card {} ({}) has been {} for {} without activity; move it, add evidence, or release it",
                        card.id, card.title, card.state, idle_text
                    ),
                });
            }
            if idle >= (eff.stale_after_s + eff.escalate_after_s) * 1000.0 {
                let mut seen = BTreeSet::new();
                let chain: Vec<&str> = eff
                    .escalation_chain
                    .iter()
                    .map(String::as_str)
                    .filter(|t| seen.insert(*t))
                    .collect();
                let targets = if chain.is_empty() {
                    self.supervisors.clone()
                } else {
                    chain
                };
                let tail = if eff.reclaim {
                    format!(
                        "it will be reclaimed after {}",
                        span(Some(eff.reclaim_after_s))
                    )
                } else {
                    "reclaim is off for this lane".to_owned()
                };
                for target in targets {
                    self.push(ReconcileAction::Notice {
                        key: format!("stale2:{}:{}:{}", card.id, card.updated_ms, target),
                        target: target.to_owned(),
                        kind: "escalation".to_owned(),
                        subject: card.id.clone(),
                        text: format!(
                            "card {} ({}) owned by {} has been {} for {}; {}",
                            card.id,
                            card.title,
                            pseudonym(owner),
                            card.state,
                            idle_text,
                            tail
                        ),
                    });
                }
            }
            if eff.reclaim && idle >= eff.reclaim_after_s * 1000.0 {
                self.push(ReconcileAction::Notice {
                    key: format!("stale3:{}:{}", card.id, card.updated_ms),
                    target: owner.to_owned(),
                    kind: "reclaimed".to_owned(),
                    subject: card.id.clone(),
                    text: format!(
                        "card {} ({}) was reclaimed after {} without activity; it is ready again and no longer yours",
                        card.id, card.title, idle_text
                    ),
                });
                self.push(ReconcileAction::ReclaimCard {
                    key: format!("reclaim:{}:{}", card.id, card.updated_ms),
                    card: card.id.clone(),
                    owner: owner.to_owned(),
                });
            }
        }
    }

    /// R4: lanes whose claimed and doing cards exceed their WIP limit, notified to every supervisor.
    fn wip_limits(&mut self) {
        let input = self.input;
        let lanes: BTreeSet<(&str, &str)> = input
            .cards
            .iter()
            .map(|c| (c.team.as_str(), c.lane.as_str()))
            .collect();
        for (team, lane) in lanes {
            let Some(limit) = self.effective_for(team, lane).wip_limit else {
                continue;
            };
            let count = input
                .cards
                .iter()
                .filter(|c| {
                    c.team == team
                        && c.lane == lane
                        && matches!(c.state.as_str(), "claimed" | "doing")
                })
                .count();
            if count <= usize::try_from(limit).unwrap_or(usize::MAX) {
                continue;
            }
            let bucket = input.now_ms / 3_600_000;
            for target in self.supervisors.clone() {
                self.push(ReconcileAction::Notice {
                    key: format!("wip:{team}/{lane}:{count}:{bucket}:{target}"),
                    target: target.to_owned(),
                    kind: "wip".to_owned(),
                    subject: format!("{team}/{lane}"),
                    text: format!(
                        "lane {team}/{lane} has {count} cards in progress, above its WIP limit of {limit}"
                    ),
                });
            }
        }
    }

    /// R5: machine floors. Pauses admission and opens a blocker on a breach. Without a breach it resumes
    /// admission if paused, and closes the blocker card whenever one is open, whether or not admission was
    /// paused. Nothing happens without a machine snapshot.
    fn machine_floors(&mut self) {
        let input = self.input;
        if input.machine.is_none() {
            return;
        }
        let breaches = floors::breaches(input);
        let bucket = input.now_ms / 600_000;
        if breaches.is_empty() {
            if input.admission_paused.is_some() {
                self.push(ReconcileAction::Admission {
                    key: format!("resume:{bucket}"),
                    paused: None,
                });
            }
            if let Some(card) = &input.blocker_card {
                self.push(ReconcileAction::Blocker {
                    key: format!("unblock:{card}"),
                    open: false,
                    title: String::new(),
                    owner_action: String::new(),
                });
            }
            return;
        }
        let text = breaches
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if input.admission_paused.is_none() {
            self.push(ReconcileAction::Admission {
                key: format!("pause:{bucket}"),
                paused: Some(text.clone()),
            });
        }
        self.push(ReconcileAction::Blocker {
            key: format!("blocker:{bucket}"),
            open: true,
            title: format!("Machine floor breached: {text}"),
            owner_action: floor_owner_action(&breaches),
        });
    }

    /// R6: active sprints past their end, notified to every supervisor when the global policy rolls
    /// sprints over.
    fn sprint_rollovers(&mut self) {
        let input = self.input;
        if !self.global_policy().sprint_rollover {
            return;
        }
        let mut ended: Vec<&SprintSnap> = input
            .sprints
            .iter()
            .filter(|s| s.state == "active" && s.ends_ms <= input.now_ms)
            .collect();
        ended.sort_by(|a, b| a.id.cmp(&b.id));
        for sprint in ended {
            let ago = span(Some(
                input.now_ms.saturating_sub(sprint.ends_ms) as f64 / 1000.0,
            ));
            let text = format!(
                "sprint {} ended {} ago with {} open cards; close it and roll them over",
                sprint.name, ago, sprint.open_cards
            );
            for target in self.supervisors.clone() {
                self.push(ReconcileAction::Notice {
                    key: format!("sprint:{}:{target}", sprint.id),
                    target: target.to_owned(),
                    kind: "sprint_end".to_owned(),
                    subject: sprint.id.clone(),
                    text: text.clone(),
                });
            }
        }
    }
}
