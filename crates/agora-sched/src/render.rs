//! Text and JSON rendering of `order` / `plan` results, byte-compatible with work.py.

use crate::assign::{Assignment, PyNum, assign};
use crate::graph::Graph;
use crate::order::{OrderPolicy, Ordered, order};
use crate::plan::{Schedule, schedule};
use crate::pyfmt::{PyJson, float_repr, fmt_g, json_dumps_indent2, printable};
use crate::task::Queued;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Order,
    Plan {
        agents: usize,
        assign: bool,
        conflict_cost: Option<f64>,
    },
}

/// work.py render(): JSON (json_dumps_indent2) or text (each line through printable), WITHOUT trailing newline.
/// Plan with agents == 1 and !assign renders the Order shape (ref-ordering section 5.1).
pub fn render(g: &Graph, policy: OrderPolicy, mode: &Mode, json: bool) -> String {
    match mode {
        Mode::Plan {
            agents,
            assign: true,
            conflict_cost,
        } => {
            let result = assign(g, *agents, policy, *conflict_cost);
            if json {
                assign_json(g, *agents, &result)
            } else {
                join_text(&assign_lines(g, &result))
            }
        }
        Mode::Plan {
            agents,
            assign: false,
            ..
        } if *agents > 1 => {
            let result = schedule(g, *agents, policy);
            if json {
                schedule_json(g, *agents, &result)
            } else {
                join_text(&schedule_lines(g, &result))
            }
        }
        Mode::Order | Mode::Plan { .. } => {
            let ordered = order(g, policy);
            if json {
                order_json(g, policy, &ordered)
            } else {
                join_text(&order_lines(g, &ordered))
            }
        }
    }
}

fn join_text(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| printable(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn count(n: usize) -> PyJson {
    PyJson::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn task_id(g: &Graph, index: usize) -> Option<&str> {
    g.tasks.get(index).map(|task| task.id.as_str())
}

/// The id of the descendant that moved task `index` up, if any (`source` in work.py).
fn moved_up_for<'a>(g: &'a Graph, ordered: &Ordered, index: usize) -> Option<&'a str> {
    let source = ordered.source.get(index).copied().flatten()?;
    task_id(g, source)
}

/// Python str() of a queued value: int decimal, float repr, text as is.
fn queued_text(queued: &Queued) -> String {
    match queued {
        Queued::Int(value) => value.to_string(),
        Queued::Float(value) => float_repr(*value),
        Queued::Text(text) => text.clone(),
    }
}

fn order_json(g: &Graph, policy: OrderPolicy, ordered: &Ordered) -> String {
    let rows = ordered
        .order
        .iter()
        .filter_map(|&index| {
            let task = g.tasks.get(index)?;
            let moved = moved_up_for(g, ordered, index)
                .map_or(PyJson::Null, |id| PyJson::Str(id.to_owned()));
            Some(PyJson::Object(vec![
                ("id".to_owned(), PyJson::Str(task.id.clone())),
                ("tier".to_owned(), PyJson::Int(i64::from(task.tier))),
                ("moved_up_for".to_owned(), moved),
            ]))
        })
        .collect();
    json_dumps_indent2(&PyJson::Object(vec![
        ("policy".to_owned(), PyJson::Str(policy.as_str().to_owned())),
        ("order".to_owned(), PyJson::List(rows)),
    ]))
}

fn order_lines(g: &Graph, ordered: &Ordered) -> Vec<String> {
    ordered
        .order
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| {
            let task = g.tasks.get(index)?;
            let mut parts = vec![task.kind.clone()];
            if let Some(queued) = &task.queued {
                parts.push(format!("queued {}", queued_text(queued)));
            }
            if let Some(source) = moved_up_for(g, ordered, index) {
                parts.push(format!("moved up for {source}"));
            }
            let title = if task.title.is_empty() {
                String::new()
            } else {
                format!(" {}", task.title)
            };
            Some(format!(
                "{}. {}{} ({})",
                position + 1,
                task.id,
                title,
                parts.join(", ")
            ))
        })
        .collect()
}

