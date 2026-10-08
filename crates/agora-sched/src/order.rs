//! Ordering policies: fifo, wsjf, critical-path (ref-ordering section 4).

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};

use crate::graph::Graph;
use crate::task::{Queued, Task};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum OrderPolicy {
    #[default]
    Fifo,
    Wsjf,
    CriticalPath,
}

impl OrderPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fifo" => Some(OrderPolicy::Fifo),
            "wsjf" => Some(OrderPolicy::Wsjf),
            "critical-path" => Some(OrderPolicy::CriticalPath),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OrderPolicy::Fifo => "fifo",
            OrderPolicy::Wsjf => "wsjf",
            OrderPolicy::CriticalPath => "critical-path",
        }
    }
}

/// (tier, score, arrival): fifo score = 0.0; wsjf score = -value/size; critical-path score = -bottom_level.
/// arrival = (0, queued, index) | (1, index, index).
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub tier: u8,
    pub score: f64,
    pub arrival_class: u8,
    pub queued: Option<Queued>,
    pub index: usize,
}

/// Total order on keys, smaller is sooner: tier, score, arrival class, queued, index.
///
/// Scores compare with `partial_cmp`, as Python does, so -0.0 ties with 0.0. A NaN score compares equal to every
/// score (the reference defines no NaN order), so the arrival fields decide.
pub fn cmp_key(a: &Key, b: &Key) -> Ordering {
    a.tier
        .cmp(&b.tier)
        .then_with(|| cmp_f64(a.score, b.score))
        .then_with(|| a.arrival_class.cmp(&b.arrival_class))
        .then_with(|| cmp_queued(a.queued.as_ref(), b.queued.as_ref()))
        .then_with(|| a.index.cmp(&b.index))
}

fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Numbers compare numerically and text by code point. Int against Float goes through f64, so two integers above
/// 2^53 may tie. Numbers sort before text and a missing arrival sorts last; load_tasks never mixes these kinds.
fn cmp_queued(a: Option<&Queued>, b: Option<&Queued>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(Queued::Text(x)), Some(Queued::Text(y))) => x.cmp(y),
        (Some(Queued::Text(_)), Some(_)) => Ordering::Greater,
        (Some(_), Some(Queued::Text(_))) => Ordering::Less,
        (Some(Queued::Int(x)), Some(Queued::Int(y))) => x.cmp(y),
        (Some(Queued::Int(x)), Some(Queued::Float(y))) => cmp_f64(*x as f64, *y),
        (Some(Queued::Float(x)), Some(Queued::Int(y))) => cmp_f64(*x, *y as f64),
        (Some(Queued::Float(x)), Some(Queued::Float(y))) => cmp_f64(*x, *y),
    }
}

#[derive(Debug, Clone)]
pub struct Ordered {
    pub order: Vec<usize>,
    pub effective: Vec<Key>,
    /// source[t] = descendant whose own key moved t up (strictly smaller), else None.
    pub source: Vec<Option<usize>>,
}

/// One agent's order under `policy`: the most urgent task whose prerequisites are done (ref-ordering 4.3-4.5).
pub fn order(g: &Graph, policy: OrderPolicy) -> Ordered {
    let own: Vec<Key> = g
        .tasks
        .iter()
        .zip(g.bottom_levels())
        .map(|(task, level)| own_key(task, level, policy))
        .collect();
    let (effective, source) = inherit(g, own);
    let order = kahn(g, &effective);
    Ordered {
        order,
        effective,
        source,
    }
}

/// A task's own priority tuple before inheritance. The arrival class is 0 with `queued` (sorts first) and 1 without.
fn own_key(task: &Task, level: f64, policy: OrderPolicy) -> Key {
    let score = match policy {
        OrderPolicy::Fifo => 0.0,
        OrderPolicy::Wsjf => -task.value / task.size,
        OrderPolicy::CriticalPath => -level,
    };
    Key {
        tier: task.tier,
        score,
        arrival_class: u8::from(task.queued.is_none()),
        queued: task.queued.clone(),
        index: task.index,
    }
}

/// Priority inheritance (ref-ordering 4.4): walking prerequisites after their dependents, a task takes the smallest
/// key among its dependents. `source` records the descendant the key came from.
fn inherit(g: &Graph, own: Vec<Key>) -> (Vec<Key>, Vec<Option<usize>>) {
    let mut effective = own;
    let mut source: Vec<Option<usize>> = vec![None; effective.len()];
    for &task in g.topo.iter().rev() {
        for &dependent in g.dependents.get(task).into_iter().flatten() {
            let (Some(candidate), Some(current)) = (effective.get(dependent), effective.get(task))
            else {
                continue;
            };
            if cmp_key(candidate, current) != Ordering::Less {
                continue;
            }
            let inherited = source.get(dependent).copied().flatten().or(Some(dependent));
            let candidate = candidate.clone();
            if let (Some(slot), Some(from)) = (effective.get_mut(task), source.get_mut(task)) {
                *slot = candidate;
                *from = inherited;
            }
        }
    }
    (effective, source)
}

