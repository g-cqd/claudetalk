//! `agora install` / `agora uninstall`: configure an agent client to use Agora.
//!
//! M4 supports `--client claude` only (others -> usage error "client X is planned for M6").
//! Neither command loads an identity or talks to the daemon.

pub mod assets;
pub mod claude;
pub mod probe;
pub mod safe_write;

use crate::{CliError, InstallArgs, ModeArg, Out, ScopeArg};
use agora_client::capabilities::{self, ClaudeCapabilities};
use agora_daemon::{Paths, now_ms};
use claude::{Action, Mode, Options, Scope};
use probe::Probe;
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The only client M4 configures.
const CLIENT: &str = "claude";
/// Mode of the agora home when this command creates it.
const HOME_MODE: u32 = 0o700;
/// The dev-channel hint printed in plugin mode.
const CHANNEL_HINT: &str =
    "To enable channel push: claude --dangerously-load-development-channels plugin:agora@agora";
/// Printed after a project-scope install: the written project files hold this machine's paths.
const PROJECT_NOTE: &str = "note: absolute paths to this machine's agora binary and home were written into project files; do not commit them if the project is shared";

/// Resolve the paths (`HOME`, the current directory, the current executable canonicalized,
/// `home` made absolute), optionally `--probe`, run `claude::install`, write
/// `capabilities.json` (every non-dry-run install; the probe fields are empty/false without
/// `--probe`) and return the result: text lines per action (ending with the plugin-mode hint
/// `If Claude Code does not list the plugin, run: claude plugin marketplace add <dir> && claude
/// plugin install agora@agora`) and JSON
/// `{"client":"claude","scope":..,"mode":..,"dry_run":b,"actions":[{path,result,detail}],
/// "probe":{..}|null,"capabilities":{..}}`.
pub fn install(home: PathBuf, args: InstallArgs) -> Result<Out, CliError> {
    check_client(&args.client)?;
    let o = resolve_options(home, &args)?;
    let now = now_ms();
    if !o.dry_run {
        // Before `claude::install` writes into the home: the home must exist (0700 when this call
        // creates it), and a symlinked capabilities.json must fail before any file is written.
        ensure_home(&o.agora_home)?;
        check_capabilities_target(&Paths::new(&o.agora_home))?;
    }
    let mut actions = claude::install(&o)?;
    let probe = if args.probe {
        Some(probe::probe(probe::find_claude().as_deref()))
    } else {
        None
    };
    let caps = probe::capabilities(probe.as_ref(), &o, now);
    if !o.dry_run {
        actions.extend(record_capabilities(&o, &caps, now)?);
    }
    Ok(install_out(&o, &actions, probe.as_ref(), &caps))
}

/// Like `install` for `claude::uninstall`; also removes the `claude` entry of
/// `capabilities.json`. Idempotent: a second run reports `unchanged` / `absent` actions.
pub fn uninstall(home: PathBuf, args: InstallArgs) -> Result<Out, CliError> {
    check_client(&args.client)?;
    let o = resolve_options(home, &args)?;
    let mut actions = claude::uninstall(&o)?;
    if !o.dry_run {
        actions.push(clear_capabilities(&o));
    }
    Ok(uninstall_out(&o, &actions))
}

fn check_client(client: &str) -> Result<(), CliError> {
    if client == CLIENT {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "client {client} is planned for M6; only `claude` is supported"
        )))
    }
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
fn options_from(
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
fn ensure_home(home: &Path) -> io::Result<()> {
    if home.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(home)?;
    fs::set_permissions(home, fs::Permissions::from_mode(HOME_MODE))
}

/// `Ok(true)` when `capabilities.json` is a regular file, `Ok(false)` when it is absent or not a
/// regular file. A symlink is refused, never followed.
fn check_capabilities_target(paths: &Paths) -> Result<bool, CliError> {
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
fn backup_path(target: &Path, now: i64) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(".agora-bak-{now}"));
    PathBuf::from(name)
}

