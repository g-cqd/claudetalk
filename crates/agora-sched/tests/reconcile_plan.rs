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
            ReconcileAction::Notice { .. } => {}
        }
    }

    /// Activity on every card: each updated_ms moves to `now_ms`.
    fn touch_cards(&mut self, now_ms: i64) {
        for c in &mut self.input.cards {
            c.updated_ms = now_ms;
        }
    }
}

// ---- R3: one card over a multi-hour timeline --------------------------------------------------------------

#[test]
fn idle_card_gets_owner_notice_then_supervisor_escalation_then_reclaim_over_three_hours() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[0, 10, 20, 30, 40, 50, 70, 90, 120, 180]);
    assert_eq!(
        fired,
        vec![
            (30, vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "5a", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c1", "aa", NOW, "5b", "50 min 0 s", CLOSE_TO_RECLAIM),
                ],
            ),
            (70, reclaim_actions("c1", "aa", NOW, "1 h 10 min")),
        ]
    );
}

#[test]
fn an_update_restarts_the_stale_cycle_under_keys_that_name_the_new_timestamp() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(
        store.pass(at(30)),
        vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]
    );
    store.touch_cards(at(35));
    assert!(
        store.pass(at(60)).is_empty(),
        "25 minutes after the update the card is not stale yet"
    );
    assert_eq!(
        store.pass(at(65)),
        vec![stale_notice("c1", "aa", at(35), "30 min 0 s")]
    );
}

#[test]
fn escalation_goes_to_the_team_chain_and_other_lanes_fall_back_to_supervisors() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![
            card("c1", "doing", Some("aa"), NOW),
            card_in("c2", "ops", "dev", "doing", Some("dd"), NOW),
        ],
        policies: vec![policy(
            "core",
            "*",
            TeamPolicy {
                escalation_chain: Some(vec![pk("cc"), pk("bb")]),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[30, 50]);
    assert_eq!(
        fired,
        vec![
            (
                30,
                vec![
                    stale_notice("c1", "aa", NOW, "30 min 0 s"),
                    stale_notice("c2", "dd", NOW, "30 min 0 s"),
                ],
            ),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "cc", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c1", "aa", NOW, "bb", "50 min 0 s", CLOSE_TO_RECLAIM),
                    escalation_notice("c2", "dd", NOW, "5a", "50 min 0 s", CLOSE_TO_RECLAIM),
                ],
            ),
        ]
    );
}

#[test]
fn reclaim_off_for_the_lane_escalates_with_that_text_and_never_reclaims() {
    let mut store = Store::new(ReconcileInput {
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                reclaim: Some(false),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    });
    let fired = store.passes_at(&[30, 50, 70, 180]);
    assert_eq!(
        fired,
        vec![
            (30, vec![stale_notice("c1", "aa", NOW, "30 min 0 s")]),
            (
                50,
                vec![
                    escalation_notice("c1", "aa", NOW, "5a", "50 min 0 s", RECLAIM_OFF),
                    escalation_notice("c1", "aa", NOW, "5b", "50 min 0 s", RECLAIM_OFF),
                ],
            ),
        ]
    );
}

