//! `agora install` / `agora uninstall`: configure agent clients to use Agora.
//!
//! `--client` takes one client, a comma-separated list, or `all` (SPEC 4.1). A list that is exactly
//! `claude` runs the Claude path; any other list runs `multi`. Neither command loads an identity or
//! talks to the daemon.

pub mod agy;
pub mod assets;
pub mod claude;
pub mod client;
pub mod codex;
pub mod codex_toml;
pub mod codex_trust;
pub mod copilot_cli;
pub mod cursor;
pub mod instructions;
pub mod json_merge;
pub mod markers;
pub mod multi;
pub mod opencode;
pub mod paths;
pub mod probe;
pub mod probe_clients;
pub mod safe_write;
pub mod text_write;
pub mod vscode;

use crate::{CliError, InstallArgs, ModeArg, Out, ScopeArg};
use agora_client::capabilities::{self, ClaudeCapabilities};
use agora_daemon::{Paths, now_ms};
use claude::{Action, Mode, Options, Scope};
use client::Client;
use probe::Probe;
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The client name of the Claude path (its JSON `client` field).
const CLIENT: &str = "claude";
/// Mode of the agora home when this command creates it.
const HOME_MODE: u32 = 0o700;
/// The dev-channel hint printed in plugin mode.
const CHANNEL_HINT: &str =
    "To enable channel push: claude --dangerously-load-development-channels plugin:agora@agora";
/// Printed after a project-scope install: the written project files hold this machine's paths.
pub(super) const PROJECT_NOTE: &str = "note: absolute paths to this machine's agora binary and home were written into project files; do not commit them if the project is shared";

/// Resolve the paths (`HOME`, the current directory, the current executable canonicalized,
/// `home` made absolute), optionally `--probe`, run `claude::install`, write
/// `capabilities.json` (every non-dry-run install; the probe fields are empty/false without
/// `--probe`) and return the result: text lines per action (ending with the plugin-mode hint
/// `If Claude Code does not list the plugin, run: claude plugin marketplace add <dir> && claude
/// plugin install agora@agora`) and JSON
/// `{"client":"claude","scope":..,"mode":..,"dry_run":b,"actions":[{path,result,detail}],
/// "probe":{..}|null,"capabilities":{..}}`.
///
/// `--client` is parsed first (`client::parse_clients`); `--mode entries` without `claude` in the
/// list is a usage error, since the mode is Claude-only. `--trust-hooks` is read by Codex only: with
/// no `codex` in the list it is silently ignored.
pub fn install(home: PathBuf, args: InstallArgs) -> Result<Out, CliError> {
    let clients = client::parse_clients(&args.client)?;
    check_mode(&clients, args.mode)?;
    let o = resolve_options(home, &args)?;
    dispatch_install(&o, &clients, args.probe, args.trust_hooks)
}

/// The install of a non-empty client list under the resolved options: exactly `[claude]` runs
/// `install_claude` (today's output); every other list runs `multi::install`.
fn dispatch_install(
    o: &Options,
    clients: &[Client],
    probe: bool,
    trust_hooks: bool,
) -> Result<Out, CliError> {
    match clients {
        [Client::Claude] => install_claude(o, probe),
        _ => multi::install(o, clients, probe, trust_hooks),
    }
}

/// `--mode` applies to the Claude client only. Usage error when `entries` is asked for and the
/// client list has no `claude`; the default `plugin` is accepted for any list.
fn check_mode(clients: &[Client], mode: ModeArg) -> Result<(), CliError> {
    if mode == ModeArg::Entries && !clients.contains(&Client::Claude) {
        return Err(CliError::Usage(
            "--mode applies to --client claude only".into(),
        ));
    }
    Ok(())
}

/// The Claude install path (`--client claude`): everything `install` does after resolving the
/// options. `multi::install` reuses it for `claude` inside a client list and embeds its `Out` as is.
pub(crate) fn install_claude(o: &Options, probe_requested: bool) -> Result<Out, CliError> {
    let now = now_ms();
    if !o.dry_run {
        // Before `claude::install` writes into the home: the home must exist (0700 when this call
        // creates it), and a symlinked capabilities.json must fail before any file is written.
        ensure_home(&o.agora_home)?;
        check_capabilities_target(&Paths::new(&o.agora_home))?;
    }
    let mut actions = claude::install(o)?;
    let probe = if probe_requested {
        Some(probe::probe(probe::find_claude().as_deref()))
    } else {
        None
    };
    let caps = probe::capabilities(probe.as_ref(), o, now);
    if !o.dry_run {
        actions.extend(record_capabilities(o, &caps, now)?);
    }
    Ok(install_out(o, &actions, probe.as_ref(), &caps))
}

/// Like `install` for `claude::uninstall`; also removes the `claude` entry of
/// `capabilities.json`. Idempotent: a second run reports `unchanged` / `absent` actions.
/// The client list is parsed and checked like `install`; exactly `[claude]` runs `uninstall_claude`,
/// every other list `multi::uninstall`.
pub fn uninstall(home: PathBuf, args: InstallArgs) -> Result<Out, CliError> {
    let clients = client::parse_clients(&args.client)?;
    check_mode(&clients, args.mode)?;
    let o = resolve_options(home, &args)?;
    dispatch_uninstall(&o, &clients)
}

