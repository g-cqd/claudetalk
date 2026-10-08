//! Graham list scheduling over the ordered tasks (ref-ordering section 4.6).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};

use crate::graph::{Graph, conflict};
use crate::order::{Key, OrderPolicy, cmp_key, order};
use crate::pyfmt::py_fsum;
use crate::task::Task;

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub task: usize,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub lanes: Vec<Vec<Slot>>,
    pub makespan: f64,
    pub lower_bound: f64,
}

/// The most agents one schedule or assignment is run for. The CLI, the daemon and MCP refuse more (work.py has no cap);
/// `schedule` and `assign` clamp to it as a backstop, so no caller can make them allocate a lane per unbounded agent.
pub const MAX_AGENTS: usize = 256;

/// Validates an agent count from a request: 1 to `MAX_AGENTS` inclusive.
pub fn check_agents(n: usize) -> Result<usize, String> {
    if (1..=MAX_AGENTS).contains(&n) {
        Ok(n)
    } else {
        Err(format!("agents must be between 1 and {MAX_AGENTS}"))
    }
}

/// Graham list scheduling, ref-ordering section 4.6 (agents >= 1).
///
/// work.py raises for 0 agents and the CLI never passes 0; for 0 this returns no lanes and zero bounds instead of
/// dividing by zero. Counts above `MAX_AGENTS` are clamped to it.
pub fn schedule(g: &Graph, agents: usize, policy: OrderPolicy) -> Schedule {
    let agents = agents.min(MAX_AGENTS);
    if agents == 0 {
        return Schedule {
            lanes: Vec::new(),
            makespan: 0.0,
            lower_bound: 0.0,
        };
    }
    let effective = order(g, policy).effective;
    let mut indegree: Vec<usize> = g
        .tasks
        .iter()
        .map(|task| task.after.iter().collect::<BTreeSet<_>>().len())
        .collect();
    let mut ready: BinaryHeap<Reverse<Ready<'_>>> = (0..g.tasks.len())
        .filter(|&position| indegree.get(position) == Some(&0))
        .filter_map(|position| ready_entry(g, &effective, position))
        .map(Reverse)
        .collect();
    let mut running: BinaryHeap<Reverse<Running<'_>>> = BinaryHeap::new();
    let mut free: BinaryHeap<Reverse<usize>> = (0..agents).map(Reverse).collect();
    let mut lanes: Vec<Vec<Slot>> = vec![Vec::new(); agents];
    let mut now = 0.0_f64;

    while !ready.is_empty() || !running.is_empty() {
        // Start the most urgent ready tasks on the lowest free agents. A task that shares a file with a running
        // task (including one started at this same instant) waits and goes back to the ready heap.
        let mut waiting = Vec::new();
        while let Some(&Reverse(agent)) = free.peek() {
            let Some(Reverse(next)) = ready.pop() else {
                break;
            };
            if running
                .iter()
                .any(|Reverse(busy)| conflict(next.task, busy.task))
            {
                waiting.push(Reverse(next));
                continue;
            }
            free.pop();
            let end = now + next.task.size;
            if let Some(lane) = lanes.get_mut(agent) {
                lane.push(Slot {
                    task: next.position,
                    start: now,
                    end,
                });
            }
            running.push(Reverse(Running {
                end,
                agent,
                task: next.task,
                position: next.position,
            }));
        }
        ready.extend(waiting);

        let Some(next_end) = running.peek().map(|Reverse(r)| r.end) else {
            break;
        };
        now = next_end;
        // Every task ending at exactly `now` completes together; its agent is free and its dependents may become ready.
        while running
            .peek()
            .is_some_and(|Reverse(r)| r.end.total_cmp(&now).is_eq())
        {
            let Some(Reverse(done)) = running.pop() else {
                break;
            };
            free.push(Reverse(done.agent));
            let dependents: BTreeSet<usize> = g
                .dependents
                .get(done.position)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            for dependent in dependents {
                if let Some(degree) = indegree.get_mut(dependent) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        ready.extend(ready_entry(g, &effective, dependent).map(Reverse));
                    }
                }
            }
        }
    }

    let makespan = lanes
        .iter()
        .filter_map(|lane| lane.last())
        .map(|slot| slot.end)
        .fold(0.0_f64, f64::max);
    let sizes: Vec<f64> = g.tasks.iter().map(|task| task.size).collect();
    let total = py_fsum(&sizes);
    let critical = g.bottom_levels().into_iter().fold(0.0_f64, f64::max);
    let lower_bound = (total / agents as f64).max(critical);
    Schedule {
        lanes,
        makespan,
        lower_bound,
    }
}