/// Kahn's algorithm with a min-heap on (effective key, id). Each distinct prerequisite is counted once and each
/// distinct dependent is released once, so repeated `after` entries change nothing.
fn kahn(g: &Graph, effective: &[Key]) -> Vec<usize> {
    let count = g.tasks.len();
    let mut indegree: Vec<usize> = g
        .tasks
        .iter()
        .map(|task| task.after.iter().collect::<HashSet<_>>().len())
        .collect();
    let mut heap: BinaryHeap<Reverse<Ready<'_>>> = (0..count)
        .filter(|&position| indegree.get(position) == Some(&0))
        .filter_map(|position| ready_entry(g, effective, position))
        .map(Reverse)
        .collect();
    let mut result = Vec::with_capacity(count);
    while let Some(Reverse(next)) = heap.pop() {
        result.push(next.position);
        let dependents: HashSet<usize> = g
            .dependents
            .get(next.position)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for dependent in dependents {
            let Some(degree) = indegree.get_mut(dependent) else {
                continue;
            };
            *degree = degree.saturating_sub(1);
            if *degree == 0 {
                heap.extend(ready_entry(g, effective, dependent).map(Reverse));
            }
        }
    }
    result
}

fn ready_entry<'a>(g: &'a Graph, effective: &'a [Key], position: usize) -> Option<Ready<'a>> {
    let task = g.tasks.get(position)?;
    let key = effective.get(position)?;
    Some(Ready {
        key,
        id: task.id.as_str(),
        position,
    })
}

/// A task waiting in Kahn's heap. Smaller effective key first, then smaller id (byte order).
struct Ready<'a> {
    key: &'a Key,
    id: &'a str,
    position: usize,
}