fn schedule_json(g: &Graph, agents: usize, result: &Schedule) -> String {
    let lanes = result
        .lanes
        .iter()
        .map(|lane| {
            PyJson::List(
                lane.iter()
                    .filter_map(|slot| {
                        let id = task_id(g, slot.task)?;
                        Some(PyJson::Object(vec![
                            ("id".to_owned(), PyJson::Str(id.to_owned())),
                            ("start".to_owned(), PyJson::Float(slot.start)),
                            ("end".to_owned(), PyJson::Float(slot.end)),
                        ]))
                    })
                    .collect(),
            )
        })
        .collect();
    json_dumps_indent2(&PyJson::Object(vec![
        ("agents".to_owned(), count(agents)),
        ("makespan".to_owned(), PyJson::Float(result.makespan)),
        ("lower_bound".to_owned(), PyJson::Float(result.lower_bound)),
        ("lanes".to_owned(), PyJson::List(lanes)),
    ]))
}

fn schedule_lines(g: &Graph, result: &Schedule) -> Vec<String> {
    let mut lines: Vec<String> = result
        .lanes
        .iter()
        .enumerate()
        .map(|(lane_index, lane)| {
            let steps: Vec<String> = lane
                .iter()
                .filter_map(|slot| {
                    let id = task_id(g, slot.task)?;
                    Some(format!("{id} [{}-{}]", fmt_g(slot.start), fmt_g(slot.end)))
                })
                .collect();
            if steps.is_empty() {
                format!("agent {}: idle", lane_index + 1)
            } else {
                format!("agent {}: {}", lane_index + 1, steps.join(", "))
            }
        })
        .collect();
    lines.push(format!(
        "makespan {}; lower bound {}",
        fmt_g(result.makespan),
        fmt_g(result.lower_bound)
    ));
    lines
}

fn pynum_json(value: PyNum) -> PyJson {
    match value {
        PyNum::Int(number) => PyJson::Int(number),
        PyNum::Float(number) => PyJson::Float(number),
    }
}

fn pynum_value(value: PyNum) -> f64 {
    match value {
        PyNum::Int(number) => number as f64,
        PyNum::Float(number) => number,
    }
}

fn assign_json(g: &Graph, agents: usize, result: &Assignment) -> String {
    let rows = result
        .choices
        .iter()
        .zip(result.costs.iter())
        .enumerate()
        .map(|(agent, (&choice, &cost))| {
            let task = choice
                .and_then(|index| task_id(g, index))
                .map_or(PyJson::Null, |id| PyJson::Str(id.to_owned()));
            PyJson::Object(vec![
                ("agent".to_owned(), count(agent)),
                ("task".to_owned(), task),
                ("cost".to_owned(), pynum_json(cost)),
            ])
        })
        .collect();
    json_dumps_indent2(&PyJson::Object(vec![
        ("agents".to_owned(), count(agents)),
        ("assignment".to_owned(), PyJson::List(rows)),
        ("potential".to_owned(), pynum_json(result.potential)),
        ("rounds".to_owned(), PyJson::Int(i64::from(result.rounds))),
        ("equilibrium".to_owned(), PyJson::Bool(true)),
    ]))
}

fn assign_lines(g: &Graph, result: &Assignment) -> Vec<String> {
    let mut lines: Vec<String> = result
        .choices
        .iter()
        .enumerate()
        .map(|(agent, &choice)| {
            let who = choice.and_then(|index| task_id(g, index)).unwrap_or("idle");
            format!("agent {}: {who}", agent + 1)
        })
        .collect();
    lines.push(format!(
        "Nash equilibrium after {} rounds; potential {}",
        result.rounds,
        fmt_g(pynum_value(result.potential))
    ));
    lines
}

#[cfg(test)]
mod tests;
