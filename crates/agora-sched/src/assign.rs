//! Congestion game + best-response dynamics for file-conflicting tasks (ref-ordering section 4.7).

use crate::graph::{Graph, conflict};
use crate::order::{OrderPolicy, order};
use crate::plan::MAX_AGENTS;
use crate::pyfmt::py_fsum;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PyNum {
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub choices: Vec<Option<usize>>,
    pub costs: Vec<PyNum>,
    pub potential: PyNum,
    pub rounds: u32,
}

/// One option for an agent: a root task (None = idle) and its position in the ranking. The position of idle is the
/// number of ranked roots, which is also the idle cost.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    task: Option<usize>,
    pos: i64,
}

/// The game for one graph: the costs of idle, duplicates and conflicts, and the ranked roots.
struct Game<'a> {
    graph: &'a Graph,
    idle: f64,
    duplicate: f64,
    penalty: f64,
    none: Candidate,
    ranked: Vec<Candidate>,
}

impl<'a> Game<'a> {
    fn new(graph: &'a Graph, policy: OrderPolicy, conflict_cost: Option<f64>) -> Self {
        let roots: Vec<usize> = order(graph, policy)
            .order
            .into_iter()
            .filter(|&t| graph.tasks.get(t).is_some_and(|task| task.after.is_empty()))
            .collect();
        let count = i64::try_from(roots.len()).unwrap_or(i64::MAX);
        let ranked = roots
            .into_iter()
            .enumerate()
            .map(|(position, task)| Candidate {
                task: Some(task),
                pos: i64::try_from(position).unwrap_or(i64::MAX),
            })
            .collect();
        // Taking a task another agent took duplicates the work, which never beats idling, whatever the conflict cost.
        let duplicate = count.saturating_add(1) as f64;
        Game {
            graph,
            idle: count as f64,
            duplicate,
            penalty: conflict_cost.unwrap_or(duplicate),
            none: Candidate {
                task: None,
                pos: count,
            },
            ranked,
        }
    }

    /// clash(x, y): duplicate for the same task, penalty for file-conflicting tasks, 0.0 otherwise.
    fn clash(&self, first: usize, second: usize) -> f64 {
        if first == second {
            return self.duplicate;
        }
        match (self.graph.tasks.get(first), self.graph.tasks.get(second)) {
            (Some(a), Some(b)) if conflict(a, b) => self.penalty,
            _ => 0.0,
        }
    }

    /// cost(choice, others). Idle is a float. A task is its rank plus the clashes with the other agents' tasks; the
    /// sum of an empty list of clashes is the int 0, so the cost stays an int when nobody else has a task.
    fn cost(&self, candidate: Candidate, others: &[Candidate]) -> PyNum {
        let Some(task) = candidate.task else {
            return PyNum::Float(self.idle);
        };
        let terms: Vec<f64> = others
            .iter()
            .filter_map(|other| other.task)
            .map(|next| self.clash(task, next))
            .collect();
        if terms.is_empty() {
            PyNum::Int(candidate.pos)
        } else {
            PyNum::Float(candidate.pos as f64 + py_fsum(&terms))
        }
    }

    /// The selection key (cost, rank-or-idle) as f64 pair; comparisons go through `key_lt`.
    fn key(&self, candidate: Candidate, others: &[Candidate]) -> (f64, f64) {
        (as_f64(self.cost(candidate, others)), candidate.pos as f64)
    }

    /// The minimum of the key over idle and the ranked roots. Keeps the first minimum, as Python's min does.
    fn best(&self, others: &[Candidate]) -> Candidate {
        let mut best = self.none;
        let mut best_key = self.key(best, others);
        for &candidate in &self.ranked {
            let key = self.key(candidate, others);
            if key_lt(key, best_key) {
                best = candidate;
                best_key = key;
            }
        }
        best
    }
}

/// Python tuple `<` on (cost, rank-or-idle): the first component decides unless the two are equal.
fn key_lt(a: (f64, f64), b: (f64, f64)) -> bool {
    if a.0 == b.0 { a.1 < b.1 } else { a.0 < b.0 }
}

fn as_f64(num: PyNum) -> f64 {
    match num {
        PyNum::Int(value) => value as f64,
        PyNum::Float(value) => value,
    }
}

/// The other agents' candidates, in agent order (`choices[:agent] + choices[agent + 1:]`).
fn others_of(choices: &[Candidate], agent: usize) -> Vec<Candidate> {
    choices
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != agent)
        .map(|(_, candidate)| *candidate)
        .collect()
}

/// The most best-response passes `assign` runs (S-6). Beyond it `assign` returns the choices reached so far, which need
/// not be an equilibrium; `rounds` still counts the passes run.
const MAX_ROUNDS: u32 = 10_000;

