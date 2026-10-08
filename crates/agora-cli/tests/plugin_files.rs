//! Static checks of the Claude Code plugin under `plugin/claude/`.
//!
//! The plugin is data: JSON manifests, the hook wiring, a mod, and markdown skill and command
//! files. These tests read it from disk and pin the contract the rest of the system depends on:
//! the hook commands that the real `agora` binary must accept, the one placeholder line the
//! installer rewrites, the frontmatter Claude Code parses, and that no secret material ships.
//! The installer embeds the same files with `include_str!`; a test pins that list to the disk.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard cap on one `agora` invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Subcommands of `agora hook` that the plugin's hook commands may name.
const HOOK_SUBCOMMANDS: [&str; 4] = ["session-start", "context", "stop", "rewake"];

/// The only events the plugin wires. SubagentStop is deliberately absent: its additionalContext
/// and decision output would make the subagent continue and read the parent's messages, so the
/// plugin never wires it (`agora hook context` treats SubagentStop as a no-op).
const EVENTS: [&str; 5] = [
    "PostToolBatch",
    "PostToolUse",
    "SessionStart",
    "Stop",
    "UserPromptSubmit",
];

/// Prefix of every plugin-provided MCP tool name: `mcp__plugin_<plugin>_<server>__<tool>`.
const PLUGIN_TOOL_PREFIX: &str = "mcp__plugin_agora_agora__";

/// The single line of `register.js` that the installer rewrites into an absolute command.
const AGORA_PLACEHOLDER: &str = r#"const AGORA = ["agora"];"#;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn plugin_dir() -> PathBuf {
    repo_root().join("plugin").join("claude")
}

/// Every file under `dir`, as `/`-separated paths relative to `dir`, sorted.
fn files_under(dir: &Path) -> Vec<String> {
    fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read plugin directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).expect("path under plugin root");
                out.push(rel.to_str().expect("UTF-8 path").to_owned());
            }
        }
    }
    let mut out = Vec::new();
    collect(dir, dir, &mut out);
    out.sort();
    out
}

/// Generated output (the `.claude-plugin/types/` directory and `tsconfig.json`), not shipped source.
fn is_generated(rel: &str) -> bool {
    rel.starts_with(".claude-plugin/types/") || rel == "tsconfig.json"
}

/// The plugin files that ship: generated output and the local `.gitignore` excluded.
fn shipped_files() -> Vec<String> {
    files_under(&plugin_dir())
        .into_iter()
        .filter(|rel| !is_generated(rel) && rel != ".gitignore")
        .collect()
}

fn read(rel: &str) -> String {
    fs::read_to_string(plugin_dir().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn read_json(rel: &str) -> Value {
    serde_json::from_str(&read(rel)).unwrap_or_else(|e| panic!("{rel} is not valid JSON: {e}"))
}

/// Every plugin file (generated or not) with its text, for the secret scans.
fn plugin_texts() -> Vec<(String, String)> {
    files_under(&plugin_dir())
        .into_iter()
        .map(|rel| {
            let text = read(&rel);
            (rel, text)
        })
        .collect()
}

fn is_kebab_case(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `X.Y.Z` with three numeric parts.
fn is_dotted_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.parse::<u32>().is_ok())
}

/// The hook document: `hooks/hooks.json`.
fn hooks_doc() -> Value {
    read_json("hooks/hooks.json")
}

/// One entry per hook handler: (event, group matcher, handler JSON).
fn handlers(doc: &Value) -> Vec<(String, Option<String>, Value)> {
    let mut out = Vec::new();
    for (event, groups) in doc["hooks"].as_object().expect("hooks is an object") {
        for group in groups.as_array().expect("event value is an array") {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_owned);
            for handler in group["hooks"].as_array().expect("group.hooks is an array") {
                out.push((event.clone(), matcher.clone(), handler.clone()));
            }
        }
    }
    out
}

fn command_of(handler: &Value) -> &str {
    handler["command"]
        .as_str()
        .unwrap_or_else(|| panic!("handler has no string command: {handler}"))
}

/// The argv after `agora hook` for a command line `agora hook <sub> [flags]`; None otherwise.
fn hook_args(command: &str) -> Option<Vec<&str>> {
    let rest = command.strip_prefix("agora hook ")?;
    Some(rest.split_whitespace().collect())
}

