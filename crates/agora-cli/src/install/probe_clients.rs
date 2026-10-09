//! Read-only probes of the non-Claude clients (`agora install --probe`). Never starts a session
//! and never touches a config.

use super::client::Client;
use super::probe::{is_executable_file, parse_version, run_capture};
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClientProbe {
    /// Absolute path of the binary, `None` if not found.
    pub bin: Option<String>,
    /// First line of `<bin> --version`, cut to 100 chars.
    pub version: Option<String>,
    /// The binary supports the owned-session bridge (see `HarnessCapabilities::owned_session`).
    pub owned_session: bool,
    pub notes: Vec<String>,
}

/// Upper bound for each binary invocation made by the probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest version line kept in `ClientProbe::version`.
const VERSION_MAX_CHARS: usize = 100;
/// First Copilot CLI version whose hooks take effect (SPEC 2.2: the hooks are ignored before it).
const COPILOT_HOOKS_MIN: (u32, u32, u32) = (1, 0, 72);

/// Locate the binary (env `AGORA_<CLIENT>_BIN`: `AGORA_CODEX_BIN`, `AGORA_COPILOT_BIN`,
/// `AGORA_CODE_BIN`, `AGORA_AGY_BIN`, `AGORA_OPENCODE_BIN`, `AGORA_CURSOR_BIN`, else a PATH search
/// for `codex` / `copilot` / `code` / `agy` / `opencode` / `cursor-agent` then `agent`) and run only
/// `--version` / `--help` style commands through `probe::run_capture` with a 10 s timeout:
/// owned_session: codex -> `codex app-server --help` succeeds; copilot-cli -> `copilot --help`
/// mentions `--acp`; opencode -> `opencode serve --help` succeeds; agy -> `agy --help` mentions
/// `--conversation`; others false. `Client::Claude` -> default (not probed here).
pub fn probe(client: Client) -> ClientProbe {
    probe_with(
        client,
        &|name: &str| std::env::var_os(name),
        std::env::var_os("PATH"),
    )
}

/// `probe` with the environment lookup and `PATH` passed in, so tests need no process env.
fn probe_with(
    client: Client,
    env: &dyn Fn(&str) -> Option<OsString>,
    path: Option<OsString>,
) -> ClientProbe {
    let Some((env_var, names)) = locate(client) else {
        return ClientProbe::default();
    };
    let Some(bin) = find_bin(env_var, names, env, path) else {
        return ClientProbe {
            notes: vec![format!(
                "{} not found (set {env_var} or put it on PATH)",
                names[0]
            )],
            ..ClientProbe::default()
        };
    };
    let version = run_capture(&bin, &["--version"], PROBE_TIMEOUT)
        .as_deref()
        .and_then(|out| out.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|l| l.chars().take(VERSION_MAX_CHARS).collect::<String>());
    ClientProbe {
        bin: Some(bin.display().to_string()),
        version,
        owned_session: owned_session(client, &bin),
        notes: Vec::new(),
    }
}

/// The override env var and the binary names searched on `PATH`, in order. `None` for Claude,
/// which has its own probe (`install::probe`).
fn locate(client: Client) -> Option<(&'static str, &'static [&'static str])> {
    match client {
        Client::Claude => None,
        Client::Codex => Some(("AGORA_CODEX_BIN", &["codex"])),
        Client::CopilotCli => Some(("AGORA_COPILOT_BIN", &["copilot"])),
        Client::CopilotVsCode => Some(("AGORA_CODE_BIN", &["code"])),
        Client::Agy => Some(("AGORA_AGY_BIN", &["agy"])),
        Client::OpenCode => Some(("AGORA_OPENCODE_BIN", &["opencode"])),
        Client::Cursor => Some(("AGORA_CURSOR_BIN", &["cursor-agent", "agent"])),
    }
}

/// The override when it names an executable file, else the first executable match on `PATH`,
/// trying each name over all `PATH` dirs before the next name. Empty `PATH` entries are skipped
/// (they would mean the current directory). The result is made absolute, not symlink-resolved.
fn find_bin(
    env_var: &str,
    names: &[&str],
    env: &dyn Fn(&str) -> Option<OsString>,
    path: Option<OsString>,
) -> Option<PathBuf> {
    let overridden = env(env_var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| is_executable_file(p));
    if let Some(bin) = overridden {
        return Some(std::path::absolute(&bin).unwrap_or(bin));
    }
    let path = path?;
    names.iter().find_map(|name| {
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable_file(candidate))
            .map(|found| std::path::absolute(&found).unwrap_or(found))
    })
}

/// Whether `bin` supports the owned-session bridge, by a help call only (see `probe`).
fn owned_session(client: Client, bin: &Path) -> bool {
    let help = |args: &[&str]| run_capture(bin, args, PROBE_TIMEOUT);
    match client {
        Client::Codex => help(&["app-server", "--help"]).is_some(),
        Client::CopilotCli => help(&["--help"]).is_some_and(|h| h.contains("--acp")),
        Client::OpenCode => help(&["serve", "--help"]).is_some(),
        Client::Agy => help(&["--help"]).is_some_and(|h| h.contains("--conversation")),
        Client::Claude | Client::CopilotVsCode | Client::Cursor => false,
    }
}

