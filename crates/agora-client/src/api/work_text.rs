//! Pure text renderers for the work verbs, in work.py style. Every untrusted string (titles, commands, notes,
//! files) goes through `agora_sched::pyfmt::printable`. Renderers return lines joined with "\n" and no trailing
//! newline; `Vec<String>` results are one entry per printed line.

mod status;
#[cfg(test)]
mod tests;
mod wait;

pub use status::status_text;
pub use wait::wait_condition_text;

use agora_sched::cron::civil_from_ms;
use agora_sched::model::{NextOutcome, ScopeRecord, TaskRecord, TaskState, WorkEvent};
use agora_sched::pyfmt::{fmt_fixed, fmt_g, printable};
use agora_sched::textfmt::{capped, display, memory, span};

/// work.py `capped(..., 10)` limit for the after and files lists.
const LIST_LIMIT: usize = 10;

/// One line for a task in lists, in the style of work.py `order`: `ID[ title] (kind; state[; command by AUTHOR])`.
pub fn task_line(t: &TaskRecord) -> String {
    let command = if t.command.is_some() {
        format!("; command by {}", printable(&t.author_name))
    } else {
        String::new()
    };
    format!(
        "{}{} ({}; {}{command})",
        printable(&t.id),
        title_suffix(&t.title, " "),
        printable(&t.kind),
        printable(&t.describe_state()),
    )
}

/// `work show` text (ref-queue-cli section 3): plain text with `printable()`; the MCP layer frames it. Includes
/// `digest: <hex>` and the acceptance line using `agora work accept`.
pub fn show_text(t: &TaskRecord, now_ms: i64) -> String {
    let id = printable(&t.id);
    let mut lines = Vec::new();
    // The command comes first, with its size, so that a long title or file list does not push it out of view.
    if let Some(argv) = &t.command {
        lines.push(format!(
            "command of task {id} ({}), queued by {}:",
            command_size(argv),
            printable(&t.author_name)
        ));
        lines.push(format!("  {}", display(argv)));
        if let Some(cwd) = &t.cwd {
            lines.push(format!("directory: {}", printable(cwd)));
        }
        if let Some(digest) = &t.digest {
            lines.push(format!("digest: {digest}"));
        }
        lines.push(acceptance_line(t, &id));
    }
    lines.push(format!("task {id}{}", title_suffix(&t.title, ": ")));
    lines.push(format!("state: {}", printable(&t.describe_state())));
    lines.push(format!(
        "kind: {}; size {}, value {}, weight {}",
        printable(&t.kind),
        fmt_g(t.size),
        fmt_g(t.value),
        fmt_g(t.weight)
    ));
    lines.push(format!(
        "scope: {}; author: {}; queued {}",
        scope_label(t.scope.as_deref()),
        printable(&t.author_name),
        clock(t.queued_ms)
    ));
    if !t.after.is_empty() {
        lines.push(capped("after", &printable_all(&t.after), LIST_LIMIT));
    }
    if !t.files.is_empty() {
        lines.push(capped("files", &printable_all(&t.files), LIST_LIMIT));
    }
    if let Some(log) = t.log.as_deref().filter(|log| !log.is_empty()) {
        lines.push(format!("log: {}", printable(log)));
    }
    if t.requested_ms.is_some() {
        lines.push(run_line(t, now_ms));
        lines.push(format!("predicted: {}", describe_prediction(t)));
    }
    if !t.note.is_empty() {
        lines.push(format!("note: {}", printable(&t.note)));
    }
    lines.join("\n")
}

/// The result of `next`: `work: claimed task ID[: title]` with the next-step lines, or
/// `work: nothing to claim: REASON`.
pub fn next_text(o: &NextOutcome) -> String {
    let lines = match &o.claimed {
        Some(t) => {
            let mut lines = vec![format!(
                "work: claimed task {}{}",
                printable(&t.id),
                title_suffix(&t.title, ": ")
            )];
            lines.push(format!("work: {}", next_step(t)));
            lines
        }
        None => vec![format!("work: nothing to claim: {}", printable(&o.reason))],
    };
    lines.join("\n")
}

/// The scope tree, two spaces per depth, children in input order. Roots are the scopes whose parent is absent or
/// not in `s`.
pub fn scopes_text(s: &[ScopeRecord]) -> String {
    let lines = scope_tree_lines(s);
    if lines.is_empty() {
        return "no scope yet: `agora work scope set ID --kind project`".to_owned();
    }
    lines.join("\n")
}