/// What to do with `capabilities.json`: `"unchanged"` when the loaded file already equals the
/// new one, `"backed_up"` when a file exists but does not load (it is moved aside first), else
/// `"wrote"`.
fn capabilities_plan(exists: bool, loaded: bool, equal: bool) -> &'static str {
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

fn scope_name(scope: Scope) -> &'static str {
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
mod tests {
    use super::*;

    fn args(scope: ScopeArg, mode: ModeArg, dry_run: bool) -> InstallArgs {
        InstallArgs {
            client: CLIENT.to_owned(),
            scope,
            mode,
            probe: false,
            dry_run,
        }
    }

    fn opts_at(agora_home: PathBuf, mode: Mode) -> Options {
        Options {
            scope: Scope::User,
            mode,
            dry_run: false,
            agora_home,
            user_home: PathBuf::from("/home/u"),
            project_dir: PathBuf::from("/work/proj"),
            exe: PathBuf::from("/usr/local/bin/agora"),
        }
    }

    fn opts(mode: Mode) -> Options {
        opts_at(PathBuf::from("/home/u/.agora"), mode)
    }

    fn action(path: &str, result: &str, detail: &str) -> Action {
        Action {
            path: path.to_owned(),
            result: result.to_owned(),
            detail: detail.to_owned(),
        }
    }

    fn sample_caps() -> ClaudeCapabilities {
        ClaudeCapabilities {
            claude_version: Some("2.1.293".to_owned()),
            mods: true,
            async_rewake: true,
            ..ClaudeCapabilities::default()
        }
    }

    #[test]
    fn unknown_client_is_a_usage_error_for_install_and_uninstall() {
        for client in ["codex", "Claude", ""] {
            let mut a = args(ScopeArg::User, ModeArg::Plugin, false);
            a.client = client.to_owned();
            let Err(err) = install(PathBuf::from("/nonexistent/agora"), a) else {
                panic!("install must reject client {client:?}");
            };
            assert!(
                matches!(err, CliError::Usage(ref m) if m.contains("planned for M6")),
                "{err}"
            );

            let mut a = args(ScopeArg::User, ModeArg::Plugin, false);
            a.client = client.to_owned();
            let Err(err) = uninstall(PathBuf::from("/nonexistent/agora"), a) else {
                panic!("uninstall must reject client {client:?}");
            };
            assert!(matches!(err, CliError::Usage(_)), "{err}");
        }
    }

    #[test]
    fn scope_and_mode_map_to_options() {
        let o = options_from(
            PathBuf::from("/abs/home"),
            &args(ScopeArg::Project, ModeArg::Entries, true),
            Some(OsString::from("/h/u")),
            PathBuf::from("/proj"),
            PathBuf::from("/bin/agora"),
        )
        .expect("options");
        assert_eq!(o.scope, Scope::Project);
        assert_eq!(o.mode, Mode::Entries);
        assert!(o.dry_run);
        assert_eq!(o.agora_home, PathBuf::from("/abs/home"));
        assert_eq!(o.user_home, PathBuf::from("/h/u"));
        assert_eq!(o.project_dir, PathBuf::from("/proj"));
        assert_eq!(o.exe, PathBuf::from("/bin/agora"));

        let d = options_from(
            PathBuf::from("/abs/home"),
            &args(ScopeArg::User, ModeArg::Plugin, false),
            Some(OsString::from("/h/u")),
            PathBuf::from("/proj"),
            PathBuf::from("/bin/agora"),
        )
        .expect("options");
        assert_eq!(d.scope, Scope::User);
        assert_eq!(d.mode, Mode::Plugin);
        assert!(!d.dry_run);
    }

