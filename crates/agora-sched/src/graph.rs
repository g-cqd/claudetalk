//! Dependency graph over a task list, bottom levels and file-overlap predicates.

use std::collections::{HashMap, HashSet};

use crate::pyfmt::repr_str;
use crate::task::{InputError, Task};

#[derive(Debug, Clone)]
pub struct Graph {
    pub tasks: Vec<Task>,
    pub index: HashMap<String, usize>,
    /// dependents[p] = task indices having tasks[p].id in `after`, input order, duplicates kept.
    pub dependents: Vec<Vec<usize>>,
    /// a topological order (Kahn, any tie rule).
    pub topo: Vec<usize>,
}

impl Graph {
    /// duplicate / unknown / self / cycle errors with the exact messages and cycle path of ref-ordering section 3.
    pub fn new(tasks: Vec<Task>) -> Result<Graph, InputError> {
        let mut index = HashMap::with_capacity(tasks.len());
        for (position, task) in tasks.iter().enumerate() {
            if index.insert(task.id.clone(), position).is_some() {
                return Err(InputError(format!(
                    "duplicate task id {}",
                    repr_str(&task.id)
                )));
            }
        }

        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];
        for (position, task) in tasks.iter().enumerate() {
            for prerequisite in &task.after {
                let Some(&before) = index.get(prerequisite) else {
                    return Err(InputError(format!(
                        "task {} is after unknown task {}",
                        repr_str(&task.id),
                        repr_str(prerequisite)
                    )));
                };
                if *prerequisite == task.id {
                    return Err(InputError(format!(
                        "task {} is after itself",
                        repr_str(&task.id)
                    )));
                }
                if let Some(list) = dependents.get_mut(before) {
                    list.push(position);
                }
            }
        }

        // Kahn's algorithm. The in-degree counts every `after` entry, and each dependents entry is one
        // occurrence, so repeated prerequisites cancel out and the result matches the distinct-set rule.
        let mut indegree: Vec<usize> = tasks.iter().map(|task| task.after.len()).collect();
        let mut ready: Vec<usize> = (0..tasks.len())
            .filter(|&position| indegree.get(position) == Some(&0))
            .collect();
        let mut topo = Vec::with_capacity(tasks.len());
        while let Some(current) = ready.pop() {
            topo.push(current);
            for &dependent in dependents.get(current).into_iter().flatten() {
                if let Some(degree) = indegree.get_mut(dependent) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        ready.push(dependent);
                    }
                }
            }
        }

        if topo.len() < tasks.len() {
            let ordered: HashSet<usize> = topo.iter().copied().collect();
            let remaining: HashSet<usize> = (0..tasks.len())
                .filter(|position| !ordered.contains(position))
                .collect();
            // Every remaining task has a prerequisite that is also remaining, so the walk always finds a cycle.
            // The fallback lists the remaining ids and is unreachable in practice.
            let cycle: Vec<&str> = match cycle_path(&tasks, &index, &remaining) {
                Some(path) => path.iter().filter_map(|&p| id_at(&tasks, p)).collect(),
                None => {
                    let mut ids: Vec<&str> =
                        remaining.iter().filter_map(|&p| id_at(&tasks, p)).collect();
                    ids.sort_unstable();
                    ids
                }
            };
            return Err(InputError(format!(
                "the tasks form a cycle: {}",
                cycle.join(" -> ")
            )));
        }

        Ok(Graph {
            tasks,
            index,
            dependents,
            topo,
        })
    }

    /// level[t] = size[t] + max(level[d] for d in dependents[t], default 0.0).
    pub fn bottom_levels(&self) -> Vec<f64> {
        let mut levels = vec![0.0_f64; self.tasks.len()];
        for &current in self.topo.iter().rev() {
            let longest = self
                .dependents
                .get(current)
                .into_iter()
                .flatten()
                .filter_map(|&dependent| levels.get(dependent))
                .copied()
                .fold(0.0_f64, f64::max);
            let size = self.tasks.get(current).map_or(0.0, |task| task.size);
            if let Some(slot) = levels.get_mut(current) {
                *slot = size + longest;
            }
        }
        levels
    }
}

fn id_at(tasks: &[Task], position: usize) -> Option<&str> {
    tasks.get(position).map(|task| task.id.as_str())
}

/// A cycle among `remaining` (the tasks Kahn could not order), found by following prerequisites from the smallest
/// id. Returns the positions of the cycle, first repeated position at both ends.
fn cycle_path(
    tasks: &[Task],
    index: &HashMap<String, usize>,
    remaining: &HashSet<usize>,
) -> Option<Vec<usize>> {
    let start = remaining
        .iter()
        .copied()
        .min_by_key(|&position| id_at(tasks, position))?;
    let mut path = vec![start];
    let mut seen: HashMap<usize, usize> = HashMap::from([(start, 0)]);
    loop {
        let last = *path.last()?;
        let following = tasks
            .get(last)?
            .after
            .iter()
            .filter_map(|prerequisite| index.get(prerequisite).copied())
            .filter(|position| remaining.contains(position))
            .min_by_key(|&position| id_at(tasks, position))?;
        if let Some(&first) = seen.get(&following) {
            let mut cycle = path.get(first..)?.to_vec();
            cycle.push(following);
            return Some(cycle);
        }
        seen.insert(following, path.len());
        path.push(following);
    }
}