fn ready_entry<'a>(g: &'a Graph, effective: &'a [Key], position: usize) -> Option<Ready<'a>> {
    Some(Ready {
        key: effective.get(position)?,
        task: g.tasks.get(position)?,
        position,
    })
}

/// A task waiting to start. Smaller effective key first, then smaller id (byte order).
struct Ready<'a> {
    key: &'a Key,
    task: &'a Task,
    position: usize,
}

impl Ord for Ready<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_key(self.key, other.key).then_with(|| self.task.id.cmp(&other.task.id))
    }
}

impl PartialOrd for Ready<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ready<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Ready<'_> {}

/// A task on an agent. Earlier end first, then lower agent index, then smaller id.
struct Running<'a> {
    end: f64,
    agent: usize,
    task: &'a Task,
    position: usize,
}

impl Ord for Running<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.end
            .total_cmp(&other.end)
            .then_with(|| self.agent.cmp(&other.agent))
            .then_with(|| self.task.id.cmp(&other.task.id))
    }
}

impl PartialOrd for Running<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Running<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Running<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Queued;

    fn task(id: &str, size: f64, after: &[&str], files: &[&str]) -> Task {
        Task {
            index: 0,
            id: id.to_string(),
            title: String::new(),
            kind: "other".to_string(),
            tier: 1,
            queued: None,
            after: after.iter().map(|s| s.to_string()).collect(),
            size,
            value: 1.0,
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Numbers the tasks by their position in the list, as load_tasks does, then builds the graph.
    fn graph(tasks: Vec<Task>) -> Graph {
        let numbered = tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| Task { index, ..task })
            .collect();
        Graph::new(numbered).expect("valid graph")
    }

    fn spans<'a>(g: &'a Graph, lane: &[Slot]) -> Vec<(&'a str, f64, f64)> {
        lane.iter()
            .map(|slot| (g.tasks[slot.task].id.as_str(), slot.start, slot.end))
            .collect()
    }

    fn start_of(g: &Graph, sched: &Schedule, id: &str) -> Option<f64> {
        let position = *g.index.get(id)?;
        sched
            .lanes
            .iter()
            .flatten()
            .find(|slot| slot.task == position)
            .map(|slot| slot.start)
    }

    /// Deterministic linear congruential generator for the random graphs (no external crates).
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % bound
        }
    }

    const FILES: [&str; 6] = ["src", "src/a", "src/a/b.rs", "src/ab", "docs", ""];

    /// Task t<i> may follow any earlier task, so the graph is acyclic by construction.
    fn random_graph(rng: &mut Lcg, with_files: bool) -> Graph {
        let count = 1 + rng.below(12) as usize;
        let mut tasks = Vec::with_capacity(count);
        for index in 0..count {
            let mut after = Vec::new();
            for earlier in 0..index {
                if rng.below(4) == 0 {
                    after.push(format!("t{earlier}"));
                }
            }
            let mut files = Vec::new();
            if with_files {
                for _ in 0..rng.below(3) {
                    files.push(FILES[rng.below(FILES.len() as u64) as usize].to_string());
                }
            }
            tasks.push(Task {
                index,
                id: format!("t{index}"),
                title: String::new(),
                kind: "other".to_string(),
                tier: if rng.below(3) == 0 { 0 } else { 1 },
                queued: (rng.below(4) == 0).then(|| Queued::Int(rng.below(5) as i64)),
                after,
                size: (1 + rng.below(30)) as f64 / 10.0,
                value: (1 + rng.below(9)) as f64,
                files,
            });
        }
        Graph::new(tasks).expect("random graphs are acyclic")
    }

    /// The ScheduleTests invariants: lanes do not overlap, each task is placed once with duration equal to its size,
    /// prerequisites end before their dependents start, conflicting tasks never overlap, and the makespan is at least
    /// the lower bound.
    fn assert_valid(g: &Graph, sched: &Schedule, agents: usize) {
        assert_eq!(sched.lanes.len(), agents);
        for lane in &sched.lanes {
            for pair in lane.windows(2) {
                if let [before, after] = pair {
                    assert!(
                        after.start >= before.end,
                        "lane overlap: {before:?} then {after:?}"
                    );
                }
            }
        }

        let mut placed: Vec<Option<(f64, f64)>> = vec![None; g.tasks.len()];
        let mut slots = 0;
        for slot in sched.lanes.iter().flatten() {
            slots += 1;
            let cell = placed.get_mut(slot.task).expect("slot names a task");
            assert!(cell.is_none(), "task {} placed twice", slot.task);
            *cell = Some((slot.start, slot.end));
            let size = g.tasks[slot.task].size;
            assert!(
                (slot.end - slot.start - size).abs() <= 1e-9 * (1.0 + size),
                "duration of task {} is not its size",
                slot.task
            );
        }
        assert_eq!(slots, g.tasks.len());
        let at: Vec<(f64, f64)> = placed
            .into_iter()
            .map(|cell| cell.expect("every task is placed"))
            .collect();

        for (task, &(start, _)) in g.tasks.iter().zip(&at) {
            for prerequisite in &task.after {
                let before = *g.index.get(prerequisite).expect("known prerequisite");
                assert!(
                    at[before].1 <= start,
                    "{} starts before {prerequisite} ends",
                    task.id
                );
            }
        }

        for (i, a) in g.tasks.iter().enumerate() {
            for (j, b) in g.tasks.iter().enumerate().skip(i + 1) {
                if conflict(a, b) {
                    let (start_a, end_a) = at[i];
                    let (start_b, end_b) = at[j];
                    assert!(
                        end_a <= start_b || end_b <= start_a,
                        "{} and {} overlap while sharing files",
                        a.id,
                        b.id
                    );
                }
            }
        }

        assert!(sched.makespan >= sched.lower_bound - 1e-9);
    }

    #[test]
    fn example_b_packs_c_behind_a_on_the_first_lane() {
        let g = graph(vec![
            task("a", 2.0, &[], &[]),
            task("b", 1.0, &[], &[]),
            task("c", 1.0, &["a"], &[]),
        ]);
        let sched = schedule(&g, 2, OrderPolicy::Fifo);
        assert_eq!(
            spans(&g, &sched.lanes[0]),
            vec![("a", 0.0, 2.0), ("c", 2.0, 3.0)]
        );
        assert_eq!(spans(&g, &sched.lanes[1]), vec![("b", 0.0, 1.0)]);
        assert_eq!(sched.makespan, 3.0);
        assert_eq!(sched.lower_bound, 3.0);
    }

    #[test]
    fn overlapping_files_never_run_together() {
        let g = graph(vec![
            task("a", 1.0, &[], &["src/app"]),
            task("b", 1.0, &[], &["src/app/view.swift"]),
            task("c", 1.0, &[], &["docs"]),
        ]);
        let sched = schedule(&g, 3, OrderPolicy::Fifo);
        assert_eq!(start_of(&g, &sched, "a"), Some(0.0));
        assert_eq!(start_of(&g, &sched, "c"), Some(0.0));
        assert_eq!(start_of(&g, &sched, "b"), Some(1.0));
    }

    #[test]
    fn tasks_sharing_a_file_run_one_at_a_time() {
        let g = graph(vec![
            task("a", 1.0, &[], &["x"]),
            task("b", 2.0, &[], &["x"]),
            task("c", 3.0, &[], &["x"]),
        ]);
        let sched = schedule(&g, 3, OrderPolicy::Fifo);
        assert_eq!(start_of(&g, &sched, "a"), Some(0.0));
        assert_eq!(start_of(&g, &sched, "b"), Some(1.0));
        assert_eq!(start_of(&g, &sched, "c"), Some(3.0));
        assert_eq!(sched.makespan, 6.0);
    }

    #[test]
    fn one_agent_runs_the_single_order() {
        let mut rng = Lcg(3);
        for _ in 0..50 {
            let g = random_graph(&mut rng, true);
            let sched = schedule(&g, 1, OrderPolicy::Fifo);
            let ran: Vec<usize> = sched
                .lanes
                .first()
                .into_iter()
                .flatten()
                .map(|slot| slot.task)
                .collect();
            assert_eq!(ran, order(&g, OrderPolicy::Fifo).order);
        }
    }

    #[test]
    fn one_agent_makespan_is_the_total_size() {
        let g = graph(vec![
            task("a", 0.1, &[], &[]),
            task("b", 0.2, &[], &[]),
            task("c", 0.3, &[], &[]),
        ]);
        let sched = schedule(&g, 1, OrderPolicy::Fifo);
        assert!((sched.makespan - 0.6).abs() <= 1e-9);
        assert!((sched.lower_bound - 0.6).abs() <= 1e-9);
    }

    #[test]
    fn empty_graph_has_no_work_and_zero_bounds() {
        let g = graph(Vec::new());
        let sched = schedule(&g, 3, OrderPolicy::CriticalPath);
        assert_eq!(sched.lanes, vec![Vec::<Slot>::new(); 3]);
        assert_eq!(sched.makespan, 0.0);
        assert_eq!(sched.lower_bound, 0.0);
    }

    #[test]
    fn zero_agents_gives_no_lanes_instead_of_dividing_by_zero() {
        let g = graph(vec![task("a", 1.0, &[], &[])]);
        let sched = schedule(&g, 0, OrderPolicy::Fifo);
        assert!(sched.lanes.is_empty());
        assert_eq!(sched.makespan, 0.0);
        assert_eq!(sched.lower_bound, 0.0);
    }

    #[test]
    fn check_agents_accepts_one_to_max_agents_only() {
        assert_eq!(check_agents(1), Ok(1));
        assert_eq!(check_agents(MAX_AGENTS), Ok(MAX_AGENTS));
        for n in [0, MAX_AGENTS + 1, 1000] {
            assert_eq!(
                check_agents(n),
                Err("agents must be between 1 and 256".to_string()),
                "{n}"
            );
        }
    }

    #[test]
    fn schedule_clamps_a_huge_agent_count_to_max_agents() {
        let g = graph(vec![task("a", 1.0, &[], &[]), task("b", 2.0, &[], &[])]);
        let sched = schedule(&g, 1000, OrderPolicy::Fifo);
        assert_eq!(sched.lanes.len(), MAX_AGENTS);
        assert_eq!(sched.makespan, 2.0);
        assert_eq!(sched.lower_bound, 2.0);
    }

    #[test]
    fn random_graphs_satisfy_the_schedule_invariants() {
        let mut rng = Lcg(42);
        for _ in 0..100 {
            let g = random_graph(&mut rng, true);
            for agents in [1, 2, 3, 5] {
                for policy in [
                    OrderPolicy::Fifo,
                    OrderPolicy::Wsjf,
                    OrderPolicy::CriticalPath,
                ] {
                    let sched = schedule(&g, agents, policy);
                    assert_valid(&g, &sched, agents);
                }
            }
        }
    }

    #[test]
    fn list_scheduling_meets_grahams_bound_without_files() {
        let mut rng = Lcg(11);
        for _ in 0..100 {
            let g = random_graph(&mut rng, false);
            let total: f64 = g.tasks.iter().map(|task| task.size).sum();
            let critical = g.bottom_levels().into_iter().fold(0.0_f64, f64::max);
            for agents in [1_usize, 2, 4] {
                let k = agents as f64;
                let sched = schedule(&g, agents, OrderPolicy::CriticalPath);
                assert!(
                    sched.makespan <= total / k + (1.0 - 1.0 / k) * critical + 1e-9,
                    "makespan {} exceeds Graham's bound on {agents} agents",
                    sched.makespan
                );
            }
        }
    }
}
