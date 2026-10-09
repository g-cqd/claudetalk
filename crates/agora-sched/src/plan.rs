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
mod tests;