/// strip all trailing '/' from both; equal, or one starts with the other + "/".
pub fn overlap(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

/// same id, or any file pair overlaps.
pub fn conflict(a: &Task, b: &Task) -> bool {
    a.id == b.id
        || a.files
            .iter()
            .any(|x| b.files.iter().any(|y| overlap(x, y)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, after: &[&str], size: f64, files: &[&str]) -> Task {
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

    fn plain(id: &str, after: &[&str]) -> Task {
        task(id, after, 1.0, &[])
    }

    fn error_text(tasks: Vec<Task>) -> String {
        Graph::new(tasks).expect_err("an input error").0
    }

    #[test]
    fn overlap_respects_path_boundaries() {
        assert!(!overlap("src/a", "src/ab"));
        assert!(!overlap("src/ab", "src/a"));
        assert!(overlap("src/a/", "src/a"));
        assert!(overlap("src/a", "src/a/deep/f.rs"));
        assert!(overlap("/", "/etc"));
        assert!(!overlap("/", "etc"));
        assert!(overlap("", ""));
        assert!(!overlap("./src/a", "src/a"));
    }

    #[test]
    fn conflict_by_id_or_overlapping_files() {
        assert!(conflict(&plain("a", &[]), &plain("a", &[])));
        let src = task("a", &[], 1.0, &["src/app"]);
        let view = task("b", &[], 1.0, &["src/app/view.swift"]);
        let docs = task("c", &[], 1.0, &["docs"]);
        assert!(conflict(&src, &view));
        assert!(!conflict(&src, &docs));
        assert!(!conflict(&plain("x", &[]), &plain("y", &[])));
        let empty_a = task("p", &[], 1.0, &[""]);
        let empty_b = task("q", &[], 1.0, &[""]);
        assert!(conflict(&empty_a, &empty_b));
    }

    #[test]
    fn bottom_level_adds_the_longest_dependent_chain() {
        let graph = Graph::new(vec![
            task("head", &[], 1.0, &[]),
            task("tail", &["head"], 5.0, &[]),
        ])
        .expect("valid graph");
        assert_eq!(graph.bottom_levels(), vec![6.0, 5.0]);
    }

    #[test]
    fn topological_order_puts_prerequisites_first() {
        let graph = Graph::new(vec![
            plain("use", &["build"]),
            plain("build", &["fetch"]),
            plain("fetch", &[]),
        ])
        .expect("valid graph");
        let ids: Vec<&str> = graph
            .topo
            .iter()
            .filter_map(|&p| id_at(&graph.tasks, p))
            .collect();
        assert_eq!(ids, vec!["fetch", "build", "use"]);
    }

    #[test]
    fn dependents_keep_input_order_and_repeats() {
        let graph = Graph::new(vec![
            plain("a", &[]),
            plain("b", &["a", "a"]),
            plain("c", &["a"]),
        ])
        .expect("valid graph");
        assert_eq!(graph.dependents[0], vec![1, 1, 2]);
        assert_eq!(graph.index.get("c"), Some(&2));
    }

    #[test]
    fn empty_graph_is_valid() {
        let graph = Graph::new(Vec::new()).expect("valid graph");
        assert!(graph.topo.is_empty());
        assert!(graph.bottom_levels().is_empty());
    }

    #[test]
    fn duplicate_id_is_reported_before_unknown_prerequisites() {
        let text = error_text(vec![plain("a", &["ghost"]), plain("a", &[])]);
        assert_eq!(text, "duplicate task id 'a'");
    }

    #[test]
    fn unknown_prerequisite_is_reported_in_after_order() {
        let text = error_text(vec![plain("a", &["ghost"])]);
        assert_eq!(text, "task 'a' is after unknown task 'ghost'");
        let text = error_text(vec![plain("a", &["ghost", "a"])]);
        assert_eq!(text, "task 'a' is after unknown task 'ghost'");
    }

    #[test]
    fn self_reference_is_reported_after_earlier_entries() {
        let text = error_text(vec![plain("a", &["a"])]);
        assert_eq!(text, "task 'a' is after itself");
        let text = error_text(vec![plain("a", &["a", "ghost"])]);
        assert_eq!(text, "task 'a' is after itself");
    }

    #[test]
    fn cycle_path_follows_prerequisites_from_the_smallest_id() {
        let text = error_text(vec![
            plain("a", &["c"]),
            plain("b", &["a"]),
            plain("c", &["b"]),
            plain("free", &[]),
        ]);
        assert_eq!(text, "the tasks form a cycle: a -> c -> b -> a");
    }

    #[test]
    fn cycle_path_starts_at_the_cycle_not_downstream_tasks() {
        let text = error_text(vec![
            plain("a", &["z", "c"]),
            plain("b", &["c"]),
            plain("c", &["b"]),
            plain("z", &["c"]),
            plain("ok", &[]),
        ]);
        assert_eq!(text, "the tasks form a cycle: c -> b -> c");
    }
}
