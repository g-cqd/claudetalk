//! Reconciler scenarios through agora-sched's public API (SPEC section 6, rules R1 to R6): multi-hour timelines,
//! the floors over ten-minute buckets, WIP and sprint notices, the pass cap, input-order independence, and the
//! policy layers as the reconciler reads them. Each scenario runs `plan` over simulated time. `Store` plays the
//! store between passes: every returned action becomes a mark, and the effects the store owns (lease release,
//! reclaim, admission pause, blocker card) are applied to the next input. No clock, host or file is read.

use agora_sched::admit::{MachineSnapshot, Signals};
use agora_sched::model::{LeaseKind, LeaseRecord, PolicyRecord};
use agora_sched::policy::TeamPolicy;
use agora_sched::reconcile::{
    CardSnap, MAX_ACTIONS_PER_PASS, Mark, ReconcileAction, ReconcileInput, SprintSnap, plan,
};
use agora_sched::settings::WorkSettings;
use std::collections::BTreeSet;

mod floors;
mod notices;
mod order_keys;
mod timelines;

const NOW: i64 = 1_800_000_000_000;
const MINUTE: i64 = 60_000;
const GIB_KIB: u64 = 1024 * 1024;
/// 3.1 GiB free, below the 5 GiB default disk floor.
const DISK_BREACH_KIB: u64 = 3_250_586;
const HEALTHY_KIB: u64 = 100 * GIB_KIB;
/// Ten-minute bucket and one-hour bucket of NOW (NOW sits on both boundaries).
const BUCKET_10_MIN: i64 = 3_000_000;
const BUCKET_HOUR: i64 = 500_000;
const CLOSE_TO_RECLAIM: &str = "it will be reclaimed after 1 h 0 min";
const RECLAIM_OFF: &str = "reclaim is off for this lane";
const WIP_TEXT: &str = "lane core/dev has 3 cards in progress, above its WIP limit of 2";
const SPRINT_TEXT: &str =
    "sprint Sprint 1 ended 10 min 0 s ago with 2 open cards; close it and roll them over";

/// The instant `minutes` after NOW; negative values come before it.
fn at(minutes: i64) -> i64 {
    NOW + minutes * MINUTE
}

/// A 64-character lowercase hex public key whose pseudonym (first eight characters) is `tag` padded with zeros:
/// `pk("aa")` shows as "aa000000" in texts.
fn pk(tag: &str) -> String {
    format!("{tag:0<64}")
}

fn input() -> ReconcileInput {
    ReconcileInput {
        claims: vec![],
        now_ms: NOW,
        settings: WorkSettings::default(),
        machine: None,
        cards: Vec::new(),
        leases: Vec::new(),
        policies: Vec::new(),
        require_review_default: false,
        marks: Vec::new(),
        supervisors: Vec::new(),
        admission_paused: None,
        blocker_card: None,
        sprints: Vec::new(),
    }
}

/// A card in core/dev. `owner` is a tag for `pk`.
fn card(id: &str, state: &str, owner: Option<&str>, updated_ms: i64) -> CardSnap {
    card_in(id, "core", "dev", state, owner, updated_ms)
}

fn card_in(
    id: &str,
    team: &str,
    lane: &str,
    state: &str,
    owner: Option<&str>,
    updated_ms: i64,
) -> CardSnap {
    CardSnap {
        id: id.to_owned(),
        title: format!("Title {id}"),
        team: team.to_owned(),
        lane: lane.to_owned(),
        state: state.to_owned(),
        owner: owner.map(pk),
        updated_ms,
    }
}

fn lease(id: &str, kind: LeaseKind) -> LeaseRecord {
    LeaseRecord {
        id: id.to_owned(),
        kind,
        resource: "res".to_owned(),
        amount: 1.0,
        holder: pk("aa"),
        holder_name: "holder".to_owned(),
        card: None,
        task: None,
        created_ms: NOW - MINUTE,
        expires_ms: None,
        heartbeat_ms: NOW - MINUTE,
        expected_end_ms: None,
        released_ms: None,
        note: String::new(),
    }
}

fn policy(team: &str, lane: &str, policy: TeamPolicy, updated_ms: i64) -> PolicyRecord {
    PolicyRecord {
        team: team.to_owned(),
        lane: lane.to_owned(),
        policy,
        updated_ms,
        by: pk("ee"),
    }
}

fn sprint(id: &str, name: &str, state: &str, ends_ms: i64, open_cards: u32) -> SprintSnap {
    SprintSnap {
        id: id.to_owned(),
        name: name.to_owned(),
        ends_ms,
        state: state.to_owned(),
        open_cards,
    }
}

/// A snapshot with only the disk reading: `free_kib` free on the agora home filesystem.
fn disk(free_kib: u64) -> MachineSnapshot {
    MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: None,
        disk_free_kib: Some(free_kib),
        signals: None,
    }
}

/// Three fresh doing cards in core/dev, owned by `aa`, which is three cards in progress.
fn three_in_progress(prefix: &str) -> Vec<CardSnap> {
    (1..=3)
        .map(|n| card(&format!("{prefix}{n}"), "doing", Some("aa"), NOW))
        .collect()
}

fn notice(key: String, target: String, kind: &str, subject: &str, text: &str) -> ReconcileAction {
    ReconcileAction::Notice {
        key,
        target,
        kind: kind.to_owned(),
        subject: subject.to_owned(),
        text: text.to_owned(),
    }
}