/// Best-response dynamics over `choices`: each pass lets every agent in turn switch to its best option against the
/// others. Stops after a pass with no change, or after `max_rounds` passes; returns the number of passes run (a
/// settled start takes 1, `max_rounds` 0 runs none and leaves `choices` as they were).
fn settle(game: &Game<'_>, choices: &mut [Candidate], max_rounds: u32) -> u32 {
    let mut changed = true;
    let mut rounds: u32 = 0;
    while changed && rounds < max_rounds {
        changed = false;
        rounds = rounds.saturating_add(1);
        for agent in 0..choices.len() {
            let others = others_of(choices, agent);
            let best = game.best(&others);
            let current = choices[agent];
            if as_f64(game.cost(best, &others)) < as_f64(game.cost(current, &others)) {
                choices[agent] = best;
                changed = true;
            }
        }
    }
    rounds
}

/// Congestion game + best-response dynamics, ref-ordering section 4.7 (int-vs-float typing of section 5.3, py_fsum sums).
/// Counts above `MAX_AGENTS` are clamped to it. Best-response passes stop at `MAX_ROUNDS`: beyond that the returned
/// choices are the ones reached, and an equilibrium is not guaranteed.
pub fn assign(
    g: &Graph,
    agents: usize,
    policy: OrderPolicy,
    conflict_cost: Option<f64>,
) -> Assignment {
    let agents = agents.min(MAX_AGENTS);
    // With zero agents the result is empty (the CLI refuses --agents 0 before this point).
    let game = Game::new(g, policy, conflict_cost);

    // Greedy start: each agent in turn takes its best option against the agents placed before it.
    let mut choices: Vec<Candidate> = Vec::with_capacity(agents);
    for _ in 0..agents {
        let pick = game.best(&choices);
        choices.push(pick);
    }

    // Best-response dynamics. A pass with no change is counted, so the fewest rounds is 1.
    let rounds = settle(&game, &mut choices, MAX_ROUNDS);

    // Interactions: clash over every pair of agents (first < second) that both chose a task.
    let mut pair_terms: Vec<f64> = Vec::new();
    for (first, a) in choices.iter().enumerate() {
        for b in choices.iter().skip(first + 1) {
            if let (Some(x), Some(y)) = (a.task, b.task) {
                pair_terms.push(game.clash(x, y));
            }
        }
    }
    let interactions = if pair_terms.is_empty() {
        None
    } else {
        Some(py_fsum(&pair_terms))
    };

    // Potential: the rank-or-idle of every agent, then the interactions. The first sum is an exact integer, so it is
    // an int when every agent chose a task.
    let idle_count = choices.iter().filter(|c| c.task.is_none()).count();
    let rank_sum: i64 = choices
        .iter()
        .filter(|c| c.task.is_some())
        .map(|c| c.pos)
        .fold(0_i64, i64::saturating_add);
    let idle_total = i64::try_from(idle_count)
        .unwrap_or(i64::MAX)
        .saturating_mul(game.none.pos);
    let first_sum = rank_sum.saturating_add(idle_total);
    let potential = match (idle_count == 0, interactions) {
        (true, None) => PyNum::Int(first_sum),
        (_, None) => PyNum::Float(first_sum as f64),
        (_, Some(sum)) => PyNum::Float(first_sum as f64 + sum),
    };

    let costs: Vec<PyNum> = choices
        .iter()
        .enumerate()
        .map(|(agent, &candidate)| game.cost(candidate, &others_of(&choices, agent)))
        .collect();

    Assignment {
        choices: choices.iter().map(|c| c.task).collect(),
        costs,
        potential,
        rounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Task;

    fn spec(id: &str, files: &[&str], after: &[&str]) -> Task {
        Task {
            index: 0,
            id: id.to_string(),
            title: String::new(),
            kind: "other".to_string(),
            tier: 1,
            queued: None,
            after: after.iter().map(|s| s.to_string()).collect(),
            size: 1.0,
            value: 1.0,
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn build(mut tasks: Vec<Task>) -> Graph {
        for (index, task) in tasks.iter_mut().enumerate() {
            task.index = index;
        }
        Graph::new(tasks).expect("test graph is valid")
    }

    /// The chosen task ids in agent order, None for idle.
    fn ids<'a>(g: &'a Graph, a: &Assignment) -> Vec<Option<&'a str>> {
        a.choices
            .iter()
            .map(|choice| {
                choice
                    .and_then(|t| g.tasks.get(t))
                    .map(|task| task.id.as_str())
            })
            .collect()
    }

    /// Example C of ref-ordering section 6: a(src), b(src/x), c(docs), d(after c).
    fn example_c() -> Graph {
        build(vec![
            spec("a", &["src"], &[]),
            spec("b", &["src/x"], &[]),
            spec("c", &["docs"], &[]),
            spec("d", &[], &["c"]),
        ])
    }

    #[test]
    fn example_c_matches_the_worked_example() {
        let g = example_c();
        let a = assign(&g, 3, OrderPolicy::Fifo, None);
        assert_eq!(ids(&g, &a), vec![Some("a"), Some("c"), None]);
        assert_eq!(
            a.costs,
            vec![PyNum::Float(0.0), PyNum::Float(2.0), PyNum::Float(3.0)]
        );
        assert_eq!(a.potential, PyNum::Float(5.0));
        assert_eq!(a.rounds, 1);
    }

    #[test]
    fn single_agent_takes_the_first_root_and_pays_an_int_zero() {
        let g = build(vec![spec("a", &[], &[]), spec("b", &[], &["a"])]);
        let a = assign(&g, 1, OrderPolicy::Fifo, None);
        assert_eq!(ids(&g, &a), vec![Some("a")]);
        assert_eq!(a.costs, vec![PyNum::Int(0)]);
        assert_eq!(a.potential, PyNum::Int(0));
        assert_eq!(a.rounds, 1);
    }

    #[test]
    fn a_low_conflict_cost_lets_conflicting_tasks_share() {
        let g = build(vec![spec("a", &["src"], &[]), spec("b", &["src/x"], &[])]);
        let a = assign(&g, 2, OrderPolicy::Fifo, Some(0.5));
        let mut chosen = ids(&g, &a);
        chosen.sort();
        assert_eq!(chosen, vec![Some("a"), Some("b")]);
        assert_eq!(a.costs, vec![PyNum::Float(0.5), PyNum::Float(1.5)]);
        assert_eq!(a.potential, PyNum::Float(1.5));
        assert_eq!(a.rounds, 1);
    }

    #[test]
    fn a_conflicting_task_loses_to_idling_at_the_default_penalty() {
        let g = build(vec![spec("a", &["src"], &[]), spec("b", &["src/x"], &[])]);
        let a = assign(&g, 2, OrderPolicy::Fifo, None);
        assert_eq!(ids(&g, &a), vec![Some("a"), None]);
        assert_eq!(a.costs, vec![PyNum::Int(0), PyNum::Float(2.0)]);
    }

    #[test]
    fn a_duplicate_never_beats_idling() {
        let g = build(vec![spec("a", &[], &[])]);
        let a = assign(&g, 2, OrderPolicy::Fifo, None);
        assert_eq!(ids(&g, &a), vec![Some("a"), None]);
        assert_eq!(a.costs, vec![PyNum::Int(0), PyNum::Float(1.0)]);
        assert_eq!(a.potential, PyNum::Float(1.0));
    }

    #[test]
    fn two_agents_on_independent_tasks_pay_float_costs() {
        let g = build(vec![spec("a", &[], &[]), spec("b", &[], &[])]);
        let a = assign(&g, 2, OrderPolicy::Fifo, None);
        assert_eq!(ids(&g, &a), vec![Some("a"), Some("b")]);
        assert_eq!(a.costs, vec![PyNum::Float(0.0), PyNum::Float(1.0)]);
        assert_eq!(a.potential, PyNum::Float(1.0));
    }

    #[test]
    fn zero_conflict_cost_with_three_agents_gives_distinct_tasks() {
        for cost in [0.0, 0.5, 100.0] {
            let g = build(vec![spec("a", &[], &[]), spec("b", &[], &[])]);
            let a = assign(&g, 3, OrderPolicy::Fifo, Some(cost));
            let chosen: Vec<&str> = ids(&g, &a).into_iter().flatten().collect();
            let mut unique = chosen.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(chosen.len(), unique.len(), "cost {cost}: {chosen:?}");
            assert_eq!(a.choices.len(), 3);
        }
    }

    #[test]
    fn the_policy_decides_which_root_an_agent_takes() {
        let fifo = build(vec![spec("a", &[], &[]), spec("b", &[], &[])]);
        let mut richer = spec("b", &[], &[]);
        richer.value = 10.0;
        let wsjf = build(vec![spec("a", &[], &[]), richer]);
        assert_eq!(
            ids(&fifo, &assign(&fifo, 1, OrderPolicy::Fifo, None)),
            vec![Some("a")]
        );
        assert_eq!(
            ids(&wsjf, &assign(&wsjf, 1, OrderPolicy::Wsjf, None)),
            vec![Some("b")]
        );
    }

    #[test]
    fn a_huge_agent_count_is_clamped_to_max_agents() {
        let g = build(vec![spec("a", &[], &[])]);
        let a = assign(&g, 1000, OrderPolicy::Fifo, None);
        assert_eq!(a.choices.len(), MAX_AGENTS);
        assert_eq!(a.costs.len(), MAX_AGENTS);
        // One task: the first agent takes it and every other agent idles, as in a_duplicate_never_beats_idling.
        assert_eq!(a.choices.iter().flatten().count(), 1);
        assert_eq!(a.choices.first(), Some(&Some(0)));
    }

    #[test]
    fn a_round_cap_of_zero_runs_no_pass_and_keeps_the_choices() {
        let g = example_c();
        let game = Game::new(&g, OrderPolicy::Fifo, None);
        let idle = game.none;
        let mut choices = vec![idle, idle, idle];
        assert_eq!(settle(&game, &mut choices, 0), 0);
        assert_eq!(choices, vec![idle, idle, idle]);
    }

    #[test]
    fn no_tasks_means_every_agent_idles_at_zero() {
        let g = build(Vec::new());
        let a = assign(&g, 2, OrderPolicy::Fifo, None);
        assert_eq!(a.choices, vec![None, None]);
        assert_eq!(a.costs, vec![PyNum::Float(0.0), PyNum::Float(0.0)]);
        assert_eq!(a.potential, PyNum::Float(0.0));
        assert_eq!(a.rounds, 1);
    }

    /// Deterministic linear congruential generator, so the random graphs are the same on every run.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    fn random_graph(rng: &mut Lcg) -> Graph {
        const POOL: [&str; 5] = ["src", "src/x", "docs", "src/y", "docs/z"];
        let count = 1 + rng.below(6) as usize;
        let mut tasks = Vec::with_capacity(count);
        for i in 0..count {
            let files: Vec<&str> = (0..rng.below(3))
                .map(|_| POOL[rng.below(5) as usize])
                .collect();
            // Prerequisites only point backwards, so the graph is acyclic.
            let after: Vec<String> = (0..i)
                .filter(|_| rng.below(3) == 0)
                .map(|j| format!("t{j}"))
                .collect();
            tasks.push(Task {
                index: i,
                id: format!("t{i}"),
                title: String::new(),
                kind: "other".to_string(),
                tier: 1,
                queued: None,
                after,
                size: 1.0,
                value: 1.0,
                files: files.iter().map(|s| s.to_string()).collect(),
            });
        }
        Graph::new(tasks).expect("random graph is valid")
    }

    /// No agent can lower its cost by switching to any option. The reference cost is computed here, not with the
    /// module's own helpers, and all values are small multiples of 0.5, so the sums are exact.
    fn assert_nash(g: &Graph, policy: OrderPolicy, conflict_cost: Option<f64>, a: &Assignment) {
        let ranked: Vec<usize> = order(g, policy)
            .order
            .into_iter()
            .filter(|&t| g.tasks[t].after.is_empty())
            .collect();
        let idle = ranked.len() as f64;
        let duplicate = idle + 1.0;
        let penalty = conflict_cost.unwrap_or(duplicate);
        let clash = |x: usize, y: usize| {
            if x == y {
                duplicate
            } else if conflict(&g.tasks[x], &g.tasks[y]) {
                penalty
            } else {
                0.0
            }
        };
        let cost = |choice: Option<usize>, others: &[Option<usize>]| -> f64 {
            match choice {
                None => idle,
                Some(t) => {
                    let rank = ranked
                        .iter()
                        .position(|&r| r == t)
                        .expect("a choice is a ranked root");
                    rank as f64 + others.iter().flatten().map(|&o| clash(t, o)).sum::<f64>()
                }
            }
        };
        for (agent, &choice) in a.choices.iter().enumerate() {
            let others: Vec<Option<usize>> = a
                .choices
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != agent)
                .map(|(_, c)| *c)
                .collect();
            let own = cost(choice, &others);
            let options = std::iter::once(None).chain(ranked.iter().map(|&t| Some(t)));
            for option in options {
                let alternative = cost(option, &others);
                assert!(
                    alternative >= own,
                    "agent {agent} lowers its cost from {own} to {alternative} by switching"
                );
            }
        }
    }

    #[test]
    fn every_assignment_on_random_graphs_is_a_nash_equilibrium() {
        let mut rng = Lcg(5);
        let policies = [
            OrderPolicy::Fifo,
            OrderPolicy::Wsjf,
            OrderPolicy::CriticalPath,
        ];
        for trial in 0..100_usize {
            let g = random_graph(&mut rng);
            let agents = 1 + trial % 6;
            let policy = policies[trial % 3];
            let conflict_cost = if trial % 2 == 0 { None } else { Some(0.5) };
            let a = assign(&g, agents, policy, conflict_cost);
            assert_eq!(a.choices.len(), agents, "trial {trial}");
            assert!(a.rounds >= 1, "trial {trial}");
            assert_nash(&g, policy, conflict_cost, &a);
        }
    }
}
