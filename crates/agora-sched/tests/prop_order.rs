//! Property tests over random task graphs (SPEC section 8, prop_order.rs): the ordering policies of `order`, the
//! Graham list schedule of `plan::schedule`, the congestion game of `assign::assign`, and the head cap of
//! `quiet::decide`. A graph has 1 to 12 tasks with ids "t0".."t11", and every prerequisite is an earlier task, so the
//! graph is acyclic by construction. Runs use a fixed ChaCha seed and write no persistence files.

use std::cmp::Ordering;

use agora_sched::admit::{self, MachineSnapshot, MemLevel, RunningJob, Signals, WaitingJob};
use agora_sched::assign::assign;
use agora_sched::graph::{Graph, conflict};
use agora_sched::order::{OrderPolicy, cmp_key, order};
use agora_sched::plan::{Schedule, Slot, schedule};
use agora_sched::predict::exclusive_at;
use agora_sched::quiet::{AdmitInput, decide};
use agora_sched::settings::WorkSettings;
use agora_sched::task::{Task, tier_of};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngAlgorithm, RngSeed};

const SEED: u64 = 0x0A60_0A5C_ED00_0005;
/// Float slack for times and costs. Sizes are at least 0.5, so this never hides a real overlap.
const TOL: f64 = 1e-9;
const POLICIES: [OrderPolicy; 3] = [
    OrderPolicy::Fifo,
    OrderPolicy::Wsjf,
    OrderPolicy::CriticalPath,
];
const CPUS: f64 = 4.0;

/// One task before its prerequisites are chosen: a bit mask over earlier tasks, kind, size, value and file paths.
type Row = (u16, &'static str, f64, f64, Vec<&'static str>);

fn rows() -> impl Strategy<Value = Vec<Row>> {
    prop::collection::vec(
        (
            any::<u16>(),
            prop::sample::select(vec!["other", "validity", "crash", "docs"]),
            prop::sample::select(vec![0.5_f64, 1.0, 2.0, 3.0, 8.0]),
            prop::sample::select(vec![0.5_f64, 1.0, 2.0, 3.0, 8.0]),
            prop::collection::vec(
                prop::sample::select(vec!["src", "src/a", "src/a/b", "docs", "/", ""]),
                0..=2,
            ),
        ),
        1..=12,
    )
}

/// Random tasks whose `after` lists name only earlier tasks, so the graph is a DAG.
fn dag() -> impl Strategy<Value = Vec<Task>> {
    rows().prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(index, (mask, kind, size, value, files))| Task {
                index,
                id: format!("t{index}"),
                title: String::new(),
                kind: kind.to_owned(),
                tier: tier_of(kind),
                queued: None,
                after: (0..index)
                    .filter(|&j| (mask >> j) & 1 == 1)
                    .map(|j| format!("t{j}"))
                    .collect(),
                size,
                value,
                files: files.into_iter().map(str::to_owned).collect(),
            })
            .collect()
    })
}

fn graph(tasks: Vec<Task>) -> Graph {
    Graph::new(tasks).expect("generated graphs are acyclic and name only earlier tasks")
}

/// The roots in ranking order, which is how ref-ordering 4.7 ranks the options of an agent.
fn ranked_roots(g: &Graph, policy: OrderPolicy) -> Vec<usize> {
    order(g, policy)
        .order
        .into_iter()
        .filter(|&t| g.tasks[t].after.is_empty())
        .collect()
}

/// An agent's cost per ref-ordering 4.7, recomputed here from the rules: the rank of its task (the idle rank for
/// None), plus a clash with every other agent's task.
fn agent_cost(
    g: &Graph,
    roots: &[usize],
    penalty: f64,
    choice: Option<usize>,
    others: &[Option<usize>],
) -> f64 {
    let idle = roots.len() as f64;
    let Some(task) = choice else {
        return idle;
    };
    let rank = roots
        .iter()
        .position(|&r| r == task)
        .expect("choices are ranked roots") as f64;
    let duplicate = idle + 1.0;
    let clash_sum: f64 = others
        .iter()
        .flatten()
        .map(|&other| {
            if other == task {
                duplicate
            } else if conflict(&g.tasks[task], &g.tasks[other]) {
                penalty
            } else {
                0.0
            }
        })
        .sum();
    rank + clash_sum
}