#[test]
fn stale_threshold_resolves_per_lane_through_the_team_and_lane_layers() {
    let idle_10_min = at(-10);
    let i = ReconcileInput {
        cards: vec![
            card_in("c1", "core", "dev", "doing", Some("aa"), idle_10_min),
            card_in("c2", "core", "review", "doing", Some("bb"), idle_10_min),
            card_in("c3", "ops", "dev", "doing", Some("cc"), idle_10_min),
            card_in("c4", "ops", "review", "doing", Some("dd"), idle_10_min),
        ],
        policies: vec![
            policy(
                "core",
                "*",
                TeamPolicy {
                    stale_after_s: Some(300.0),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "*",
                "review",
                TeamPolicy {
                    stale_after_s: Some(1200.0),
                    ..TeamPolicy::default()
                },
                1,
            ),
        ],
        ..input()
    };
    // core/dev takes the team threshold of 5 min; core/review takes the lane threshold of 20 min over the team's;
    // ops/dev keeps the 30 min default; ops/review takes the lane threshold of 20 min.
    assert_eq!(
        notices(&plan(&i)),
        vec![view(
            pk("aa"),
            "stale",
            "c1",
            "card c1 (Title c1) has been doing for 10 min 0 s without activity; move it, add evidence, or release it",
        )]
    );
}

// ---- R1 and R2: leases and quiet windows at their instants ----------------------------------------------

#[test]
fn path_lease_expires_on_the_pass_whose_time_equals_expires_ms() {
    let mut expiring = lease("p1", LeaseKind::Path);
    expiring.expires_ms = Some(at(15));
    let mut store = Store::new(ReconcileInput {
        leases: vec![expiring],
        ..input()
    });
    assert!(
        store.pass(at(15) - 1).is_empty(),
        "one millisecond before expires_ms the lease is live"
    );
    assert_eq!(
        store.pass(at(15)),
        vec![ReconcileAction::ExpireLease {
            key: format!("expire:p1:{}", at(15)),
            lease: "p1".to_owned(),
        }]
    );
}

#[test]
fn quiet_window_closes_once_quiet_max_has_passed_since_its_expected_end() {
    let mut window = lease("q1", LeaseKind::Quiet);
    window.expected_end_ms = Some(NOW);
    let quiet_max_ms = WorkSettings::default().quiet_max as i64 * 1000;
    let mut store = Store::new(ReconcileInput {
        leases: vec![window],
        ..input()
    });
    assert!(store.pass(NOW + quiet_max_ms - 1).is_empty());
    assert_eq!(
        store.pass(NOW + quiet_max_ms),
        vec![ReconcileAction::CloseQuiet {
            key: format!("quiet:q1:{NOW}"),
            lease: "q1".to_owned(),
        }]
    );
}

// ---- R5: machine floors over ten-minute buckets ---------------------------------------------------------

#[test]
fn disk_below_its_floor_pauses_admission_and_opens_a_blocker() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    assert_breach_pass(
        &store.pass(at(0)),
        BUCKET_10_MIN,
        "disk free 3.1 GiB below the floor of 5.0 GiB",
    );
}

#[test]
fn a_breach_repeated_within_the_same_ten_minute_bucket_adds_nothing() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    assert!(store.pass(at(5)).is_empty());
}

#[test]
fn a_breach_still_standing_in_the_next_bucket_refreshes_only_the_blocker() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    let actions = store.pass(at(10));
    assert!(
        matches!(
            actions.as_slice(),
            [ReconcileAction::Blocker { key, open: true, .. }]
                if key == &format!("blocker:{}", BUCKET_10_MIN + 1)
        ),
        "admission stays paused; only the blocker is refreshed, got {actions:?}"
    );
}

#[test]
fn recovery_resumes_admission_and_closes_the_blocker_once() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    store.input.machine = Some(disk(HEALTHY_KIB));
    let actions = store.pass(at(10));
    assert!(
        matches!(
            actions.as_slice(),
            [
                ReconcileAction::Admission { key, paused: None },
                ReconcileAction::Blocker { key: blocker, open: false, .. },
            ] if key == &format!("resume:{}", BUCKET_10_MIN + 1) && blocker == "unblock:blocker-1"
        ),
        "got {actions:?}"
    );
    assert!(store.pass(at(20)).is_empty(), "the close is not repeated");
}

#[test]
fn memory_floor_breach_names_the_available_memory_in_gib() {
    let i = ReconcileInput {
        machine: Some(MachineSnapshot {
            cpus: 8.0,
            total_memory_kib: Some(16 * GIB_KIB),
            disk_free_kib: Some(HEALTHY_KIB),
            signals: Some(Signals {
                memory_used_kib: Some(15 * GIB_KIB + GIB_KIB / 2),
                ..Signals::default()
            }),
        }),
        settings: WorkSettings {
            memory_floor_kib: Some(2 * GIB_KIB),
            ..WorkSettings::default()
        },
        ..input()
    };
    assert_breach_pass(
        &plan(&i),
        BUCKET_10_MIN,
        "available memory 0.5 GiB below the floor of 2.0 GiB",
    );
}

#[test]
fn cpu_idle_floor_breach_names_the_idle_percentage() {
    let i = ReconcileInput {
        machine: Some(MachineSnapshot {
            cpus: 8.0,
            total_memory_kib: None,
            disk_free_kib: Some(HEALTHY_KIB),
            signals: Some(Signals {
                idle_pct: Some(4.4),
                ..Signals::default()
            }),
        }),
        settings: WorkSettings {
            idle_floor_pct: Some(10.0),
            ..WorkSettings::default()
        },
        ..input()
    };
    assert_breach_pass(
        &plan(&i),
        BUCKET_10_MIN,
        "CPU 4% idle below the floor of 10%",
    );
}

