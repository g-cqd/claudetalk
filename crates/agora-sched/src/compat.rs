//! Byte-compatible emulation of `work order|plan` in given-tasks mode (differential fixtures).

use crate::graph::Graph;
use crate::order::OrderPolicy;
use crate::plan::MAX_AGENTS;
use crate::pyfmt::{printable, py_float, py_int, repr_str};
use crate::render::{Mode, render};
use crate::task::load_tasks;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// True when args (argv after "work", i.e. starting with "order"|"plan") contain --tasks or --task (also --tasks=X,
/// unique-prefix forms like --tas). Used by the CLI to route to `run`.
pub fn is_given_tasks(args: &[String]) -> bool {
    // Every prefix of "--tasks" of at least four characters ("--ta" .. "--tasks") names --task or --tasks, or is
    // ambiguous between them; both route to `run`, which reports the ambiguity itself.
    args.iter().any(|arg| {
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        name.len() >= 4 && "--tasks".starts_with(name)
    })
}

/// Emulates `work order|plan` in given-tasks mode byte-for-byte for exit code and stdout, and for stderr except
/// argparse/JSON-decode/OS texts (fixtures mark those `compare_stderr: false`). `read(path)` returns the text of a
/// --tasks FILE ("-" = stdin) or Err(message). Errors: "work: <msg>\n" on stderr, exit 2; success: rendered + "\n".
pub fn run(args: &[String], read: &mut dyn FnMut(&str) -> Result<String, String>) -> CompatOutput {
    let Some((command, rest)) = args.split_first() else {
        return command_error("the following arguments are required: command");
    };
    let sub = match command.as_str() {
        "order" => Sub::Order,
        "plan" => Sub::Plan,
        other => {
            let message = format!(
                "argument command: invalid choice: {} (choose from 'order', 'plan')",
                repr_str(other)
            );
            return command_error(&message);
        }
    };
    // argparse sees only the words before the first "--"; a command after it is refused once parsing succeeds.
    let head = rest
        .split(|arg| arg.as_str() == "--")
        .next()
        .unwrap_or_default();
    let dash_dash = head.len() != rest.len();
    let parsed = match parse(sub, head) {
        Ok(parsed) => parsed,
        Err(Stop::Help) => {
            return CompatOutput {
                exit_code: 0,
                stdout: format!("{}\n", usage(sub)),
                stderr: String::new(),
            };
        }
        Err(Stop::Error(message)) => return usage_error(sub, &message),
    };
    if dash_dash {
        return fail("only add and run take a command after --");
    }
    if parsed.scope.is_some() {
        return fail("--scope does not apply to tasks given with --tasks or --task");
    }
    if parsed.conflict_cost.is_some() && !parsed.assign {
        return fail("--conflict-cost needs --assign");
    }
    if parsed
        .tasks
        .iter()
        .filter(|path| path.as_str() == "-")
        .count()
        > 1
    {
        return fail("--tasks - reads the standard input once");
    }
    match render_given(sub, &parsed, read) {
        Ok(text) => CompatOutput {
            exit_code: 0,
            stdout: format!("{text}\n"),
            stderr: String::new(),
        },
        Err(message) => fail(&message),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Order,
    Plan,
}

impl Sub {
    fn name(self) -> &'static str {
        match self {
            Sub::Order => "order",
            Sub::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Help,
    Scope,
    Json,
    Policy,
    Tasks,
    Task,
    Agents,
    Assign,
    ConflictCost,
}

impl Opt {
    fn takes_value(self) -> bool {
        matches!(
            self,
            Opt::Scope | Opt::Policy | Opt::Tasks | Opt::Task | Opt::Agents | Opt::ConflictCost
        )
    }
}

const SHARED_OPTIONS: [(&str, Opt); 7] = [
    ("-h", Opt::Help),
    ("--help", Opt::Help),
    ("--scope", Opt::Scope),
    ("--json", Opt::Json),
    ("--policy", Opt::Policy),
    ("--tasks", Opt::Tasks),
    ("--task", Opt::Task),
];

const PLAN_OPTIONS: [(&str, Opt); 3] = [
    ("--agents", Opt::Agents),
    ("--assign", Opt::Assign),
    ("--conflict-cost", Opt::ConflictCost),
];

fn option_table(sub: Sub) -> Vec<(&'static str, Opt)> {
    let mut table = SHARED_OPTIONS.to_vec();
    if sub == Sub::Plan {
        table.extend(PLAN_OPTIONS);
    }
    table
}

/// How argparse classifies one argv word.
enum Token<'a> {
    /// A positional word, or an option value (including "-" and negative numbers).
    Value,
    /// An option: the spelling it matched, and an inline `=value` if one was given.
    Option {
        name: &'static str,
        opt: Opt,
        explicit: Option<&'a str>,
    },
    /// Starts with a dash and matches no option.
    Unknown,
    /// A long-option abbreviation that matches several options; the message is argparse's.
    Ambiguous(String),
}

fn classify<'a>(word: &'a str, table: &[(&'static str, Opt)]) -> Token<'a> {
    if !word.starts_with('-') || word == "-" {
        return Token::Value;
    }
    if let Some(&(name, opt)) = table.iter().find(|(name, _)| *name == word) {
        return Token::Option {
            name,
            opt,
            explicit: None,
        };
    }
    let (spelled, explicit) = match word.split_once('=') {
        Some((spelled, value)) => (spelled, Some(value)),
        None => (word, None),
    };
    if let Some(&(name, opt)) = table.iter().find(|(name, _)| *name == spelled) {
        return Token::Option {
            name,
            opt,
            explicit,
        };
    }
    if spelled.starts_with("--") {
        let hits: Vec<(&'static str, Opt)> = table
            .iter()
            .copied()
            .filter(|(name, _)| name.starts_with(spelled))
            .collect();
        match hits.as_slice() {
            [] => {}
            &[(name, opt)] => {
                return Token::Option {
                    name,
                    opt,
                    explicit,
                };
            }
            many => {
                let names: Vec<&str> = many.iter().map(|(name, _)| *name).collect();
                let message = format!(
                    "ambiguous option: {spelled} could match {}",
                    names.join(", ")
                );
                return Token::Ambiguous(message);
            }
        }
    }
    if is_negative_number(word) || word.contains(' ') {
        Token::Value
    } else {
        Token::Unknown
    }
}

/// argparse's `^-\d+$|^-\d*\.\d+$`, which reads as a value because no option looks like a number.
fn is_negative_number(word: &str) -> bool {
    let Some(body) = word.strip_prefix('-') else {
        return false;
    };
    let digits = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
    digits(body)
        || body
            .split_once('.')
            .is_some_and(|(whole, part)| (whole.is_empty() || digits(whole)) && digits(part))
}

#[derive(Debug, Default)]
struct Parsed {
    scope: Option<String>,
    json: bool,
    policy: OrderPolicy,
    tasks: Vec<String>,
    specs: Vec<String>,
    agents: Option<usize>,
    assign: bool,
    conflict_cost: Option<f64>,
}

/// Why parsing stopped: `-h` prints the usage and exits 0; anything else is an argparse error (exit 2).
enum Stop {
    Help,
    Error(String),
}

fn argument_error(name: &str, detail: &str) -> Stop {
    Stop::Error(format!("argument {name}: {detail}"))
}

/// argparse's left-to-right parse of the words before "--" (type errors, then required, then unrecognised).
fn parse(sub: Sub, words: &[String]) -> Result<Parsed, Stop> {
    let table = option_table(sub);
    let mut parsed = Parsed::default();
    let mut extras: Vec<&str> = Vec::new();
    let mut i = 0;
    while let Some(word) = words.get(i) {
        i += 1;
        match classify(word, &table) {
            Token::Value | Token::Unknown => extras.push(word.as_str()),
            Token::Ambiguous(message) => return Err(Stop::Error(message)),
            Token::Option {
                name,
                opt,
                explicit,
            } => {
                let value = match (opt.takes_value(), explicit) {
                    (false, Some(text)) => {
                        let detail = format!("ignored explicit argument {}", repr_str(text));
                        return Err(argument_error(name, &detail));
                    }
                    (false, None) => "",
                    (true, Some(text)) => text,
                    (true, None) => match words.get(i) {
                        Some(next) if matches!(classify(next, &table), Token::Value) => {
                            i += 1;
                            next.as_str()
                        }
                        _ => return Err(argument_error(name, "expected one argument")),
                    },
                };
                apply(&mut parsed, name, opt, value)?;
            }
        }
    }
    if sub == Sub::Plan && parsed.agents.is_none() {
        return Err(Stop::Error(
            "the following arguments are required: --agents".to_owned(),
        ));
    }
    if !extras.is_empty() {
        return Err(Stop::Error(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    Ok(parsed)
}

fn apply(parsed: &mut Parsed, name: &str, opt: Opt, value: &str) -> Result<(), Stop> {
    match opt {
        Opt::Help => return Err(Stop::Help),
        Opt::Json => parsed.json = true,
        Opt::Assign => parsed.assign = true,
        Opt::Scope => parsed.scope = Some(value.to_owned()),
        Opt::Tasks => parsed.tasks.push(value.to_owned()),
        Opt::Task => parsed.specs.push(value.to_owned()),
        Opt::Policy => {
            parsed.policy = OrderPolicy::parse(value).ok_or_else(|| {
                let detail = format!(
                    "invalid choice: {} (choose from 'fifo', 'wsjf', 'critical-path')",
                    repr_str(value)
                );
                argument_error(name, &detail)
            })?;
        }
        Opt::Agents => {
            parsed.agents =
                Some(agents_value(value).map_err(|message| argument_error(name, &message))?)
        }
        Opt::ConflictCost => {
            parsed.conflict_cost =
                Some(cost_value(value).map_err(|message| argument_error(name, &message))?);
        }
    }
    Ok(())
}

/// argparse `counting(1)`: Python `int(text)` that is at least one, and at most `MAX_AGENTS`. The cap is the CLI's own
/// (work.py has none), so a huge count cannot make the scheduler allocate a lane per agent.
fn agents_value(text: &str) -> Result<usize, String> {
    let value = py_int(text).ok_or_else(|| format!("{} is not a whole number", repr_str(text)))?;
    if value < 1 {
        return Err(format!("{} is less than 1", repr_str(text)));
    }
    match usize::try_from(value) {
        Ok(agents) if agents <= MAX_AGENTS => Ok(agents),
        _ => Err(format!("{} is more than {MAX_AGENTS}", repr_str(text))),
    }
}

/// argparse `duration`: Python `float(text)` that is finite and not negative.
fn cost_value(text: &str) -> Result<f64, String> {
    let value = py_float(text).ok_or_else(|| format!("{} is not a number", repr_str(text)))?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(format!(
            "{} is not a finite number, zero or more",
            repr_str(text)
        ))
    }
}

/// The most tasks one given-tasks plan or order accepts; more is refused before the graph is built.
const MAX_TASKS: usize = 2000;

/// `cmd_given_tasks` after the flag checks: read and parse every document, load, order, render.
fn render_given(
    sub: Sub,
    parsed: &Parsed,
    read: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut documents = Vec::with_capacity(parsed.tasks.len());
    for path in &parsed.tasks {
        let text = read(path)?;
        let document = serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|error| json_message(&error, &text))?;
        documents.push(document);
    }
    let tasks = load_tasks(&documents, &parsed.specs).map_err(|error| error.0)?;
    if tasks.is_empty() {
        return Err("no tasks were given".to_owned());
    }
    if tasks.len() > MAX_TASKS {
        return Err(format!("more than {MAX_TASKS} tasks"));
    }
    let graph = Graph::new(tasks).map_err(|error| error.0)?;
    let mode = match sub {
        Sub::Order => Mode::Order,
        Sub::Plan => Mode::Plan {
            agents: parsed.agents.unwrap_or(1),
            assign: parsed.assign,
            conflict_cost: parsed.conflict_cost,
        },
    };
    Ok(render(&graph, parsed.policy, &mode, parsed.json))
}

/// Best-effort text for a JSON syntax error (Python's JSONDecodeError wording and position; not compared).
fn json_message(error: &serde_json::Error, text: &str) -> String {
    let line = error.line();
    let column = error.column();
    let before: usize = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(|l| l.chars().count())
        .sum();
    format!(
        "Expecting value: line {line} column {column} (char {})",
        before + column.saturating_sub(1)
    )
}

fn usage(sub: Sub) -> &'static str {
    match sub {
        Sub::Order => {
            "usage: work order [-h] [--scope SCOPE] [--json] [--policy {fifo,wsjf,critical-path}] [--tasks FILE] [--task SPEC]"
        }
        Sub::Plan => {
            "usage: work plan [-h] [--scope SCOPE] [--json] [--policy {fifo,wsjf,critical-path}] [--tasks FILE] [--task SPEC] --agents AGENTS [--assign] [--conflict-cost COST]"
        }
    }
}

/// A `WorkError`: "work: <msg>" on stderr, exit 2, nothing on stdout.
fn fail(message: &str) -> CompatOutput {
    CompatOutput {
        exit_code: 2,
        stdout: String::new(),
        stderr: format!("work: {}\n", printable(message)),
    }
}

fn usage_error(sub: Sub, message: &str) -> CompatOutput {
    let stderr = format!(
        "{}\nwork {}: error: {}\n",
        usage(sub),
        sub.name(),
        printable(message)
    );
    CompatOutput {
        exit_code: 2,
        stdout: String::new(),
        stderr,
    }
}

fn command_error(message: &str) -> CompatOutput {
    let stderr = format!(
        "usage: work {{order,plan}} ...\nwork: error: {}\n",
        printable(message)
    );
    CompatOutput {
        exit_code: 2,
        stdout: String::new(),
        stderr,
    }
}

#[cfg(test)]
mod tests;