/// The uninstall of a non-empty client list under the resolved options (see `dispatch_install`).
fn dispatch_uninstall(o: &Options, clients: &[Client]) -> Result<Out, CliError> {
    match clients {
        [Client::Claude] => uninstall_claude(o),
        _ => multi::uninstall(o, clients),
    }
}

/// The Claude uninstall path; `multi::uninstall` reuses it for `claude` inside a client list.
pub(crate) fn uninstall_claude(o: &Options) -> Result<Out, CliError> {
    let mut actions = claude::uninstall(o)?;
    if !o.dry_run {
        actions.push(clear_capabilities(o));
    }
    Ok(uninstall_out(o, &actions))
}

/// Read the process environment and the filesystem, then resolve `Options`. The exe is the
/// `current_exe` path as reported (not canonicalized), made absolute when relative.
fn resolve_options(home: PathBuf, args: &InstallArgs) -> Result<Options, CliError> {
    let cwd = std::env::current_dir()?;
    let exe = absolute_against(std::env::current_exe()?, &cwd);
    let project = canonical_or(cwd);
    options_from(home, args, std::env::var_os("HOME"), project, exe)
}

/// Pure resolution: `user_home` must be non-empty; a relative `home` is taken against the
/// current directory. A home or exe path containing `${` is refused.
pub(crate) fn options_from(
    home: PathBuf,
    args: &InstallArgs,
    user_home: Option<OsString>,
    project_dir: PathBuf,
    exe: PathBuf,
) -> Result<Options, CliError> {
    let user_home = user_home
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CliError::Usage("HOME is not set".into()))?;
    let agora_home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir()?.join(home)
    };
    reject_placeholder("agora home", &agora_home)?;
    reject_placeholder("agora binary", &exe)?;
    Ok(Options {
        scope: match args.scope {
            ScopeArg::User => Scope::User,
            ScopeArg::Project => Scope::Project,
        },
        mode: match args.mode {
            ModeArg::Plugin => Mode::Plugin,
            ModeArg::Entries => Mode::Entries,
        },
        dry_run: args.dry_run,
        agora_home,
        user_home,
        project_dir,
        exe,
    })
}

/// `path` canonicalized, or as given when that fails.
fn canonical_or(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

/// `path` as is when absolute, else joined onto `cwd`.
fn absolute_against(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Refuse a path containing `${`: Claude Code expands `${...}` in the files the installer
/// writes, so such a path would be changed silently.
fn reject_placeholder(what: &str, path: &Path) -> Result<(), CliError> {
    if path.to_string_lossy().contains("${") {
        return Err(CliError::Usage(format!(
            "the {what} path {} contains \"${{\"; Claude Code would expand it, so use a path without it",
            path.display()
        )));
    }
    Ok(())
}

/// Create the agora home (`create_dir_all`) when it does not exist and, only when this call
/// created it, set it to 0700. An existing home keeps its mode. Not called by dry runs.
pub(crate) fn ensure_home(home: &Path) -> io::Result<()> {
    if home.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(home)?;
    fs::set_permissions(home, fs::Permissions::from_mode(HOME_MODE))
}

/// `Ok(true)` when `capabilities.json` is a regular file, `Ok(false)` when it is absent or not a
/// regular file. A symlink is refused, never followed.
pub(crate) fn check_capabilities_target(paths: &Paths) -> Result<bool, CliError> {
    let target = capabilities::path(paths);
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => Err(CliError::Usage(format!(
            "refusing to write {} because it is a symlink",
            target.display()
        ))),
        Ok(meta) => Ok(meta.is_file()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// `<target>.agora-bak-<now>`, in the same directory.
pub(crate) fn backup_path(target: &Path, now: i64) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(".agora-bak-{now}"));
    PathBuf::from(name)
}

/// What to do with `capabilities.json`: `"unchanged"` when the loaded file already equals the
/// new one, `"backed_up"` when a file exists but does not load (it is moved aside first), else
/// `"wrote"`.
pub(crate) fn capabilities_plan(exists: bool, loaded: bool, equal: bool) -> &'static str {
    match (loaded, exists, equal) {
        (true, _, true) => "unchanged",
        (false, true, _) => "backed_up",
        _ => "wrote",
    }
}

/// Write the probe-derived `claude` entry into `capabilities.json`, keeping other keys. An
/// unchanged file is not rewritten. A file that exists but does not load is moved aside first,
/// never dropped silently. A failed write is an error.
fn record_capabilities(
    o: &Options,
    caps: &ClaudeCapabilities,
    now: i64,
) -> Result<Vec<Action>, CliError> {
    let paths = Paths::new(&o.agora_home);
    let target = capabilities::path(&paths);
    let shown = target.display().to_string();
    let exists = check_capabilities_target(&paths)?;
    let loaded = capabilities::load(&paths);
    let mut file = loaded.clone().unwrap_or_default();
    file.claude = Some(caps.clone());
    let plan = capabilities_plan(exists, loaded.is_some(), loaded.as_ref() == Some(&file));
    if plan == "unchanged" {
        return Ok(vec![Action {
            path: shown,
            result: "unchanged".to_owned(),
            detail: "claude entry already up to date".to_owned(),
        }]);
    }
    let mut actions = Vec::with_capacity(2);
    if plan == "backed_up" {
        let bak = backup_path(&target, now);
        fs::rename(&target, &bak)?;
        actions.push(Action {
            path: shown.clone(),
            result: "backed_up".to_owned(),
            detail: format!(
                "unparsable capabilities.json moved aside to {}",
                bak.display()
            ),
        });
    }
    capabilities::save(&paths, &file)?;
    actions.push(Action {
        path: shown,
        result: "wrote".to_owned(),
        detail: "claude entry recorded".to_owned(),
    });
    Ok(actions)
}

/// Remove the `claude` entry of `capabilities.json`, keeping other keys. The entry is kept while
/// the other scope still uses agora.
fn clear_capabilities(o: &Options) -> Action {
    let paths = Paths::new(&o.agora_home);
    let shown = capabilities::path(&paths).display().to_string();
    let Some(mut file) = capabilities::load(&paths) else {
        return Action {
            path: shown,
            result: "absent".to_owned(),
            detail: "no readable capabilities.json".to_owned(),
        };
    };
    if file.claude.is_none() {
        return Action {
            path: shown,
            result: "unchanged".to_owned(),
            detail: String::new(),
        };
    }
    if claude::other_scope_uses_agora(o) {
        return Action {
            path: shown,
            result: "kept".to_owned(),
            detail: "still used by the other scope".to_owned(),
        };
    }
    file.claude = None;
    let (result, detail) = match capabilities::save(&paths, &file) {
        Ok(()) => ("wrote", "claude entry removed".to_owned()),
        Err(e) => ("error", e.to_string()),
    };
    Action {
        path: shown,
        result: result.to_owned(),
        detail,
    }
}

pub(crate) fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Plugin => "plugin",
        Mode::Entries => "entries",
    }
}

