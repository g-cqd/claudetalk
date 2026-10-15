//! The instruction text `agora install` puts into AGENTS.md-style files, Cursor rules and VS Code
//! user instructions (content spec: SPEC 4.2, `instructions.rs` paragraph).

use super::markers::{BEGIN, END};

/// Frontmatter of the owned Cursor rule `agora.mdc`.
const CURSOR_FRONTMATTER: &str =
    "---\ndescription: Agora inbox and coordination rules\nalwaysApply: true\n---\n";

/// Frontmatter of the owned VS Code user instructions file `agora.instructions.md`.
const VSCODE_FRONTMATTER: &str = "---\napplyTo: \"**\"\n---\n";

/// The shared snippet (`assets/agora-instructions.md`, at most 60 lines).
pub fn snippet() -> &'static str {
    include_str!("../../assets/agora-instructions.md")
}

/// `---\ndescription: Agora inbox and coordination rules\nalwaysApply: true\n---\n` + BEGIN + snippet + END
/// (the owned Cursor rule `agora.mdc`; ends with `\n`).
pub fn cursor_rule() -> String {
    framed(CURSOR_FRONTMATTER)
}

/// `---\napplyTo: "**"\n---\n` + BEGIN + snippet + END (the owned VS Code user file
/// `agora.instructions.md`; ends with `\n`).
pub fn vscode_user_instructions() -> String {
    framed(VSCODE_FRONTMATTER)
}

/// `frontmatter` + BEGIN line + snippet + END line, ending with `\n`. The snippet's trailing
/// newlines are normalised so exactly one newline separates it from the END line.
fn framed(frontmatter: &str) -> String {
    format!("{frontmatter}{BEGIN}\n{}\n{END}\n", snippet().trim_end())
}

#[cfg(test)]
mod tests {
    use super::super::markers::{BEGIN, END, has_block};
    use super::{cursor_rule, snippet, vscode_user_instructions};

    /// Tools registered in `crates/agora-mcp/src/tools/mod.rs` (checked by grepping `name: "..."`).
    const VERIFIED_TOOLS: &[&str] = &[
        "whoami",
        "inbox",
        "inbox_wait",
        "read",
        "ack",
        "chat",
        "react",
        "board_summary",
        "card_list",
        "card_move",
        "card_evidence",
        "card_approve",
        "work_next",
        "work_claim",
        "work_show",
        "work_accept",
        "work_run",
        "work_wait",
        "status_set",
        "status_clear",
        "status_list",
    ];

    /// The text between backticks in `text`, every second span of the split.
    fn backticked(text: &str) -> Vec<&str> {
        text.split('`').skip(1).step_by(2).collect()
    }

    fn count_lines(text: &str, line: &str) -> usize {
        text.lines().filter(|l| *l == line).count()
    }

    #[test]
    fn snippet_is_non_empty_and_within_budget() {
        let text = snippet();
        assert!(!text.trim().is_empty());
        assert!(text.lines().count() <= 60, "{} lines", text.lines().count());
        assert_eq!(
            text.lines().next(),
            Some("## Agora (agent messaging and coordination)")
        );
    }

    #[test]
    fn snippet_has_no_marker_lines() {
        for line in snippet().lines() {
            assert!(!line.contains("agora:begin"), "marker in snippet: {line}");
            assert!(!line.contains("agora:end"), "marker in snippet: {line}");
        }
    }

    #[test]
    fn snippet_states_untrusted_rule_and_inbox_wait() {
        let text = snippet();
        assert!(text.contains("untrusted"));
        assert!(text.contains("<untrusted-data>"));
        assert!(text.contains("`inbox_wait`"));
    }

    #[test]
    fn snippet_mentions_only_verified_tools() {
        let text = snippet();
        for span in backticked(text) {
            let tool_like =
                !span.is_empty() && span.chars().all(|c| c.is_ascii_lowercase() || c == '_');
            if tool_like {
                assert!(
                    VERIFIED_TOOLS.contains(&span),
                    "unknown tool `{span}` in snippet"
                );
            }
        }
        for required in [
            "inbox",
            "inbox_wait",
            "ack",
            "chat",
            "work_run",
            "work_wait",
        ] {
            assert!(
                text.contains(&format!("`{required}`")),
                "missing `{required}`"
            );
        }
    }

    #[test]
    fn cursor_rule_has_frontmatter_and_one_block() {
        let rule = cursor_rule();
        assert!(rule.starts_with("---\ndescription:"));
        assert!(rule.contains("alwaysApply: true"));
        assert!(rule.ends_with('\n'));
        assert!(has_block(&rule));
        assert_eq!(count_lines(&rule, BEGIN), 1);
        assert_eq!(count_lines(&rule, END), 1);
        assert!(rule.contains(&format!("{}\n{END}\n", snippet().trim_end())));
    }

    #[test]
    fn vscode_user_instructions_has_apply_to_and_one_block() {
        let file = vscode_user_instructions();
        assert!(file.starts_with("---\napplyTo: \"**\"\n---\n"));
        assert!(file.ends_with('\n'));
        assert!(has_block(&file));
        assert_eq!(count_lines(&file, BEGIN), 1);
        assert_eq!(count_lines(&file, END), 1);
        assert!(file.contains(&format!("{}\n{END}\n", snippet().trim_end())));
    }
}