/// Every placed slot of a schedule, across all lanes.
fn all_slots(s: &Schedule) -> Vec<&Slot> {
    s.lanes.iter().flatten().collect()
}

/// The critical path length in sizes, computed from the tasks alone (prerequisites have smaller indices).
fn critical_length(g: &Graph) -> f64 {
    let mut finish = vec![0.0_f64; g.tasks.len()];
    for (t, task) in g.tasks.iter().enumerate() {
        let before = task
            .after
            .iter()
            .map(|p| finish[g.index[p]])
            .fold(0.0_f64, f64::max);
        finish[t] = task.size + before;
    }
    finish.iter().copied().fold(0.0_f64, f64::max)
}

fn admission_scenario() -> impl Strategy<Value = (Vec<WaitingJob>, Vec<RunningJob>)> {
    let waiting = prop::collection::vec(
        (
            prop::sample::select(vec![1.0_f64, 2.0, 3.0, 4.0]),
            prop::sample::select(vec!["other", "validity", "docs"]),
            0_u32..=100,
            5_u32..=600,
        ),
        1..=6,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(index, (weight, kind, requested, duration))| WaitingJob {
                id: format!("j{index}"),
                kind: kind.to_owned(),
                seq: i64::try_from(index).unwrap_or(i64::MAX),
                requested_s: f64::from(requested),
                weight,
                pred_cores: None,
                pred_duration_s: Some(f64::from(duration)),
                pred_duration_p90_s: None,
                pred_rss_p90_kib: None,
                machine_held_since_s: None,
            })
            .collect()
    });
    let running = prop::collection::vec(
        (prop::sample::select(vec![1.0_f64, 2.0, 3.0]), 0_u32..=100),
        0..=2,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(index, (weight, started))| RunningJob {
                id: format!("r{index}"),
                weight,
                pred_cores: None,
                pred_rss_p90_kib: None,
                pred_duration_p90_s: Some(300.0),
                rss_kib: None,
                started_s: Some(f64::from(started)),
                quiet_until_s: None,
            })
            .collect()
    });
    (waiting, running)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(SEED),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn order_lists_every_task_exactly_once(tasks in dag()) {
        let g = graph(tasks);
        let n = g.tasks.len();
        for policy in POLICIES {
            let mut listed = order(&g, policy).order;
            listed.sort_unstable();
            prop_assert_eq!(listed, (0..n).collect::<Vec<_>>(), "policy {}", policy.as_str());
        }
    }

    #[test]
    fn order_puts_every_prerequisite_before_its_dependent(tasks in dag()) {
        let g = graph(tasks);
        for policy in POLICIES {
            let listed = order(&g, policy).order;
            let mut position = vec![0_usize; g.tasks.len()];
            for (place, &t) in listed.iter().enumerate() {
                position[t] = place;
            }
            for task in &g.tasks {
                for prereq in &task.after {
                    let p = g.index[prereq];
                    prop_assert!(
                        position[p] < position[task.index],
                        "policy {}: {} came before its prerequisite {}",
                        policy.as_str(),
                        task.id,
                        prereq
                    );
                }
            }
        }
    }

    #[test]
    fn order_keeps_adjacent_tasks_in_effective_priority(tasks in dag()) {
        let g = graph(tasks);
        for policy in POLICIES {
            let ordered = order(&g, policy);
            for pair in ordered.order.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if g.tasks[b].after.contains(&g.tasks[a].id) {
                    continue;
                }
                // The heap pops the smallest (effective key, id), and b was already ready when a was popped.
                let by_key = cmp_key(&ordered.effective[a], &ordered.effective[b])
                    .then_with(|| g.tasks[a].id.cmp(&g.tasks[b].id));
                prop_assert!(
                    by_key != Ordering::Greater,
                    "policy {}: {} was placed before {} although it has the lower priority",
                    policy.as_str(),
                    g.tasks[a].id,
                    g.tasks[b].id
                );
            }
        }
    }

    #[test]
    fn schedule_runs_each_task_once_in_non_overlapping_slots_of_its_size(tasks in dag()) {
        let g = graph(tasks);
        let n = g.tasks.len();
        for agents in 1..=4_usize {
            for policy in POLICIES {
                let s = schedule(&g, agents, policy);
                prop_assert_eq!(s.lanes.len(), agents);
                let mut placed: Vec<usize> = all_slots(&s).iter().map(|slot| slot.task).collect();
                placed.sort_unstable();
                prop_assert_eq!(placed, (0..n).collect::<Vec<_>>());
                for lane in &s.lanes {
                    for pair in lane.windows(2) {
                        prop_assert!(
                            pair[1].start >= pair[0].end - TOL,
                            "agents {agents}: lane slots overlap"
                        );
                    }
                    for slot in lane {
                        let size = g.tasks[slot.task].size;
                        prop_assert!(
                            ((slot.end - slot.start) - size).abs() <= TOL,
                            "agents {agents}: {} runs {} for size {}",
                            g.tasks[slot.task].id,
                            slot.end - slot.start,
                            size
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn schedule_starts_each_task_after_all_its_prerequisites_have_ended(tasks in dag()) {
        let g = graph(tasks);
        for agents in 1..=4_usize {
            for policy in POLICIES {
                let s = schedule(&g, agents, policy);
                let slots = all_slots(&s);
                for task in &g.tasks {
                    let mine = slots
                        .iter()
                        .find(|slot| slot.task == task.index)
                        .expect("every task is scheduled");
                    for prereq in &task.after {
                        let theirs = slots
                            .iter()
                            .find(|slot| slot.task == g.index[prereq])
                            .expect("every task is scheduled");
                        prop_assert!(
                            mine.start >= theirs.end - TOL,
                            "agents {agents}: {} starts at {} before {prereq} ends at {}",
                            task.id,
                            mine.start,
                            theirs.end
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn schedule_never_runs_two_conflicting_tasks_at_the_same_time(tasks in dag()) {
        let g = graph(tasks);
        for agents in 1..=4_usize {
            for policy in POLICIES {
                let s = schedule(&g, agents, policy);
                let slots = all_slots(&s);
                for (i, a) in slots.iter().enumerate() {
                    for b in slots.iter().skip(i + 1) {
                        if !conflict(&g.tasks[a.task], &g.tasks[b.task]) {
                            continue;
                        }
                        let apart = a.end <= b.start + TOL || b.end <= a.start + TOL;
                        prop_assert!(
                            apart,
                            "agents {agents}: conflicting {} [{}, {}) and {} [{}, {}) overlap",
                            g.tasks[a.task].id,
                            a.start,
                            a.end,
                            g.tasks[b.task].id,
                            b.start,
                            b.end
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn schedule_reports_the_last_finish_and_never_less_than_the_lower_bound(tasks in dag()) {
        let g = graph(tasks);
        for agents in 1..=4_usize {
            for policy in POLICIES {
                let s = schedule(&g, agents, policy);
                let last_finish = all_slots(&s).iter().map(|slot| slot.end).fold(0.0_f64, f64::max);
                prop_assert!(
                    (s.makespan - last_finish).abs() <= TOL,
                    "agents {agents}: makespan {} but the last task ends at {last_finish}",
                    s.makespan
                );
                prop_assert!(
                    s.makespan >= s.lower_bound - TOL,
                    "agents {agents}: makespan {} is below the lower bound {}",
                    s.makespan,
                    s.lower_bound
                );
            }
        }
    }

    #[test]
    fn schedule_without_files_meets_grahams_bound(tasks in dag()) {
        let mut tasks = tasks;
        for task in &mut tasks {
            task.files.clear();
        }
        let g = graph(tasks);
        let total: f64 = g.tasks.iter().map(|task| task.size).sum();
        let longest = critical_length(&g);
        for agents in 1..=4_usize {
            let k = agents as f64;
            for policy in POLICIES {
                let s = schedule(&g, agents, policy);
                let bound = total / k + (1.0 - 1.0 / k) * longest;
                prop_assert!(
                    s.makespan <= bound + TOL,
                    "agents {agents}, policy {}: makespan {} exceeds the Graham bound {bound}",
                    policy.as_str(),
                    s.makespan
                );
            }
        }
    }

    #[test]
    fn assign_gives_each_agent_a_ranked_root_or_idle(tasks in dag()) {
        let g = graph(tasks);
        for agents in 1..=4_usize {
            for policy in POLICIES {
                for conflict_cost in [None, Some(0.0), Some(0.5), Some(2.0), Some(100.0)] {
                    let a = assign(&g, agents, policy, conflict_cost);
                    let roots = ranked_roots(&g, policy);
                    prop_assert_eq!(a.choices.len(), agents);
                    for &t in a.choices.iter().flatten() {
                        prop_assert!(
                            roots.contains(&t),
                            "agents {agents}: {} is not a ranked root",
                            g.tasks[t].id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn assign_is_a_nash_equilibrium_no_agent_can_lower_its_cost(tasks in dag()) {
        let g = graph(tasks);
        for agents in 1..=4_usize {
            for policy in POLICIES {
                for conflict_cost in [None, Some(0.0), Some(0.5), Some(2.0), Some(100.0)] {
                    let a = assign(&g, agents, policy, conflict_cost);
                    let roots = ranked_roots(&g, policy);
                    let penalty = conflict_cost.unwrap_or(roots.len() as f64 + 1.0);
                    for agent in 0..agents {
                        let others: Vec<Option<usize>> = a
                            .choices
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != agent)
                            .map(|(_, choice)| *choice)
                            .collect();
                        let current = agent_cost(&g, &roots, penalty, a.choices[agent], &others);
                        let options = std::iter::once(None).chain(roots.iter().map(|&r| Some(r)));
                        for option in options {
                            let alternative = agent_cost(&g, &roots, penalty, option, &others);
                            prop_assert!(
                                alternative >= current - TOL,
                                "agents {agents}, agent {agent}: switching to {option:?} lowers the cost from {current} to {alternative}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn admission_never_starts_a_job_past_the_head_once_the_head_waited_the_backfill_limit(
        (waiting, running) in admission_scenario()
    ) {
        let settings = WorkSettings::default();
        let mut line: Vec<&WaitingJob> = waiting
            .iter()
            .filter(|w| !exclusive_at(w.weight, CPUS) && !admit::is_tiny(admit::cores(w.weight, w.pred_cores)))
            .collect();
        line.sort_by(|a, b| admit::waiting_order_cmp(a, b, settings.aging));
        prop_assume!(!line.is_empty());
        let head_id = line[0].id.clone();
        let now_s = line[0].requested_s + settings.backfill_wait + 1.0;

        let input = AdmitInput {
            now_s,
            settings: settings.clone(),
            machine: MachineSnapshot {
                cpus: CPUS,
                total_memory_kib: None,
                disk_free_kib: None,
                signals: Some(Signals {
                    idle_pct: Some(90.0),
                    memory_level: Some(MemLevel::Normal),
                    ..Signals::default()
                }),
            },
            running,
            waiting: waiting.clone(),
            window_closes: Vec::new(),
            paused: None,
            manual_quiet: Vec::new(),
        };
        let decision = decide(&input);

        if !decision.start.iter().any(|start| start.id == head_id) {
            for start in &decision.start {
                let job = waiting.iter().find(|w| w.id == start.id).expect("started jobs are waiting");
                // Exclusive jobs run under the quiet rules, and tiny jobs are exempt from the head cap.
                let exempt = exclusive_at(job.weight, CPUS)
                    || admit::is_tiny(admit::cores(job.weight, job.pred_cores));
                prop_assert!(
                    exempt,
                    "job {} started while head {head_id} is held, after the head waited {}s (limit {}s)",
                    start.id,
                    now_s - line[0].requested_s,
                    settings.backfill_wait
                );
            }
        }
    }
}