/// The (target, kind, subject, text) of each notice, leaving out the dedupe key, so the WIP and sprint keys can
/// change without changing these scenarios.
fn notices(actions: &[ReconcileAction]) -> Vec<(String, String, String, String)> {
    actions
        .iter()
        .filter_map(|action| match action {
            ReconcileAction::Notice {
                target,
                kind,
                subject,
                text,
                ..
            } => Some((target.clone(), kind.clone(), subject.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn view(target: String, kind: &str, subject: &str, text: &str) -> (String, String, String, String) {
    (target, kind.to_owned(), subject.to_owned(), text.to_owned())
}

/// The owner's first stale notice (R3) for card `id`, owned by tag `owner`, last updated at `updated_ms`.
fn stale_notice(id: &str, owner: &str, updated_ms: i64, idle: &str) -> ReconcileAction {
    notice(
        format!("stale1:{id}:{updated_ms}"),
        pk(owner),
        "stale",
        id,
        &format!(
            "card {id} (Title {id}) has been doing for {idle} without activity; move it, add evidence, or release it"
        ),
    )
}

/// The escalation notice (R3, second threshold) about card `id` (owned by tag `owner`) to `target`, a tag.
fn escalation_notice(
    id: &str,
    owner: &str,
    updated_ms: i64,
    target: &str,
    idle: &str,
    tail: &str,
) -> ReconcileAction {
    notice(
        format!("stale2:{id}:{updated_ms}:{}", pk(target)),
        pk(target),
        "escalation",
        id,
        &format!("card {id} (Title {id}) owned by {owner:0<8} has been doing for {idle}; {tail}"),
    )
}

/// The reclaim of card `id` from tag `owner`, and the owner's notice that it was reclaimed.
fn reclaim_actions(id: &str, owner: &str, updated_ms: i64, idle: &str) -> Vec<ReconcileAction> {
    vec![
        notice(
            format!("stale3:{id}:{updated_ms}"),
            pk(owner),
            "reclaimed",
            id,
            &format!(
                "card {id} (Title {id}) was reclaimed after {idle} without activity; it is ready again and no longer yours"
            ),
        ),
        ReconcileAction::ReclaimCard {
            key: format!("reclaim:{id}:{updated_ms}"),
            card: id.to_owned(),
            owner: pk(owner),
        },
    ]
}

/// Asserts a floor-breach pass: admission paused with `text`, then a blocker opened in the same bucket.
fn assert_breach_pass(actions: &[ReconcileAction], bucket: i64, text: &str) {
    let [pause, blocker] = actions else {
        panic!("a breach pauses admission and opens a blocker, got {actions:?}");
    };
    assert_eq!(
        pause,
        &ReconcileAction::Admission {
            key: format!("pause:{bucket}"),
            paused: Some(text.to_owned()),
        }
    );
    let ReconcileAction::Blocker {
        key,
        open,
        title,
        owner_action,
    } = blocker
    else {
        panic!("expected a blocker, got {blocker:?}");
    };
    assert_eq!(key, &format!("blocker:{bucket}"));
    assert!(*open, "the blocker is opened");
    assert_eq!(title, &format!("Machine floor breached: {text}"));
    assert!(
        !owner_action.is_empty(),
        "the blocker names an owner action"
    );
}

/// The store between passes. Each returned action is recorded as a mark; the effects the store owns are applied
/// to the next input, as the real store would apply them.
struct Store {
    input: ReconcileInput,
}

impl Store {
    fn new(input: ReconcileInput) -> Self {
        Store { input }
    }

    /// One reconciler pass at `now_ms`; its actions are applied before it returns.
    fn pass(&mut self, now_ms: i64) -> Vec<ReconcileAction> {
        self.input.now_ms = now_ms;
        let actions = plan(&self.input);
        for action in &actions {
            self.apply(action, now_ms);
        }
        actions
    }

    /// Several passes; returns only the passes that emitted something, with their minute.
    fn passes_at(&mut self, minutes: &[i64]) -> Vec<(i64, Vec<ReconcileAction>)> {
        minutes
            .iter()
            .filter_map(|&m| {
                let actions = self.pass(at(m));
                (!actions.is_empty()).then_some((m, actions))
            })
            .collect()
    }

    fn apply(&mut self, action: &ReconcileAction, now_ms: i64) {
        let key = action.key();
        match self.input.marks.iter().position(|m| m.key == key) {
            Some(i) => {
                let mark = &mut self.input.marks[i];
                mark.count += 1;
                mark.last_ms = now_ms;
            }
            None => self.input.marks.push(Mark {
                key: key.to_owned(),
                last_ms: now_ms,
                count: 1,
            }),
        }
        match action {
            ReconcileAction::ExpireLease { lease, .. }
            | ReconcileAction::CloseQuiet { lease, .. } => {
                for l in self.input.leases.iter_mut().filter(|l| &l.id == lease) {
                    l.released_ms = Some(now_ms);
                }
            }
            ReconcileAction::ReclaimCard { card, .. } => {
                for c in self.input.cards.iter_mut().filter(|c| &c.id == card) {
                    c.state = "ready".to_owned();
                    c.owner = None;
                    c.updated_ms = now_ms;
                }
            }
            ReconcileAction::Admission { paused, .. } => {
                self.input.admission_paused = paused.clone();
            }
            ReconcileAction::Blocker { open: true, .. } => {
                self.input.blocker_card = Some("blocker-1".to_owned());
            }
            ReconcileAction::Blocker { open: false, .. } => self.input.blocker_card = None,
            ReconcileAction::Notice { .. } | ReconcileAction::ExpireClaim { .. } => {}
        }
    }

    /// Activity on every card: each updated_ms moves to `now_ms`.
    fn touch_cards(&mut self, now_ms: i64) {
        for c in &mut self.input.cards {
            c.updated_ms = now_ms;
        }
    }
}