fn install_out(
    o: &Options,
    actions: &[Action],
    probe: Option<&Probe>,
    caps: &ClaudeCapabilities,
) -> Out {
    Out {
        json: json!({
            "client": CLIENT,
            "scope": scope_name(o.scope),
            "mode": mode_name(o.mode),
            "dry_run": o.dry_run,
            "actions": actions,
            "probe": probe,
            "capabilities": caps,
        }),
        text: install_text(o, actions, probe),
    }
}

fn uninstall_out(o: &Options, actions: &[Action]) -> Out {
    Out {
        json: json!({
            "client": CLIENT,
            "scope": scope_name(o.scope),
            "mode": mode_name(o.mode),
            "dry_run": o.dry_run,
            "actions": actions,
            "capabilities": null,
        }),
        text: render_actions(actions).join("\n"),
    }
}

/// Text of `install`: the actions, the probe summary, then the plugin hints.
fn install_text(o: &Options, actions: &[Action], probe: Option<&Probe>) -> String {
    let mut lines = render_actions(actions);
    if let Some(p) = probe {
        lines.extend(probe_lines(p));
    }
    if o.scope == Scope::Project && !o.dry_run {
        lines.push(PROJECT_NOTE.to_owned());
    }
    if o.mode == Mode::Plugin {
        lines.push(format!(
            "If Claude Code does not list the plugin, run: claude plugin marketplace add {} && claude plugin install agora@agora",
            shell_arg(&claude::marketplace_dir(o).display().to_string())
        ));
        lines.push(CHANNEL_HINT.to_owned());
    }
    lines.join("\n")
}

/// One `"<result> <path>"` line per action, then its detail lines indented.
fn render_actions(actions: &[Action]) -> Vec<String> {
    let mut lines = Vec::new();
    for a in actions {
        lines.push(format!("{} {}", a.result, a.path));
        lines.extend(a.detail.lines().map(|d| format!("  {d}")));
    }
    lines
}

fn probe_lines(p: &Probe) -> Vec<String> {
    let channels = match (p.channels_flag, p.help_lists_channels) {
        (true, true) => "yes",
        (true, false) => "yes (hidden from --help)",
        (false, _) => "no",
    };
    vec![
        format!("claude: {}", p.claude_bin.as_deref().unwrap_or("not found")),
        format!(
            "claude version: {}",
            p.version.as_deref().unwrap_or("unknown")
        ),
        format!("mods supported: {}", yes_no(p.mods_supported)),
        format!("channels flag: {channels}"),
        format!(
            "MCP_PROTOCOL_NEGOTIATION: {}",
            p.negotiation_env.as_deref().unwrap_or("unset")
        ),
    ]
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// `s` as one shell word: unchanged when it holds only plain path characters, else quoted.
fn shell_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c));
    if plain {
        s.to_owned()
    } else {
        claude::shell_quote(s)
    }
}

#[cfg(test)]
mod mod_tests;