/// Whether Copilot CLI hooks take effect for the given `--version` text (SPEC 2.2): true iff the
/// first `X.Y.Z` in it is at least 1.0.72. `None` (no probe) is false.
pub fn copilot_hooks_effective(version: Option<&str>) -> bool {
    version
        .and_then(parse_version)
        .is_some_and(|v| v >= COPILOT_HOOKS_MIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    /// Write `#!/bin/sh` + `body` to `dir/name`, executable.
    fn write_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn path_of(dir: &Path) -> Option<OsString> {
        Some(dir.as_os_str().to_owned())
    }

    #[test]
    fn codex_owned_session_follows_app_server_help_exit_status() {
        let dir = tempdir();
        write_bin(
            dir.path(),
            "codex",
            r#"case "$*" in
  --version) echo 'codex-cli 0.157.1' ;;
  'app-server --help') echo 'Usage: codex app-server' ;;
  *) exit 2 ;;
esac"#,
        );
        let p = probe_with(Client::Codex, &no_env, path_of(dir.path()));
        assert!(p.owned_session);
        assert_eq!(p.version.as_deref(), Some("codex-cli 0.157.1"));
        assert!(p.notes.is_empty());

        let failing = tempdir();
        write_bin(
            failing.path(),
            "codex",
            r#"case "$*" in
  --version) echo 'codex-cli 0.157.1' ;;
  *) exit 1 ;;
esac"#,
        );
        let p = probe_with(Client::Codex, &no_env, path_of(failing.path()));
        assert!(!p.owned_session, "a failing app-server --help is not owned");
        assert_eq!(p.version.as_deref(), Some("codex-cli 0.157.1"));
    }

    #[test]
    fn copilot_owned_session_needs_acp_in_help() {
        let dir = tempdir();
        write_bin(
            dir.path(),
            "copilot",
            r#"case "$*" in
  --version) printf '%s\n' '0.0.369' 'Commit: abc' ;;
  --help) printf '%s\n' 'Usage: copilot' '  --acp  Start the ACP server' ;;
  *) exit 2 ;;
esac"#,
        );
        let p = probe_with(Client::CopilotCli, &no_env, path_of(dir.path()));
        assert!(p.owned_session);
        assert_eq!(p.version.as_deref(), Some("0.0.369"));

        let old = tempdir();
        write_bin(
            old.path(),
            "copilot",
            "case \"$*\" in\n  --version) echo '0.0.300' ;;\n  --help) echo 'Usage: copilot' ;;\nesac",
        );
        let p = probe_with(Client::CopilotCli, &no_env, path_of(old.path()));
        assert!(!p.owned_session, "help without --acp is not owned");
    }

    #[test]
    fn env_override_wins_only_when_it_names_an_executable() {
        let exec_dir = tempdir();
        let custom = write_bin(exec_dir.path(), "opencode-custom", "echo 'opencode 1.2.3'");
        let path_dir = tempdir();
        write_bin(path_dir.path(), "opencode", "echo 'opencode 9.9.9'");

        let env =
            |name: &str| (name == "AGORA_OPENCODE_BIN").then(|| custom.clone().into_os_string());
        let p = probe_with(Client::OpenCode, &env, path_of(path_dir.path()));
        assert_eq!(p.bin, Some(custom.display().to_string()));
        assert_eq!(p.version.as_deref(), Some("opencode 1.2.3"));

        let plain = exec_dir.path().join("plain");
        std::fs::write(&plain, "not executable").expect("write");
        let env =
            |name: &str| (name == "AGORA_OPENCODE_BIN").then(|| plain.clone().into_os_string());
        let p = probe_with(Client::OpenCode, &env, path_of(path_dir.path()));
        assert_eq!(
            p.bin,
            Some(path_dir.path().join("opencode").display().to_string()),
            "a non-executable override falls through to PATH"
        );
        assert_eq!(p.version.as_deref(), Some("opencode 9.9.9"));
    }

    #[test]
    fn missing_binary_adds_a_note_and_nothing_else() {
        let p = probe_with(Client::Codex, &no_env, None);
        assert_eq!(
            p,
            ClientProbe {
                notes: vec!["codex not found (set AGORA_CODEX_BIN or put it on PATH)".into()],
                ..ClientProbe::default()
            }
        );
    }

    #[test]
    fn cursor_tries_cursor_agent_over_all_dirs_before_agent() {
        let first = tempdir();
        write_bin(first.path(), "agent", "echo 'agent 1.0'");
        let second = tempdir();
        write_bin(second.path(), "cursor-agent", "echo 'cursor-agent 2.0'");
        let path = std::env::join_paths([first.path(), second.path()]).expect("join");
        let p = probe_with(Client::Cursor, &no_env, Some(path));
        assert_eq!(
            p.bin,
            Some(second.path().join("cursor-agent").display().to_string())
        );
        assert_eq!(p.version.as_deref(), Some("cursor-agent 2.0"));
        assert!(!p.owned_session);
    }

    #[test]
    fn version_is_first_non_empty_line_cut_to_100_chars() {
        let dir = tempdir();
        let long = "x".repeat(150);
        write_bin(
            dir.path(),
            "code",
            &format!("printf '\\n\\n  %s\\nsecond line\\n' '{long}'"),
        );
        let p = probe_with(Client::CopilotVsCode, &no_env, path_of(dir.path()));
        assert_eq!(p.version.as_ref().map(|v| v.chars().count()), Some(100));
        assert!(p.version.is_some_and(|v| v.chars().all(|c| c == 'x')));
        assert!(!p.owned_session, "VS Code is probed for its version only");
    }

    #[test]
    fn claude_is_not_probed_here() {
        let dir = tempdir();
        write_bin(dir.path(), "claude", "echo '2.1.293'");
        assert_eq!(
            probe_with(Client::Claude, &no_env, path_of(dir.path())),
            ClientProbe::default()
        );
    }

    #[test]
    fn copilot_hooks_effective_needs_1_0_72() {
        assert!(!copilot_hooks_effective(Some("0.0.369")));
        assert!(copilot_hooks_effective(Some("1.0.72")));
        assert!(copilot_hooks_effective(Some(
            "GitHub Copilot CLI 1.0.93\nCommit: x"
        )));
        assert!(!copilot_hooks_effective(None));
    }
}