impl Ord for Ready<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_key(self.key, other.key).then_with(|| self.id.cmp(other.id))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, tier: u8, after: &[&str]) -> Task {
        Task {
            index: 0,
            id: id.to_string(),
            title: String::new(),
            kind: "other".to_string(),
            tier,
            queued: None,
            after: after.iter().map(|s| s.to_string()).collect(),
            size: 1.0,
            value: 1.0,
            files: Vec::new(),
        }
    }

    fn queued(id: &str, arrival: Queued) -> Task {
        Task {
            queued: Some(arrival),
            ..task(id, 1, &[])
        }
    }

    fn sized(id: &str, size: f64, value: f64) -> Task {
        Task {
            size,
            value,
            ..task(id, 1, &[])
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

    fn ids<'a>(g: &'a Graph, positions: &[usize]) -> Vec<&'a str> {
        positions
            .iter()
            .filter_map(|&p| g.tasks.get(p))
            .map(|t| t.id.as_str())
            .collect()
    }

    fn key(tier: u8, score: f64, arrival_class: u8, queued: Option<Queued>, index: usize) -> Key {
        Key {
            tier,
            score,
            arrival_class,
            queued,
            index,
        }
    }

    #[test]
    fn fifo_keeps_arrival_order() {
        let g = graph(vec![
            queued("c", Queued::Int(3)),
            queued("a", Queued::Int(1)),
            queued("b", Queued::Int(2)),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["a", "b", "c"]);
    }

    #[test]
    fn input_order_is_the_default_arrival() {
        let g = graph(vec![task("second", 1, &[]), task("first", 1, &[])]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["second", "first"]);
    }

    #[test]
    fn queued_tasks_come_before_unqueued_ones() {
        let g = graph(vec![task("plain", 1, &[]), queued("dated", Queued::Int(5))]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["dated", "plain"]);
    }

    #[test]
    fn whole_and_fractional_arrivals_compare_numerically() {
        let g = graph(vec![
            queued("a", Queued::Int(2)),
            queued("b", Queued::Float(1.5)),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["b", "a"]);
    }

    #[test]
    fn iso_timestamps_order_by_time() {
        let g = graph(vec![
            queued("late", Queued::Text("2026-09-24T11:00".to_string())),
            queued("early", Queued::Text("2026-09-24T09:30".to_string())),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["early", "late"]);
    }

    #[test]
    fn validity_goes_before_older_work() {
        let g = graph(vec![
            task("perf", 1, &[]),
            task("docs", 1, &[]),
            task("crash", 0, &[]),
            task("wrong", 0, &[]),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(
            ids(&g, &ordered.order),
            vec!["crash", "wrong", "perf", "docs"]
        );
    }

    #[test]
    fn a_task_comes_after_its_prerequisites() {
        let g = graph(vec![
            task("use", 1, &["build"]),
            task("build", 1, &["fetch"]),
            task("fetch", 1, &[]),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["fetch", "build", "use"]);
    }

    #[test]
    fn a_prerequisite_of_a_validity_task_moves_up_with_it() {
        let g = graph(vec![
            task("old", 1, &[]),
            task("refactor", 1, &[]),
            task("fix", 0, &["refactor"]),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["refactor", "fix", "old"]);
        assert_eq!(ordered.source[g.index["refactor"]], Some(g.index["fix"]));
        assert_eq!(ordered.source[g.index["old"]], None);
    }

    #[test]
    fn source_names_the_descendant_holding_the_smallest_key() {
        let g = graph(vec![
            task("a", 1, &[]),
            task("b", 1, &["a"]),
            task("c", 0, &["b"]),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["a", "b", "c"]);
        assert_eq!(ordered.source[g.index["a"]], Some(g.index["c"]));
        assert_eq!(ordered.source[g.index["b"]], Some(g.index["c"]));
        assert_eq!(ordered.source[g.index["c"]], None);
    }

    #[test]
    fn wsjf_puts_value_per_size_first_and_keeps_arrival_on_ties() {
        let g = graph(vec![
            sized("big", 8.0, 8.0),
            sized("quick", 1.0, 4.0),
            sized("tie", 8.0, 8.0),
            sized("low", 4.0, 1.0),
        ]);
        let ordered = order(&g, OrderPolicy::Wsjf);
        assert_eq!(ids(&g, &ordered.order), vec!["quick", "big", "tie", "low"]);
    }

    #[test]
    fn wsjf_still_puts_validity_first() {
        let g = graph(vec![
            sized("cheap", 1.0, 100.0),
            Task {
                tier: 0,
                ..sized("bug", 10.0, 1.0)
            },
        ]);
        let ordered = order(&g, OrderPolicy::Wsjf);
        assert_eq!(ids(&g, &ordered.order), vec!["bug", "cheap"]);
    }

    #[test]
    fn critical_path_prefers_the_longest_chain() {
        let g = graph(vec![
            sized("short", 3.0, 1.0),
            sized("head", 1.0, 1.0),
            Task {
                after: vec!["head".to_string()],
                ..sized("tail", 5.0, 1.0)
            },
        ]);
        assert_eq!(g.bottom_levels()[g.index["head"]], 6.0);
        let ordered = order(&g, OrderPolicy::CriticalPath);
        assert_eq!(ids(&g, &ordered.order), vec!["head", "tail", "short"]);
    }

    #[test]
    fn ties_on_inherited_keys_break_by_id_bytes() {
        let g = graph(vec![
            task("zeta", 1, &[]),
            task("alpha", 1, &[]),
            task("Mid", 1, &[]),
            task("x", 0, &["zeta", "alpha", "Mid"]),
            task("plain", 1, &[]),
        ]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(
            ids(&g, &ordered.order),
            vec!["Mid", "alpha", "zeta", "x", "plain"]
        );
        assert_eq!(
            ordered.effective[g.index["zeta"]],
            ordered.effective[g.index["x"]]
        );
        assert_eq!(
            ordered.effective[g.index["Mid"]],
            ordered.effective[g.index["alpha"]]
        );
    }

    #[test]
    fn repeated_prerequisites_count_once() {
        let g = graph(vec![task("a", 1, &[]), task("b", 1, &["a", "a"])]);
        let ordered = order(&g, OrderPolicy::Fifo);
        assert_eq!(ids(&g, &ordered.order), vec!["a", "b"]);
    }

    #[test]
    fn empty_graph_gives_an_empty_order() {
        let g = graph(Vec::new());
        let ordered = order(&g, OrderPolicy::CriticalPath);
        assert!(ordered.order.is_empty());
        assert!(ordered.effective.is_empty());
        assert!(ordered.source.is_empty());
    }

    #[test]
    fn cmp_key_puts_numbers_before_text_and_nan_ties_fall_through() {
        let number = key(1, 0.0, 0, Some(Queued::Int(9)), 0);
        let text = key(1, 0.0, 0, Some(Queued::Text("2026".to_string())), 1);
        assert_eq!(cmp_key(&number, &text), Ordering::Less);
        assert_eq!(cmp_key(&text, &number), Ordering::Greater);

        let nan = key(1, f64::NAN, 1, None, 5);
        let zero = key(1, 0.0, 1, None, 2);
        assert_eq!(cmp_key(&nan, &zero), Ordering::Greater);
        assert_eq!(cmp_key(&zero, &nan), Ordering::Less);
    }
}
