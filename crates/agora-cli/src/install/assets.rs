//! The Claude Code plugin files, embedded at build time from `plugin/claude/`.

/// `(path relative to plugin/claude, contents)` for every plugin file, via `include_str!`.
pub const PLUGIN_FILES: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../../../plugin/claude/.claude-plugin/plugin.json"),
    ),
    (
        ".mcp.json",
        include_str!("../../../../plugin/claude/.mcp.json"),
    ),
    (
        "hooks/hooks.json",
        include_str!("../../../../plugin/claude/hooks/hooks.json"),
    ),
    (
        "hooks/register.js",
        include_str!("../../../../plugin/claude/hooks/register.js"),
    ),
    (
        "skills/agora/SKILL.md",
        include_str!("../../../../plugin/claude/skills/agora/SKILL.md"),
    ),
    (
        "commands/inbox.md",
        include_str!("../../../../plugin/claude/commands/inbox.md"),
    ),
    (
        "commands/status.md",
        include_str!("../../../../plugin/claude/commands/status.md"),
    ),
];