/// `log` text: `<UTC clock> <what> [<[scope]>] [<task>] [by <actor>][: <detail>]`, oldest first as given.
pub fn log_text(e: &[WorkEvent]) -> String {
    e.iter()
        .map(|event| {
            let subject: Vec<String> = [
                event
                    .scope
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("[{}]", printable(s))),
                event
                    .task
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(printable),
                (!event.actor_name.is_empty())
                    .then(|| format!("by {}", printable(&event.actor_name))),
            ]
            .into_iter()
            .flatten()
            .collect();
            let detail = if event.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", printable(&event.detail))
            };
            format!(
                "{} {} {}{detail}",
                clock(event.ts_ms),
                printable(&event.kind),
                subject.join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `run`/`wait` final report (ref-queue-cli section 4): for each task, its tail lines (unprefixed) and one status
/// line prefixed `work: `; on timeout, the "call `agora work wait ...` to keep waiting" line for the open tasks.
pub fn report_text(
    tasks: &[TaskRecord],
    tails: &[Vec<String>],
    timed_out: bool,
    now_ms: i64,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        if let Some(tail) = tails.get(index) {
            lines.extend(tail.iter().map(|line| printable(line)));
        }
        let text = if task.state.is_final() {
            end_line(task, now_ms)
        } else {
            pending_line(task, now_ms)
        };
        lines.push(format!("work: {text}"));
    }
    if timed_out {
        let open: Vec<String> = tasks
            .iter()
            .filter(|t| !t.state.is_final())
            .map(|t| printable(&t.id))
            .collect();
        lines.push(format!(
            "work: call `agora work wait {}` to keep waiting",
            open.join(" ")
        ));
    }
    lines
}

/// The `work show` line for a claimed task's acceptance state.
fn acceptance_line(t: &TaskRecord, id: &str) -> String {
    match (&t.agent, &t.accepted_by) {
        (None, _) => "acceptance: the agent that claims it accepts it after reading it".to_owned(),
        (Some(agent), Some(accepted)) if accepted == agent => {
            format!("acceptance: accepted by {}", claimant(t))
        }
        _ => format!(
            "acceptance: needed from {}; if the command is right, run `agora work accept {id} --digest {}`",
            claimant(t),
            t.digest.as_deref().unwrap_or_default()
        ),
    }
}

/// work.py `next_steps`: what to do after a claim.
fn next_step(t: &TaskRecord) -> String {
    let id = printable(&t.id);
    if t.command.is_none() {
        return format!(
            "close it with `agora work done {id}` or `agora work fail {id}` when it is finished"
        );
    }
    if t.needs_acceptance {
        return format!(
            "{id} runs a command that {} queued; read it with `agora work show {id}`, and if it is right, accept \
             it with the `agora work accept {id} --digest ...` line that show prints, then `agora work run {id}`",
            printable(&t.author_name)
        );
    }
    format!("run it with `agora work run {id}`")
}

/// The scope tree as lines, walked with an explicit stack. Each node is visited once, since a scope has one parent.
fn scope_tree_lines(s: &[ScopeRecord]) -> Vec<String> {
    let ids: std::collections::HashSet<&str> = s.iter().map(|r| r.id.as_str()).collect();
    let mut children: std::collections::HashMap<Option<&str>, Vec<&ScopeRecord>> =
        std::collections::HashMap::new();
    for record in s {
        let parent = record.parent.as_deref().filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(record);
    }
    let mut lines = Vec::new();
    let mut stack: Vec<(&ScopeRecord, usize)> = children
        .get(&None::<&str>)
        .map(|roots| roots.iter().rev().map(|r| (*r, 0)).collect())
        .unwrap_or_default();
    while let Some((node, depth)) = stack.pop() {
        lines.push(format!("{}{}", "  ".repeat(depth), describe_scope(node)));
        if let Some(kids) = children.get(&Some(node.id.as_str())) {
            stack.extend(kids.iter().rev().map(|kid| (*kid, depth + 1)));
        }
    }
    lines
}

/// `id (kind)[: task][ [N state, …]]`, the counts in state-name order.
fn describe_scope(node: &ScopeRecord) -> String {
    let mut text = format!("{} ({})", printable(&node.id), node.kind.as_str());
    if !node.task.is_empty() {
        text.push_str(&format!(": {}", printable(&node.task)));
    }
    if !node.counts.is_empty() {
        let tally: Vec<String> = node
            .counts
            .iter()
            .map(|(state, n)| format!("{n} {}", printable(state)))
            .collect();
        text.push_str(&format!(" [{}]", tally.join(", ")));
    }
    text
}

/// The last line of a final task: the status line when a status was recorded, else done or failed or cancelled.
fn end_line(t: &TaskRecord, now_ms: i64) -> String {
    let id = printable(&t.id);
    if let Some(status) = t.status {
        let waited = t.waited_s(now_ms).unwrap_or(0.0);
        let ran = t.ran_s(now_ms).unwrap_or(0.0);
        return format!(
            "job {id} finished with status {status} after {} (waited {} for capacity, ran {})",
            span(Some(waited + ran)),
            span(Some(waited)),
            span(Some(ran))
        );
    }
    let note = note_suffix(&t.note);
    if t.state == TaskState::Done {
        return format!("task {id} is done{note}");
    }
    let word = if t.state == TaskState::Cancelled {
        "was cancelled"
    } else {
        "failed"
    };
    format!("task {id} {word}{note}")
}

/// The line of a task that has not ended yet.
fn pending_line(t: &TaskRecord, now_ms: i64) -> String {
    let id = printable(&t.id);
    if t.state == TaskState::Running {
        return format!(
            "job {id} still running after {} (waited {} for capacity)",
            span(t.ran_s(now_ms)),
            span(Some(t.waited_s(now_ms).unwrap_or(0.0)))
        );
    }
    if t.state == TaskState::Claimed && t.requested_ms.is_some() {
        if t.note.is_empty() {
            return format!("job {id} is starting");
        }
        return format!(
            "job {id} still waiting for capacity after {}: {}",
            span(t.waited_s(now_ms)),
            printable(&t.note)
        );
    }
    format!("task {id} is still {}", printable(&t.describe_state()))
}

/// work.py `describe_prediction` for `show`.
fn describe_prediction(t: &TaskRecord) -> String {
    match &t.prediction {
        None => "no prediction".to_owned(),
        Some(p) => format!(
            "{} cores, {} at peak, {}, p90 {}, from {}",
            fmt_fixed(p.cores, 1),
            memory(p.rss_kib),
            span(Some(p.duration_s)),
            span(p.duration_p90_s),
            p.basis()
        ),
    }
}

/// The `run:` line of `show`: the capacity wait, the run time, and the sampled usage when there is any.
fn run_line(t: &TaskRecord, now_ms: i64) -> String {
    let mut line = format!(
        "run: waited {} for capacity, ran {}",
        span(t.waited_s(now_ms)),
        span(t.ran_s(now_ms))
    );
    if let Some(status) = t.status {
        line.push_str(&format!(", status {status}"));
    }
    if let Some(pid) = t.pid.filter(|pid| *pid != 0) {
        line.push_str(&format!(", process {pid}"));
    }
    if let Some(rss) = t.rss_kib {
        let cpu = t
            .cpu_percent
            .map(|c| format!("cpu {}%, ", fmt_fixed(c, 0)))
            .unwrap_or_default();
        line.push_str(&format!(
            ", {cpu}memory {} (peak {})",
            memory(Some(rss)),
            memory(t.rss_peak_kib)
        ));
    }
    if let Some(cpu_seconds) = t.cpu_seconds {
        line.push_str(&format!(", cpu time {}", span(Some(cpu_seconds))));
    }
    line
}

/// work.py `clock`, in UTC: `%Y-%m-%d %H:%M:%S`.
fn clock(ms: i64) -> String {
    let (year, month, day, hour, minute, _) = civil_from_ms(ms);
    let second = ms.div_euclid(1000).rem_euclid(60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// `command N arguments, B bytes`: B is the UTF-8 length of the arguments plus one space between each pair.
fn command_size(argv: &[String]) -> String {
    let bytes: usize = argv.iter().map(String::len).sum::<usize>() + argv.len().saturating_sub(1);
    let plural = if argv.len() == 1 { "" } else { "s" };
    format!("{} argument{plural}, {bytes} bytes", argv.len())
}

/// The claimant's pseudonym, else its key, else "nobody".
fn claimant(t: &TaskRecord) -> String {
    t.agent_name
        .as_deref()
        .or(t.agent.as_deref())
        .map_or_else(|| "nobody".to_owned(), printable)
}

/// The scope, or "-" when none is set.
fn scope_label(scope: Option<&str>) -> String {
    scope
        .filter(|s| !s.is_empty())
        .map_or_else(|| "-".to_owned(), printable)
}

/// ` ` or `: ` then the printable title, or nothing for an empty title.
fn title_suffix(title: &str, separator: &str) -> String {
    if title.is_empty() {
        String::new()
    } else {
        format!("{separator}{}", printable(title))
    }
}

/// `: note` for a non-empty note, nothing otherwise.
fn note_suffix(note: &str) -> String {
    if note.is_empty() {
        String::new()
    } else {
        format!(": {}", printable(note))
    }
}

/// Every item through `printable`.
fn printable_all(items: &[String]) -> Vec<String> {
    items.iter().map(|item| printable(item)).collect()
}
