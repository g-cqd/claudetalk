//! `work wait` condition text: `--pid`, `--file`, `--gone` and `--log`, and the hint that resumes any other condition.

use agora_sched::model::WaitResult;
use agora_sched::pyfmt::printable;
use agora_sched::textfmt::{shlex_quote, span};

/// The tail lines (unprefixed) and the condition's status line (`work: `), as work.py prints a `wait` condition.
/// A single file target with a non-zero status is the number the file held, printed as "finished with status N".
pub fn wait_condition_text(r: &WaitResult) -> Vec<String> {
    let mut lines: Vec<String> = r.tail.iter().map(|line| printable(line)).collect();
    let seconds = span(Some(r.seconds));
    let (still, done) = condition_words(&r.condition);
    let text = if r.timed_out {
        let pending: Vec<String> = r.pending.iter().map(|item| printable(item)).collect();
        format!(
            "{still} after {seconds}: {}; {}",
            pending.join(" "),
            resume_hint(r)
        )
    } else {
        match (r.condition.as_str(), r.targets.as_slice(), r.status) {
            ("file", [target], status) if status != 0 => format!(
                "finished with status {status} after {seconds}, from {}",
                printable(target)
            ),
            _ => format!("{done} after {seconds}"),
        }
    };
    lines.push(format!("work: {text}"));
    lines
}

/// How a timed-out wait is resumed. The pid, file, gone and log conditions have a CLI form that names their targets,
/// so the hint is that command; the other conditions (tasks, all, cards, messages, notices) are resumed by running the
/// same wait again.
fn resume_hint(r: &WaitResult) -> String {
    match r.condition.as_str() {
        "pid" | "file" | "gone" | "log" => {
            let mut again: Vec<String> =
                r.targets.iter().map(|target| shlex_quote(target)).collect();
            if r.condition == "log" {
                // The result holds only the path; the pattern is not echoed, so the hint leaves a placeholder.
                again.push("PATTERN".to_owned());
            }
            format!(
                "call `{}` to keep waiting",
                printable(&format!(
                    "agora work wait --{} {}",
                    r.condition,
                    again.join(" ")
                ))
            )
        }
        _ => "run the same wait again to keep waiting".to_owned(),
    }
}

/// The two words of a condition: the still-pending phrase and the done phrase.
fn condition_words(condition: &str) -> (&'static str, &'static str) {
    match condition {
        "pid" => ("still running", "every process exited"),
        "file" => ("not ready", "every file is ready"),
        "gone" => ("still present", "every path is gone"),
        "log" => ("no match", "the log matched"),
        _ => ("still waiting", "the condition holds"),
    }
}