    #[test]
    fn relative_home_is_made_absolute_against_the_current_dir() {
        let o = options_from(
            PathBuf::from("rel/.agora"),
            &args(ScopeArg::User, ModeArg::Plugin, false),
            Some(OsString::from("/h/u")),
            PathBuf::from("/proj"),
            PathBuf::from("/bin/agora"),
        )
        .expect("options");
        assert!(o.agora_home.is_absolute());
        assert_eq!(
            o.agora_home,
            std::env::current_dir().expect("cwd").join("rel/.agora")
        );
    }

    #[test]
    fn missing_or_empty_home_env_is_a_usage_error() {
        for user_home in [None, Some(OsString::new())] {
            let Err(err) = options_from(
                PathBuf::from("/abs/home"),
                &args(ScopeArg::User, ModeArg::Plugin, false),
                user_home,
                PathBuf::from("/proj"),
                PathBuf::from("/bin/agora"),
            ) else {
                panic!("HOME must be required");
            };
            assert!(
                matches!(err, CliError::Usage(ref m) if m == "HOME is not set"),
                "{err}"
            );
        }
    }

    #[test]
    fn plugin_text_lists_actions_details_and_hints() {
        let actions = vec![action(
            "/a/settings.json",
            "wrote",
            "enabledPlugins.agora@agora = true\nextraKnownMarketplaces.agora.path = /m",
        )];
        let text = install_text(&opts(Mode::Plugin), &actions, None);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "wrote /a/settings.json");
        assert_eq!(lines[1], "  enabledPlugins.agora@agora = true");
        assert_eq!(lines[2], "  extraKnownMarketplaces.agora.path = /m");
        assert!(text.contains(
            "If Claude Code does not list the plugin, run: claude plugin marketplace add /home/u/.agora/claude/marketplace && claude plugin install agora@agora"
        ));
        assert!(lines.contains(&CHANNEL_HINT));
        assert!(!text.contains("claude version"), "no probe, no probe lines");
    }

    #[test]
    fn entries_text_has_no_plugin_hints() {
        let text = install_text(&opts(Mode::Entries), &[], None);
        assert!(!text.contains("marketplace add"));
        assert!(!text.contains("channel push"));
    }

    #[test]
    fn probe_summary_notes_channels_hidden_from_help() {
        let p = Probe {
            claude_bin: Some("/opt/claude".to_owned()),
            version: Some("2.1.293 (Claude Code)".to_owned()),
            version_parsed: Some((2, 1, 293)),
            mods_supported: true,
            channels_flag: true,
            help_lists_channels: false,
            negotiation_env: Some("legacy".to_owned()),
        };
        let text = install_text(&opts(Mode::Entries), &[], Some(&p));
        assert!(text.contains("claude: /opt/claude"));
        assert!(text.contains("claude version: 2.1.293 (Claude Code)"));
        assert!(text.contains("mods supported: yes"));
        assert!(text.contains("channels flag: yes (hidden from --help)"));
        assert!(text.contains("MCP_PROTOCOL_NEGOTIATION: legacy"));
    }

    #[test]
    fn install_json_has_the_documented_shape() {
        let o = opts(Mode::Plugin);
        let caps = sample_caps();
        let out = install_out(&o, &[action("/x", "dry_run", "")], None, &caps);
        assert_eq!(out.json["client"], "claude");
        assert_eq!(out.json["scope"], "user");
        assert_eq!(out.json["mode"], "plugin");
        assert_eq!(out.json["dry_run"], false);
        assert_eq!(out.json["actions"][0]["result"], "dry_run");
        assert!(out.json["probe"].is_null());
        assert_eq!(out.json["capabilities"]["claude_version"], "2.1.293");
    }

    #[test]
    fn uninstall_json_has_null_capabilities_and_no_probe() {
        let out = uninstall_out(&opts(Mode::Entries), &[action("/x", "removed", "")]);
        assert_eq!(out.json["mode"], "entries");
        assert!(out.json["capabilities"].is_null());
        assert!(out.json.get("probe").is_none());
        assert_eq!(out.text, "removed /x");
    }

    const NOW: i64 = 1_700_000_000_000;

    /// An existing agora home directory inside `dir`.
    fn make_home(dir: &Path) -> PathBuf {
        let home = dir.join("agora");
        std::fs::create_dir(&home).expect("home");
        home
    }

    #[test]
    fn record_writes_the_claude_entry_into_an_existing_home() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = make_home(dir.path());
        let actions =
            record_capabilities(&opts_at(home.clone(), Mode::Plugin), &sample_caps(), NOW)
                .expect("record");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].result, "wrote", "{}", actions[0].detail);
        let file = capabilities::load(&Paths::new(&home)).expect("capabilities.json loads");
        assert_eq!(file.claude, Some(sample_caps()));
    }

    #[test]
    fn unparsable_capabilities_are_moved_aside_before_the_fresh_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = make_home(dir.path());
        std::fs::write(capabilities::path(&Paths::new(&home)), "not json").expect("seed");
        let actions =
            record_capabilities(&opts_at(home.clone(), Mode::Plugin), &sample_caps(), NOW)
                .expect("record");
        let results: Vec<&str> = actions.iter().map(|a| a.result.as_str()).collect();
        assert_eq!(results, ["backed_up", "wrote"]);
        let bak = home.join(format!("capabilities.json.agora-bak-{NOW}"));
        assert_eq!(std::fs::read_to_string(&bak).expect("backup"), "not json");
        assert_eq!(
            actions[0].detail,
            format!(
                "unparsable capabilities.json moved aside to {}",
                bak.display()
            )
        );
        let fresh = capabilities::load(&Paths::new(&home)).expect("fresh file loads");
        assert_eq!(fresh.claude, Some(sample_caps()));
    }

    #[test]
    fn an_unchanged_capabilities_file_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = make_home(dir.path());
        let o = opts_at(home.clone(), Mode::Plugin);
        record_capabilities(&o, &sample_caps(), NOW).expect("first record");
        let file = capabilities::path(&Paths::new(&home));
        let before = std::fs::read(&file).expect("read");
        let actions = record_capabilities(&o, &sample_caps(), NOW + 1).expect("second record");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].result, "unchanged");
        assert_eq!(std::fs::read(&file).expect("read"), before);
        assert_eq!(
            std::fs::read_dir(&home).expect("list").count(),
            1,
            "no backup"
        );
    }

    #[test]
    fn a_symlinked_capabilities_file_is_refused_and_its_target_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = make_home(dir.path());
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, "keep").expect("target");
        std::os::unix::fs::symlink(&target, capabilities::path(&Paths::new(&home)))
            .expect("symlink");
        let Err(err) = record_capabilities(&opts_at(home, Mode::Plugin), &sample_caps(), NOW)
        else {
            panic!("a symlinked capabilities.json must be refused");
        };
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "keep");
    }

    #[test]
    fn a_failed_capabilities_write_is_an_error_not_an_action() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("not-a-dir");
        std::fs::write(&home, "x").expect("file");
        assert!(record_capabilities(&opts_at(home, Mode::Plugin), &sample_caps(), NOW).is_err());
    }

    #[test]
    fn capabilities_plan_backs_up_only_an_unloadable_file_and_skips_equal_ones() {
        assert_eq!(capabilities_plan(false, false, false), "wrote");
        assert_eq!(capabilities_plan(true, false, false), "backed_up");
        assert_eq!(capabilities_plan(true, true, false), "wrote");
        assert_eq!(capabilities_plan(true, true, true), "unchanged");
    }

    #[test]
    fn a_relative_exe_is_joined_onto_the_current_dir_and_an_absolute_one_is_kept() {
        assert_eq!(
            absolute_against(PathBuf::from("bin/agora"), Path::new("/work")),
            PathBuf::from("/work/bin/agora")
        );
        assert_eq!(
            absolute_against(PathBuf::from("/usr/bin/agora"), Path::new("/work")),
            PathBuf::from("/usr/bin/agora")
        );
    }

    #[test]
    fn a_placeholder_in_the_home_or_the_exe_is_a_usage_error() {
        let a = args(ScopeArg::User, ModeArg::Plugin, false);
        for (home, exe) in [
            ("/abs/${AGORA}", "/bin/agora"),
            ("/abs/home", "/bin/${X}/agora"),
        ] {
            let Err(err) = options_from(
                PathBuf::from(home),
                &a,
                Some(OsString::from("/h/u")),
                PathBuf::from("/proj"),
                PathBuf::from(exe),
            ) else {
                panic!("{home} / {exe} must be refused");
            };
            assert!(
                matches!(err, CliError::Usage(ref m) if m.contains("${")),
                "{err}"
            );
        }
        let plain = options_from(
            PathBuf::from("/abs/$HOME/x"),
            &a,
            Some(OsString::from("/h/u")),
            PathBuf::from("/proj"),
            PathBuf::from("/bin/agora"),
        );
        assert!(plain.is_ok(), "a lone $ is not a placeholder");
    }

    #[test]
    fn ensure_home_makes_a_private_home_and_keeps_an_existing_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("a").join("agora");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        ensure_home(&home).expect("create");
        assert_eq!(mode(&home), 0o700);
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        ensure_home(&home).expect("existing home");
        assert_eq!(mode(&home), 0o755, "an existing home keeps its mode");
    }

    #[test]
    fn project_scope_text_warns_about_absolute_paths_only_when_written() {
        let mut o = opts(Mode::Plugin);
        o.scope = Scope::Project;
        assert!(install_text(&o, &[], None).contains(PROJECT_NOTE));
        o.dry_run = true;
        assert!(!install_text(&o, &[], None).contains(PROJECT_NOTE));
        o.dry_run = false;
        o.scope = Scope::User;
        assert!(!install_text(&o, &[], None).contains(PROJECT_NOTE));
    }

    #[test]
    fn uninstall_keeps_the_claude_entry_while_the_other_scope_uses_agora() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = make_home(dir.path());
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join(".claude")).expect("project");
        let settings = project.join(".claude").join("settings.json");
        std::fs::write(&settings, r#"{"enabledPlugins":{"agora@agora":true}}"#).expect("settings");
        let mut o = opts_at(home.clone(), Mode::Plugin);
        o.project_dir = project;
        record_capabilities(&o, &sample_caps(), NOW).expect("record");

        assert_eq!(clear_capabilities(&o).result, "kept");
        let kept = capabilities::load(&Paths::new(&home)).expect("loads");
        assert_eq!(kept.claude, Some(sample_caps()));

        std::fs::remove_file(&settings).expect("remove");
        assert_eq!(clear_capabilities(&o).result, "wrote");
        let cleared = capabilities::load(&Paths::new(&home)).expect("loads");
        assert!(cleared.claude.is_none());
    }

    #[test]
    fn record_and_clear_keep_unknown_top_level_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("agora");
        std::fs::create_dir(&home).expect("home");
        let file = capabilities::path(&Paths::new(&home));
        std::fs::write(&file, r#"{"schema":1,"other_tool":{"x":1}}"#).expect("seed file");
        let o = opts_at(home.clone(), Mode::Plugin);

        assert_eq!(
            record_capabilities(&o, &sample_caps(), NOW).expect("record")[0].result,
            "wrote"
        );
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
        assert_eq!(kept["other_tool"]["x"], 1);
        assert!(kept["claude"].is_object());

        assert_eq!(clear_capabilities(&o).result, "wrote");
        let cleared: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
        assert_eq!(cleared["other_tool"]["x"], 1);
        assert!(cleared.get("claude").is_none());
        assert_eq!(clear_capabilities(&o).result, "unchanged");
    }

    #[test]
    fn clear_without_a_capabilities_file_is_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let o = opts_at(dir.path().join("missing"), Mode::Plugin);
        assert_eq!(clear_capabilities(&o).result, "absent");
    }
}