/// Claude Code matcher semantics as the plugin uses them: `|` separates alternatives, and an
/// alternative ending in `.*` matches any name with that prefix. Anything else must be equal.
fn matcher_matches(matcher: &str, name: &str) -> bool {
    matcher.split('|').any(|alt| match alt.strip_suffix(".*") {
        Some(prefix) => name.starts_with(prefix),
        None => alt == name,
    })
}

/// The `timeout` of the asyncRewake handler wired on `event`.
fn rewake_timeout(all: &[(String, Option<String>, Value)], event: &str) -> f64 {
    all.iter()
        .find(|(e, _, h)| e == event && h["asyncRewake"] == json!(true))
        .unwrap_or_else(|| panic!("no asyncRewake handler on {event}"))
        .2["timeout"]
        .as_f64()
        .expect("rewake timeout is numeric")
}

/// True when `text` holds `agora_` followed by 20 or more ASCII letters or digits.
fn has_token_shape(text: &str) -> bool {
    const PREFIX: &str = "agora_";
    text.match_indices(PREFIX).any(|(at, _)| {
        text[at + PREFIX.len()..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .count()
            >= 20
    })
}

/// True when `text` has a run of at least `min` lowercase hex digits (a seed or key shape).
fn has_hex_run(text: &str, min: usize) -> bool {
    let mut run = 0;
    for c in text.chars() {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run += 1;
            if run >= min {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// True when `text` contains `word` as a whole word, ignoring ASCII case.
fn mentions_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w.eq_ignore_ascii_case(word))
}

/// The lines between the opening `---` fence (first line) and the closing `---`.
fn frontmatter(text: &str) -> Vec<&str> {
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "file must open with a `---` frontmatter fence"
    );
    let mut body = Vec::new();
    for line in lines {
        if line == "---" {
            return body;
        }
        body.push(line);
    }
    panic!("frontmatter is not closed by a `---` line");
}

fn marketplace() -> Value {
    let path = repo_root().join(".claude-plugin").join("marketplace.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("marketplace.json is valid JSON")
}

fn marketplace_plugins(doc: &Value) -> Vec<Value> {
    doc["plugins"]
        .as_array()
        .expect("plugins is an array")
        .clone()
}

fn on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

/// The relative paths named by `include_str!("../../../../plugin/claude/<rel>")` in `source`.
fn include_paths(source: &str) -> Vec<String> {
    const PREFIX: &str = "include_str!(\"../../../../plugin/claude/";
    source
        .split(PREFIX)
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect()
}

/// `source` with JavaScript `//` and `/* */` comments removed, so only code is scanned. String
/// literals are copied through unchanged, so a `//` inside a string is not taken as a comment.
/// Each comment becomes whitespace and newlines are kept, so line structure survives.
fn strip_js_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' | '`' => {
                out.push(c);
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if d == c {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for d in chars.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// The code of `register.js`: its comments removed.
fn register_js_code() -> String {
    strip_js_comments(&read("hooks/register.js"))
}

#[test]
fn every_plugin_json_file_parses() {
    let json_files: Vec<String> = shipped_files()
        .into_iter()
        .filter(|rel| rel.ends_with(".json"))
        .collect();
    assert!(
        !json_files.is_empty(),
        "the plugin has no JSON files to check"
    );
    let broken: Vec<String> = json_files
        .iter()
        .filter(|rel| serde_json::from_str::<Value>(&read(rel)).is_err())
        .cloned()
        .collect();
    assert!(
        broken.is_empty(),
        "these plugin JSON files do not parse: {broken:?}"
    );
}

#[test]
fn plugin_manifest_names_agora_with_version_description_and_author() {
    let manifest = read_json(".claude-plugin/plugin.json");
    let name = manifest["name"].as_str().expect("name is a string");
    assert_eq!(name, "agora");
    assert!(is_kebab_case(name), "plugin name is not kebab-case: {name}");
    let version = manifest["version"].as_str().expect("version is a string");
    assert!(
        is_dotted_version(version),
        "version is not X.Y.Z: {version}"
    );
    assert!(
        manifest["description"]
            .as_str()
            .is_some_and(|d| !d.trim().is_empty()),
        "manifest has no description"
    );
    assert!(
        manifest["author"]["name"]
            .as_str()
            .is_some_and(|n| !n.trim().is_empty()),
        "manifest has no author.name"
    );
}

#[test]
fn mcp_server_launches_agora_mcp_subcommand() {
    let mcp = read_json(".mcp.json");
    let server = &mcp["mcpServers"]["agora"];
    assert_eq!(server["command"], json!("agora"));
    assert_eq!(server["args"], json!(["mcp"]));
}

#[test]
fn mcp_server_env_pins_legacy_negotiation_and_expands_project_and_home() {
    let mcp = read_json(".mcp.json");
    let env = &mcp["mcpServers"]["agora"]["env"];
    // Legacy negotiation keeps the channel path available on Claude Code 2.1.292+.
    assert_eq!(env["MCP_PROTOCOL_NEGOTIATION"], json!("legacy"));
    // Both sides of the identity key derive the folder from the same placeholder.
    assert_eq!(env["AGORA_FOLDER"], json!("${CLAUDE_PROJECT_DIR}"));
    assert_eq!(env["AGORA_HOME"], json!("${AGORA_HOME:-}"));
}

#[test]
fn hooks_json_wraps_handlers_under_hooks_and_names_an_existing_module() {
    let doc = hooks_doc();
    assert!(
        doc["hooks"].is_object(),
        "hooks.json needs a top-level `hooks` object"
    );
    assert_eq!(doc["modules"], json!(["./register.js"]));
    // The module path is relative to hooks/, where hooks.json lives.
    assert!(
        plugin_dir().join("hooks").join("register.js").is_file(),
        "the module named in hooks.json is missing"
    );
}

#[test]
fn hooks_json_wires_exactly_the_five_supported_events() {
    let doc = hooks_doc();
    let hooks = doc["hooks"].as_object().expect("hooks is an object");
    assert!(
        !hooks.contains_key("SubagentStop"),
        "SubagentStop must not be wired; see the EVENTS comment"
    );
    let wired: BTreeSet<&str> = hooks.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = EVENTS.into_iter().collect();
    assert_eq!(wired, expected);
}

#[test]
fn every_hook_handler_is_a_command_with_a_numeric_timeout() {
    let all = handlers(&hooks_doc());
    assert!(!all.is_empty(), "hooks.json wires no handlers");
    for (event, _, handler) in &all {
        assert_eq!(
            handler["type"],
            json!("command"),
            "{event} handler is not a command"
        );
        assert!(
            handler["timeout"].is_number(),
            "{event} handler has no numeric timeout: {handler}"
        );
    }
}

#[test]
fn every_hook_command_is_agora_hook_with_a_known_subcommand() {
    for (event, _, handler) in handlers(&hooks_doc()) {
        let command = command_of(&handler);
        let args = hook_args(command)
            .unwrap_or_else(|| panic!("{event}: `{command}` is not `agora hook <sub>`"));
        let (sub, flags) = args.split_first().expect("hook command names a subcommand");
        assert!(
            HOOK_SUBCOMMANDS.contains(sub),
            "{event}: `{sub}` is not a known hook subcommand"
        );
        assert!(
            flags.is_empty() || flags == ["--quick"],
            "{event}: `{command}` has flags other than `--quick`"
        );
    }
}

#[test]
fn every_hook_command_line_is_accepted_by_the_agora_binary() {
    for (event, _, handler) in handlers(&hooks_doc()) {
        let command = command_of(&handler);
        let args = hook_args(command).expect("agora hook command");
        let output = Command::cargo_bin("agora")
            .expect("agora binary")
            .arg("hook")
            .args(&args)
            .arg("--help")
            .timeout(CMD_TIMEOUT)
            .output()
            .expect("run agora");
        assert!(
            output.status.success(),
            "{event}: `{command} --help` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Usage"),
            "{event}: `{command} --help` printed no usage: {stdout}"
        );
    }
}

#[test]
fn async_rewake_is_set_exactly_on_the_rewake_handlers() {
    let mut async_commands = BTreeSet::new();
    for (event, _, handler) in handlers(&hooks_doc()) {
        let command = command_of(&handler);
        let is_async = handler["asyncRewake"] == json!(true);
        let is_rewake = hook_args(command).and_then(|a| a.first().copied()) == Some("rewake");
        assert_eq!(
            is_async, is_rewake,
            "{event}: `{command}` must set asyncRewake exactly when it is a rewake hook"
        );
        if is_async {
            async_commands.insert(command.to_owned());
        }
    }
    let expected: BTreeSet<String> = ["agora hook rewake", "agora hook rewake --quick"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(async_commands, expected);
}

#[test]
fn rewake_timeouts_cover_a_long_stop_wait_and_a_short_startup_block() {
    // bug anthropics/claude-code#89960: a SessionStart asyncRewake hook can block startup for its
    // full timeout in -p and desktop modes, so that one stays short. The Stop long-poll needs room.
    let all = handlers(&hooks_doc());
    assert!(
        rewake_timeout(&all, "Stop") >= 600.0,
        "Stop rewake timeout must be at least 600 s"
    );
    assert!(
        rewake_timeout(&all, "SessionStart") <= 10.0,
        "SessionStart rewake timeout must be at most 10 s"
    );
}

#[test]
fn post_tool_use_matcher_selects_agora_inbox_tools_only() {
    let all = handlers(&hooks_doc());
    let matcher = all
        .iter()
        .find(|(event, _, _)| event == "PostToolUse")
        .and_then(|(_, m, _)| m.clone())
        .expect("PostToolUse has a matcher");
    assert!(matcher_matches(&matcher, "mcp__plugin_agora_agora__inbox"));
    assert!(matcher_matches(&matcher, "mcp__agora__inbox"));
    assert!(!matcher_matches(&matcher, "mcp__other__inbox"));
    assert!(!matcher_matches(&matcher, "Bash"));
}

#[test]
fn register_js_has_exactly_one_agora_placeholder_line() {
    let source = read("hooks/register.js");
    let count = source
        .lines()
        .filter(|line| *line == AGORA_PLACEHOLDER)
        .count();
    assert_eq!(
        count, 1,
        "register.js must contain exactly one line `{AGORA_PLACEHOLDER}`"
    );
}

#[test]
fn register_js_submits_prompts_and_never_as_user() {
    // Comments may mention asUser; only code counts.
    let code = register_js_code();
    assert!(
        code.contains("$.prompt.submit("),
        "the mod must submit polled messages"
    );
    assert!(
        !code.contains("asUser"),
        "the mod must never submit with asUser"
    );
}

#[test]
fn register_js_uses_no_node_apis_timers_or_dynamic_import() {
    // JSDoc type annotations such as `import('claude-code')` are comments, not code.
    assert!(!strip_js_comments("a // import(\nb").contains("import("));
    assert!(strip_js_comments("a \"//\" import('x')").contains("import('x')"));
    let code = register_js_code();
    for banned in ["require(", "setTimeout", "setInterval", "import("] {
        assert!(
            !code.contains(banned),
            "register.js code contains `{banned}`; the mod runtime has no Node APIs, no setTimeout and no dynamic import"
        );
    }
}

#[test]
fn register_js_has_no_static_import_statements() {
    // The mod may use only a bare import of 'claude-code'; it has none today.
    let code = register_js_code();
    let imports: Vec<&str> = code
        .lines()
        .map(str::trim_start)
        .filter(|line| {
            line.starts_with("import ")
                || line.starts_with("import{")
                || line.starts_with("import*")
        })
        .collect();
    assert!(
        imports.is_empty(),
        "register.js has import statements: {imports:?}"
    );
}

#[test]
fn plugin_files_contain_no_token_shaped_agora_strings() {
    // The detector must flag a 20-character token and ignore 19 characters and real tool names.
    assert!(has_token_shape("key agora_abcdefghijklmnopqrst end"));
    assert!(!has_token_shape("key agora_abcdefghijklmnopqrs end"));
    assert!(!has_token_shape("mcp__plugin_agora_agora__inbox"));
    for (rel, text) in plugin_texts() {
        assert!(
            !has_token_shape(&text),
            "{rel} contains a token-shaped `agora_` string"
        );
    }
}

#[test]
fn plugin_files_contain_no_private_key_block() {
    for (rel, text) in plugin_texts() {
        assert!(
            !text.contains("BEGIN PRIVATE KEY"),
            "{rel} contains a private key block"
        );
    }
}

#[test]
fn plugin_files_contain_no_seed_or_key_shaped_values() {
    // A seed or key is 64 lowercase hex digits: the detector must flag 64 and ignore 63.
    assert!(has_hex_run(&"0123456789abcdef".repeat(4), 64));
    assert!(!has_hex_run(&"a".repeat(63), 64));
    for (rel, text) in plugin_texts() {
        assert!(
            !has_hex_run(&text, 64),
            "{rel} contains a 64-character lowercase hex run"
        );
        assert!(!text.contains("\"seed\""), "{rel} has a \"seed\" JSON key");
    }
}

#[test]
fn plugin_files_contain_no_absolute_users_path() {
    for (rel, text) in plugin_texts() {
        assert!(
            !text.contains("/Users/"),
            "{rel} contains an absolute /Users/ path"
        );
    }
}

#[test]
fn skill_frontmatter_names_agora_and_describes_when_to_use_it() {
    let text = read("skills/agora/SKILL.md");
    let fm = frontmatter(&text);
    assert!(
        fm.contains(&"name: agora"),
        "SKILL.md frontmatter has no `name: agora`"
    );
    assert!(
        fm.iter().any(|line| line.starts_with("description:")),
        "SKILL.md frontmatter has no description"
    );
}

#[test]
fn skill_states_the_untrusted_content_ack_and_inbox_wait_rules() {
    let text = read("skills/agora/SKILL.md");
    for word in ["untrusted", "ack", "inbox_wait"] {
        assert!(
            mentions_word(&text, word),
            "SKILL.md does not mention `{word}`"
        );
    }
}

#[test]
fn command_files_have_frontmatter_with_a_description() {
    for rel in ["commands/inbox.md", "commands/status.md"] {
        let text = read(rel);
        let has_description = frontmatter(&text).iter().any(|line| {
            line.strip_prefix("description:")
                .is_some_and(|d| !d.trim().is_empty())
        });
        assert!(has_description, "{rel} has no non-empty description");
    }
}

#[test]
fn command_allowed_tools_name_only_agora_plugin_tools() {
    for rel in ["commands/inbox.md", "commands/status.md"] {
        let text = read(rel);
        let line = frontmatter(&text)
            .into_iter()
            .find_map(|line| line.strip_prefix("allowed-tools:"))
            .unwrap_or_else(|| panic!("{rel} has no allowed-tools line"));
        let tools: Vec<&str> = line
            .split(',')
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .collect();
        assert!(!tools.is_empty(), "{rel} allows no tools");
        for tool in tools {
            assert!(
                tool.starts_with(PLUGIN_TOOL_PREFIX),
                "{rel}: `{tool}` is not a plugin agora tool"
            );
        }
    }
}

#[test]
fn embedded_plugin_files_match_the_files_on_disk() {
    let assets_path = repo_root().join("crates/agora-cli/src/install/assets.rs");
    let assets = fs::read_to_string(&assets_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", assets_path.display()));
    let embedded: BTreeSet<String> = include_paths(&assets).into_iter().collect();
    let on_disk: BTreeSet<String> = shipped_files().into_iter().collect();
    assert_eq!(
        embedded, on_disk,
        "assets.rs must embed exactly the shipped plugin files"
    );
}

#[test]
fn marketplace_is_named_claudetalk_and_lists_agora_from_plugin_claude() {
    let doc = marketplace();
    assert_eq!(doc["name"], json!("claudetalk"));
    let agora: Vec<Value> = marketplace_plugins(&doc)
        .into_iter()
        .filter(|p| p["name"] == json!("agora"))
        .collect();
    assert_eq!(agora.len(), 1, "exactly one agora plugin entry");
    assert_eq!(agora[0]["source"], json!("./plugin/claude"));
}

#[test]
fn marketplace_keeps_the_legacy_claudetalk_plugin_entry() {
    let doc = marketplace();
    let legacy = marketplace_plugins(&doc)
        .into_iter()
        .filter(|p| p["name"] == json!("claudetalk"))
        .count();
    assert_eq!(legacy, 1, "the legacy claudetalk plugin entry must stay");
}

#[test]
fn claude_plugin_validate_accepts_the_plugin() {
    // Opt-in: needs the Claude Code CLI on PATH and AGORA_TEST_CLAUDE=1. Otherwise it passes
    // without checking anything, so a machine without Claude Code is never a failure.
    if env::var("AGORA_TEST_CLAUDE").as_deref() != Ok("1") || !on_path("claude") {
        return;
    }
    Command::new("claude")
        .args(["plugin", "validate"])
        .arg(plugin_dir())
        .timeout(Duration::from_secs(120))
        .assert()
        .success();
}
