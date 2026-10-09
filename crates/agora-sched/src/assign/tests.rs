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