// ---- R4: WIP limits by lane, through the team and lane layers -------------------------------------------

#[test]
fn wip_limit_resolves_per_lane_through_the_team_and_global_layers() {
    let mut cards = three_in_progress("a");
    cards.push(card_in("b1", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b2", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("c1", "ops", "dev", "doing", Some("aa"), NOW));
    cards.push(card_in("c2", "ops", "dev", "doing", Some("aa"), NOW));
    let i = ReconcileInput {
        cards,
        policies: vec![
            policy(
                "*",
                "*",
                TeamPolicy {
                    wip_limit: Some(1),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "*",
                TeamPolicy {
                    wip_limit: Some(2),
                    ..TeamPolicy::default()
                },
                1,
            ),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    // core/dev has 3 against the team's 2; core/review has 2, at the team's limit; ops/dev has 2 against the
    // global 1, because no team layer covers ops.
    assert_eq!(
        notices(&plan(&i)),
        vec![
            view(pk("5a"), "wip", "core/dev", WIP_TEXT),
            view(
                pk("5a"),
                "wip",
                "ops/dev",
                "lane ops/dev has 2 cards in progress, above its WIP limit of 1",
            ),
        ]
    );
}

#[test]
fn lane_layer_overrides_the_team_layer_for_the_wip_limit() {
    let mut cards = three_in_progress("a");
    cards.push(card_in("b1", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b2", "core", "review", "claimed", Some("aa"), NOW));
    cards.push(card_in("b3", "core", "review", "claimed", Some("aa"), NOW));
    let lane_limit = |limit: u32| TeamPolicy {
        wip_limit: Some(limit),
        ..TeamPolicy::default()
    };
    let i = ReconcileInput {
        cards,
        policies: vec![
            policy("*", "*", lane_limit(1), 1),
            policy("core", "*", lane_limit(2), 1),
            policy("*", "dev", lane_limit(4), 1),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    // core/dev: the lane layer (4) beats the team layer (2), so its 3 cards are under the limit.
    // core/review: no lane layer, so the team layer (2) applies to its 3 cards.
    assert_eq!(
        notices(&plan(&i)),
        vec![view(
            pk("5a"),
            "wip",
            "core/review",
            "lane core/review has 3 cards in progress, above its WIP limit of 2",
        )]
    );
}

#[test]
fn wip_notice_is_sent_once_per_clock_hour() {
    let mut store = Store::new(ReconcileInput {
        cards: three_in_progress("a"),
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5a")],
        ..input()
    });
    let first = store.pass(at(0));
    assert_eq!(notices(&first).len(), 1);
    store.touch_cards(at(59));
    assert!(store.pass(at(59)).is_empty(), "same hour bucket");
    store.touch_cards(at(60));
    let next_hour = store.pass(at(60));
    assert_eq!(notices(&next_hour), notices(&first));
    assert_eq!(
        next_hour.len(),
        1,
        "the next hour bucket re-sends the notice"
    );
}

// ---- R6: sprints past their end -------------------------------------------------------------------------

#[test]
fn ended_active_sprints_notify_each_supervisor_in_sorted_order() {
    let i = ReconcileInput {
        sprints: vec![
            sprint("s1", "Sprint 1", "active", at(-10), 2),
            sprint("s2", "Sprint 2", "closed", at(-10), 0),
            sprint("s3", "Sprint 3", "active", at(1), 5),
        ],
        supervisors: vec![pk("5b"), pk("5a")],
        ..input()
    };
    assert_eq!(
        notices(&plan(&i)),
        vec![
            view(pk("5a"), "sprint_end", "s1", SPRINT_TEXT),
            view(pk("5b"), "sprint_end", "s1", SPRINT_TEXT),
        ]
    );
}

#[test]
fn an_ended_sprint_is_not_announced_again_once_its_notices_are_applied() {
    let mut store = Store::new(ReconcileInput {
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(store.pass(at(0)).len(), 1);
    assert!(store.pass(at(5)).is_empty());
}

// ---- Pass cap ---------------------------------------------------------------------------------------------

#[test]
fn a_pass_stops_at_the_action_cap_in_card_id_order_with_100_stale_cards() {
    let cards: Vec<CardSnap> = (0..100)
        .map(|n| card(&format!("c{n:03}"), "doing", Some("aa"), at(-40)))
        .collect();
    let actions = plan(&ReconcileInput { cards, ..input() });
    let subjects: Vec<String> = notices(&actions)
        .into_iter()
        .map(|(_, _, subject, _)| subject)
        .collect();
    let first_64: Vec<String> = (0..MAX_ACTIONS_PER_PASS)
        .map(|n| format!("c{n:03}"))
        .collect();
    assert_eq!(actions.len(), MAX_ACTIONS_PER_PASS);
    assert_eq!(subjects, first_64);
}

#[test]
fn cards_past_the_cap_are_noticed_on_the_next_pass() {
    let cards: Vec<CardSnap> = (0..100)
        .map(|n| card(&format!("c{n:03}"), "doing", Some("aa"), at(-40)))
        .collect();
    let mut store = Store::new(ReconcileInput { cards, ..input() });
    assert_eq!(store.pass(at(0)).len(), MAX_ACTIONS_PER_PASS);
    let rest: Vec<String> = notices(&store.pass(at(0)))
        .into_iter()
        .map(|(_, _, subject, _)| subject)
        .collect();
    let expected: Vec<String> = (MAX_ACTIONS_PER_PASS..100)
        .map(|n| format!("c{n:03}"))
        .collect();
    assert_eq!(rest, expected);
}

// ---- Input order and duplicate policy rows ----------------------------------------------------------------

#[test]
fn reversing_or_rotating_every_input_vector_gives_the_same_plan() {
    let mut path = lease("p1", LeaseKind::Path);
    path.expires_ms = Some(NOW - 1);
    let mut pool = lease("p2", LeaseKind::Pool);
    pool.expires_ms = Some(NOW - 1);
    let mut quiet = lease("q1", LeaseKind::Quiet);
    quiet.expected_end_ms = Some(NOW - 200_000);
    let base = ReconcileInput {
        now_ms: at(70),
        settings: WorkSettings::default(),
        machine: Some(disk(DISK_BREACH_KIB)),
        cards: vec![
            card("c1", "doing", Some("aa"), NOW),
            card("c2", "claimed", Some("bb"), at(35)),
            card_in("c3", "ops", "dev", "doing", Some("cc"), at(65)),
            card_in("c4", "core", "dev", "review", None, at(70)),
            card("c5", "doing", Some("dd"), at(70)),
            card("c6", "claimed", Some("ee"), at(70)),
        ],
        leases: vec![pool, path, quiet],
        policies: vec![
            policy(
                "*",
                "*",
                TeamPolicy {
                    wip_limit: Some(2),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "dev",
                TeamPolicy {
                    escalation_chain: Some(vec![pk("cc"), pk("bb")]),
                    ..TeamPolicy::default()
                },
                2,
            ),
        ],
        require_review_default: true,
        marks: vec![
            Mark {
                key: format!("expire:p2:{}", NOW - 1),
                last_ms: NOW,
                count: 1,
            },
            Mark {
                key: "unrelated".to_owned(),
                last_ms: NOW,
                count: 1,
            },
        ],
        supervisors: vec![pk("bb"), pk("5a"), pk("bb")],
        admission_paused: None,
        blocker_card: None,
        sprints: vec![
            sprint("s1", "Sprint 1", "active", NOW, 3),
            sprint("s2", "Sprint 2", "active", at(-5), 0),
        ],
    };
    let expected = plan(&base);
    let kinds: BTreeSet<&str> = expected.iter().map(ReconcileAction::op_kind).collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            "reconcile.expire",
            "reconcile.quiet_close",
            "reconcile.notice",
            "reconcile.reclaim",
            "reconcile.blocker",
            "reconcile.admission",
        ]),
        "the scenario exercises every rule"
    );

    let mut reversed = base.clone();
    reversed.cards.reverse();
    reversed.leases.reverse();
    reversed.policies.reverse();
    reversed.supervisors.reverse();
    reversed.sprints.reverse();
    reversed.marks.reverse();
    assert_eq!(plan(&reversed), expected);

    let mut rotated = base.clone();
    rotated.cards.rotate_left(2);
    rotated.leases.rotate_left(1);
    assert_eq!(plan(&rotated), expected);
}

#[test]
fn the_most_recently_updated_duplicate_policy_row_wins_in_either_input_order() {
    let mut i = ReconcileInput {
        cards: vec![
            card("c1", "claimed", Some("aa"), NOW),
            card("c2", "doing", Some("bb"), NOW),
        ],
        policies: vec![
            policy(
                "core",
                "dev",
                TeamPolicy {
                    wip_limit: Some(1),
                    ..TeamPolicy::default()
                },
                1,
            ),
            policy(
                "core",
                "dev",
                TeamPolicy {
                    wip_limit: Some(3),
                    ..TeamPolicy::default()
                },
                2,
            ),
        ],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let forward = plan(&i);
    i.policies.reverse();
    assert_eq!(plan(&i), forward);
    assert!(
        forward.is_empty(),
        "the newer row sets the limit to 3, and two cards are under it"
    );
}

// ---- Keys (SPEC section 6) ------------------------------------------------------------------------------

#[test]
fn action_keys_follow_the_section_6_formats() {
    let mut path = lease("p1", LeaseKind::Path);
    path.expires_ms = Some(NOW - 1);
    let mut quiet = lease("q1", LeaseKind::Quiet);
    quiet.expected_end_ms = Some(NOW - 200_000);
    let i = ReconcileInput {
        now_ms: at(70),
        machine: Some(disk(DISK_BREACH_KIB)),
        cards: vec![card("c1", "doing", Some("aa"), NOW)],
        leases: vec![path, quiet],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let keys: Vec<String> = plan(&i).iter().map(|a| a.key().to_owned()).collect();
    assert_eq!(
        keys,
        vec![
            format!("expire:p1:{}", NOW - 1),
            format!("quiet:q1:{}", NOW - 200_000),
            format!("stale1:c1:{NOW}"),
            format!("stale2:c1:{NOW}:{}", pk("5a")),
            format!("stale3:c1:{NOW}"),
            format!("reclaim:c1:{NOW}"),
            format!("pause:{}", BUCKET_10_MIN + 7),
            format!("blocker:{}", BUCKET_10_MIN + 7),
        ]
    );
}

// ---- Blocker, WIP and sprint keys (M5 decisions O-1 and O-2) --------------------------------------------------

#[test]
fn a_stale_blocker_card_is_closed_even_when_admission_was_never_paused() {
    let i = ReconcileInput {
        machine: Some(disk(HEALTHY_KIB)),
        blocker_card: Some("blocker-7".to_owned()),
        ..input()
    };
    let actions = plan(&i);
    assert!(
        matches!(
            actions.as_slice(),
            [ReconcileAction::Blocker { key, open: false, .. }] if key == "unblock:blocker-7"
        ),
        "got {actions:?}"
    );
}

#[test]
fn a_wip_notice_to_one_supervisor_does_not_suppress_one_added_later() {
    let mut store = Store::new(ReconcileInput {
        cards: three_in_progress("a"),
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
            1,
        )],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(notices(&store.pass(at(0))).len(), 1);
    store.input.supervisors = vec![pk("5a"), pk("5b")];
    assert_eq!(
        notices(&store.pass(at(0))),
        vec![view(pk("5b"), "wip", "core/dev", WIP_TEXT)]
    );
}

#[test]
fn a_sprint_notice_to_one_supervisor_does_not_suppress_one_added_later() {
    let mut store = Store::new(ReconcileInput {
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    });
    assert_eq!(store.pass(at(0)).len(), 1);
    store.input.supervisors = vec![pk("5a"), pk("5b")];
    assert_eq!(
        notices(&store.pass(at(0))),
        vec![view(pk("5b"), "sprint_end", "s1", SPRINT_TEXT)]
    );
}

#[test]
fn wip_and_sprint_notice_keys_name_their_target() {
    let i = ReconcileInput {
        cards: three_in_progress("a"),
        policies: vec![policy(
            "core",
            "dev",
            TeamPolicy {
                wip_limit: Some(2),
                ..TeamPolicy::default()
            },
            1,
        )],
        sprints: vec![sprint("s1", "Sprint 1", "active", at(-10), 2)],
        supervisors: vec![pk("5a")],
        ..input()
    };
    let keys: Vec<String> = plan(&i).iter().map(|a| a.key().to_owned()).collect();
    assert_eq!(
        keys,
        vec![
            format!("wip:core/dev:3:{BUCKET_HOUR}:{}", pk("5a")),
            format!("sprint:s1:{}", pk("5a")),
        ]
    );
}
