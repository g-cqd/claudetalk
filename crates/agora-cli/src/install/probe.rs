//! `agora install --probe`: what the installed Claude Code supports.

use crate::install::claude::{Mode, Options, Scope};
use agora_client::capabilities::ClaudeCapabilities;
use serde::Serialize;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// What the probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub claude_bin: Option<String>,
    /// The `X.Y.Z` text.
    pub version: Option<String>,
    pub version_parsed: Option<(u32, u32, u32)>,
    /// version >= `MODS_MIN` (the install mode is checked by `capabilities`).
    pub mods_supported: bool,
    /// `--help` lists a channels flag, or version >= `MODS_MIN` (2.1.293 hides the flags from
    /// `--help` but supports them).
    pub channels_flag: bool,
    /// `--help` mentions `--channels` or `--dangerously-load-development-channels`.
    pub help_lists_channels: bool,
    /// Env `MCP_PROTOCOL_NEGOTIATION` of the current process.
    pub negotiation_env: Option<String>,
}

/// First Claude Code version with mods.
pub const MODS_MIN: (u32, u32, u32) = (2, 1, 287);

/// Upper bound for each `claude` invocation made by the probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest version line kept in `Probe::version`.
const VERSION_MAX_CHARS: usize = 100;
/// Most stdout bytes kept from one invocation; the rest is drained and discarded.
const CAPTURE_LIMIT: u64 = 1024 * 1024;

/// Env `AGORA_CLAUDE_BIN` (when it names an existing file), else `claude` found on `PATH`.
pub fn find_claude() -> Option<PathBuf> {
    pick_claude(
        std::env::var_os("AGORA_CLAUDE_BIN"),
        std::env::var_os("PATH"),
    )
}

/// The override when it names a file, else the first executable `claude` in the `PATH` dirs.
/// Empty `PATH` entries are skipped (they would mean the current directory).
fn pick_claude(override_bin: Option<OsString>, path: Option<OsString>) -> Option<PathBuf> {
    if let Some(bin) = override_bin.filter(|b| !b.is_empty()) {
        let candidate = PathBuf::from(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = path?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("claude"))
        .find(|candidate| is_executable_file(candidate))
}

pub(crate) fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run `bin args` with stdin null, capture stdout; kill the process after `timeout`; `None` on
/// spawn failure, non-zero status or timeout.
pub fn run_capture(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        reap_killed(&mut child);
        return None;
    };
    // The helper thread reads stdout to EOF; the deadline is the receive timeout below.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let bytes = read_capped(&mut stdout);
        let _ = tx.send(bytes);
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        // EOF on stdout: the child has closed its output. It may still be running (it can close
        // stdout and keep going), so reap it only until the same deadline.
        Ok(bytes) => {
            let status = reap_by(&mut child, deadline)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (status.success() && !text.is_empty()).then_some(text)
        }
        // Deadline passed (or the reader died): kill and reap.
        Err(_) => {
            reap_killed(&mut child);
            None
        }
    }
}

/// Collect the status of a child that has closed its stdout, polling until `deadline`. A child
/// still running at the deadline is killed and reaped, and `None` is returned.
fn reap_by(child: &mut std::process::Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                reap_killed(child);
                return None;
            }
        }
    }
}

/// Kill a child that has not finished and collect its status so it is not left a zombie.
fn reap_killed(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Read up to `CAPTURE_LIMIT` bytes, then drain the rest so the child never blocks on a full pipe.
fn read_capped(out: &mut ChildStdout) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = out.by_ref().take(CAPTURE_LIMIT).read_to_end(&mut buf);
    let _ = io::copy(out, &mut io::sink());
    buf
}

/// The first `X.Y.Z` (three dot-separated integers) in `s`, e.g. from `2.1.293 (Claude Code)`.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    // Candidate runs are maximal stretches of digits and dots; a run such as `1.2.3.4` yields its
    // first three parts. Empty or non-numeric parts reject the run.
    s.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|run| !run.is_empty())
        .find_map(|run| {
            let mut parts = run.split('.').map(|p| p.parse::<u32>().ok());
            let (x, y, z) = (parts.next()??, parts.next()??, parts.next()??);
            Some((x, y, z))
        })
}

/// `claude --version` and `claude --help` (each killed after 10 s) -> `Probe`. `bin == None`
/// -> a `Probe` with only `negotiation_env` set.
pub fn probe(bin: Option<&Path>) -> Probe {
    let negotiation_env = std::env::var("MCP_PROTOCOL_NEGOTIATION").ok();
    let Some(bin) = bin else {
        return Probe {
            negotiation_env,
            ..Probe::default()
        };
    };
    let version_out = run_capture(bin, &["--version"], PROBE_TIMEOUT);
    let line = version_out
        .as_deref()
        .and_then(|out| out.lines().map(str::trim).find(|l| !l.is_empty()));
    let version = line.map(|l| l.chars().take(VERSION_MAX_CHARS).collect::<String>());
    let version_parsed = line.and_then(parse_version);
    let mods_supported = version_parsed.is_some_and(|v| v >= MODS_MIN);

    let help = run_capture(bin, &["--help"], PROBE_TIMEOUT).unwrap_or_default();
    let help_lists_channels =
        help.contains("--channels") || help.contains("--dangerously-load-development-channels");

    Probe {
        claude_bin: Some(bin.display().to_string()),
        version,
        version_parsed,
        mods_supported,
        channels_flag: help_lists_channels || mods_supported,
        help_lists_channels,
        negotiation_env,
    }
}

/// The `capabilities.json` `claude` entry for an install with `o`: `mods` = version >=
/// `MODS_MIN` AND `o.mode` is plugin; `async_rewake`, `stop_hook`, `context_hook` true (the
/// hooks are installed in both modes); `channels_flag`, `claude_bin`, `claude_version` from
/// `p`; `mcp_negotiation_legacy` true only when `p.negotiation_env` is `legacy` (the env of the
/// shell running `agora install --probe`, a proxy for the shell that will run `claude`: Claude
/// Code reads `MCP_PROTOCOL_NEGOTIATION` from its own process env, not from the plugin's
/// `.mcp.json` server env); `install_mode` and `scope` from `o`; `probed_at_ms` = `now_ms`.
/// Without a probe (`p == None`) the probe-derived fields are empty/false.
pub fn capabilities(p: Option<&Probe>, o: &Options, now_ms: i64) -> ClaudeCapabilities {
    ClaudeCapabilities {
        probed_at_ms: if p.is_some() { now_ms } else { 0 },
        claude_bin: p.and_then(|p| p.claude_bin.clone()),
        claude_version: p.and_then(claude_version_text),
        mods: o.mode == Mode::Plugin && p.is_some_and(|p| p.mods_supported),
        channels_flag: p.is_some_and(|p| p.channels_flag),
        async_rewake: true,
        stop_hook: true,
        context_hook: true,
        mcp_negotiation_legacy: p.and_then(|p| p.negotiation_env.as_deref()) == Some("legacy"),
        install_mode: Some(
            match o.mode {
                Mode::Plugin => "plugin",
                Mode::Entries => "entries",
            }
            .to_string(),
        ),
        scope: Some(
            match o.scope {
                Scope::User => "user",
                Scope::Project => "project",
            }
            .to_string(),
        ),
    }
}

/// `X.Y.Z` from the parsed version, else the raw version line.
fn claude_version_text(p: &Probe) -> Option<String> {
    match p.version_parsed {
        Some((x, y, z)) => Some(format!("{x}.{y}.{z}")),
        None => p.version.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    /// Write `#!/bin/sh` + `body` to `dir/claude`, executable.
    fn write_stub(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("claude");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// A stub whose `--version` and `--help` print the given lines.
    fn stub(dir: &Path, version: &str, help: &str) -> PathBuf {
        write_stub(
            dir,
            &format!(
                "case \"$1\" in\n  --version) printf '%s\\n' '{version}' ;;\n  --help) printf '%s\\n' '{help}' ;;\nesac"
            ),
        )
    }

    fn opts(mode: Mode, scope: Scope) -> Options {
        Options {
            scope,
            mode,
            dry_run: false,
            agora_home: PathBuf::from("/home/u/.agora"),
            user_home: PathBuf::from("/home/u"),
            project_dir: PathBuf::from("/work/proj"),
            exe: PathBuf::from("/usr/local/bin/agora"),
        }
    }

    #[test]
    fn parse_version_cases() {
        assert_eq!(parse_version("2.1.293 (Claude Code)"), Some((2, 1, 293)));
        assert_eq!(parse_version("claude 10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_version("v2.1"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2.3.4"), Some((1, 2, 3)));
        assert_eq!(parse_version("99999999999.1.1"), None);
    }

    #[test]
    fn probe_293_has_mods_and_channels_without_help_flags() {
        let dir = tempdir();
        let bin = stub(
            dir.path(),
            "2.1.293 (Claude Code)",
            "Usage: claude [options]",
        );
        let p = probe(Some(bin.as_path()));
        assert_eq!(p.version.as_deref(), Some("2.1.293 (Claude Code)"));
        assert_eq!(p.version_parsed, Some((2, 1, 293)));
        assert!(p.mods_supported);
        assert!(p.channels_flag);
        assert!(!p.help_lists_channels);
        assert_eq!(p.claude_bin, Some(bin.display().to_string()));
    }

    #[test]
    fn probe_200_has_neither_mods_nor_channels() {
        let dir = tempdir();
        let bin = stub(dir.path(), "2.1.200", "Usage: claude [options]");
        let p = probe(Some(bin.as_path()));
        assert_eq!(p.version_parsed, Some((2, 1, 200)));
        assert!(!p.mods_supported);
        assert!(!p.channels_flag);
        assert!(!p.help_lists_channels);
    }

    #[test]
    fn mods_threshold_is_inclusive_at_2_1_287() {
        for (version, expected) in [("2.1.286", false), ("2.1.287", true), ("3.0.0", true)] {
            let dir = tempdir();
            let bin = stub(dir.path(), version, "Usage: claude [options]");
            let p = probe(Some(bin.as_path()));
            assert_eq!(p.mods_supported, expected, "version {version}");
        }
    }

    #[test]
    fn help_listing_channels_enables_channels_flag_on_old_claude() {
        let dir = tempdir();
        let bin = stub(
            dir.path(),
            "2.1.200",
            "  --dangerously-load-development-channels <spec>",
        );
        let p = probe(Some(bin.as_path()));
        assert!(p.help_lists_channels);
        assert!(p.channels_flag);
        assert!(!p.mods_supported);
    }

    #[test]
    fn probe_without_binary_only_reports_negotiation_env() {
        let p = probe(None);
        assert_eq!(
            p,
            Probe {
                negotiation_env: std::env::var("MCP_PROTOCOL_NEGOTIATION").ok(),
                ..Probe::default()
            }
        );
    }

    #[test]
    fn override_wins_when_it_names_a_file_and_is_ignored_otherwise() {
        let dir = tempdir();
        let bin = stub(dir.path(), "2.1.293", "");
        assert_eq!(
            pick_claude(Some(bin.clone().into_os_string()), None),
            Some(bin.clone())
        );
        let missing = dir.path().join("no-such-claude");
        assert_eq!(
            pick_claude(Some(missing.into_os_string()), None),
            None,
            "a missing override with no PATH finds nothing"
        );
        assert_eq!(
            pick_claude(
                Some(OsString::new()),
                Some(dir.path().as_os_str().to_owned())
            ),
            Some(bin),
            "an empty override falls through to PATH"
        );
    }

    #[test]
    fn path_search_skips_non_executables_and_empty_entries() {
        let plain = tempdir();
        std::fs::write(plain.path().join("claude"), "not executable").expect("write");
        let exec = tempdir();
        let bin = stub(exec.path(), "2.1.293", "");
        let path = std::env::join_paths([plain.path(), exec.path()]).expect("join");
        assert_eq!(pick_claude(None, Some(path)), Some(bin.clone()));
        let with_empty =
            std::env::join_paths([exec.path().as_os_str(), OsStr::new("")]).expect("join");
        assert_eq!(pick_claude(None, Some(with_empty)), Some(bin));
    }

    #[test]
    fn run_capture_returns_stdout_of_a_successful_child() {
        let dir = tempdir();
        let bin = write_stub(dir.path(), "printf 'hello\\n'");
        assert_eq!(
            run_capture(&bin, &[], Duration::from_secs(10)),
            Some("hello\n".to_string())
        );
    }

    #[test]
    fn run_capture_is_none_on_non_zero_exit() {
        let dir = tempdir();
        let bin = write_stub(dir.path(), "printf 'partial\\n'\nexit 3");
        assert_eq!(run_capture(&bin, &[], Duration::from_secs(10)), None);
    }

    #[test]
    fn run_capture_kills_a_child_that_outlives_the_timeout() {
        let dir = tempdir();
        let pid_file = dir.path().join("pid");
        let bin = write_stub(
            dir.path(),
            &format!("echo $$ > '{}'\nexec sleep 30", pid_file.display()),
        );
        let started = std::time::Instant::now();
        let out = run_capture(&bin, &[], Duration::from_secs(1));
        let elapsed = started.elapsed();
        assert_eq!(out, None);
        assert!(
            elapsed < Duration::from_secs(5),
            "returned after {elapsed:?}"
        );

        let pid = std::fs::read_to_string(&pid_file).expect("stub wrote its pid");
        let still_running = Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {}", pid.trim())])
            .stderr(Stdio::null())
            .status()
            .expect("run sh")
            .success();
        assert!(!still_running, "the timed-out child must be killed");
    }

    #[test]
    fn run_capture_gives_up_when_child_closes_stdout_but_keeps_running() {
        let dir = tempdir();
        let pid_file = dir.path().join("pid");
        let bin = write_stub(
            dir.path(),
            &format!(
                "echo $$ > '{}'\nexec 1>&-\nexec sleep 30",
                pid_file.display()
            ),
        );
        let started = std::time::Instant::now();
        let out = run_capture(&bin, &[], Duration::from_secs(1));
        let elapsed = started.elapsed();
        assert_eq!(out, None);
        assert!(
            elapsed < Duration::from_secs(3),
            "returned after {elapsed:?}"
        );

        let pid = std::fs::read_to_string(&pid_file).expect("stub wrote its pid");
        let still_running = Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {}", pid.trim())])
            .stderr(Stdio::null())
            .status()
            .expect("run sh")
            .success();
        assert!(!still_running, "the child past its deadline must be killed");
    }

    #[test]
    fn run_capture_missing_binary_is_none() {
        let dir = tempdir();
        let missing = dir.path().join("no-such-claude");
        assert_eq!(
            run_capture(&missing, &["--version"], Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn capabilities_plugin_with_probe_enables_mods() {
        let p = Probe {
            claude_bin: Some("/opt/claude".into()),
            version: Some("2.1.293 (Claude Code)".into()),
            version_parsed: Some((2, 1, 293)),
            mods_supported: true,
            channels_flag: true,
            help_lists_channels: false,
            negotiation_env: None,
        };
        let caps = capabilities(
            Some(&p),
            &opts(Mode::Plugin, Scope::User),
            1_786_000_000_000,
        );
        assert_eq!(
            caps,
            ClaudeCapabilities {
                probed_at_ms: 1_786_000_000_000,
                claude_bin: Some("/opt/claude".into()),
                claude_version: Some("2.1.293".into()),
                mods: true,
                channels_flag: true,
                async_rewake: true,
                stop_hook: true,
                context_hook: true,
                mcp_negotiation_legacy: false,
                install_mode: Some("plugin".into()),
                scope: Some("user".into()),
            }
        );
    }

    #[test]
    fn capabilities_negotiation_legacy_follows_probe_env() {
        let legacy = Probe {
            negotiation_env: Some("legacy".into()),
            ..Probe::default()
        };
        let caps = capabilities(Some(&legacy), &opts(Mode::Plugin, Scope::User), 1);
        assert!(caps.mcp_negotiation_legacy);

        let other = Probe {
            negotiation_env: Some("off".into()),
            ..Probe::default()
        };
        let caps = capabilities(Some(&other), &opts(Mode::Plugin, Scope::User), 1);
        assert!(!caps.mcp_negotiation_legacy);
    }

    #[test]
    fn capabilities_entries_mode_has_no_mods_but_keeps_channels() {
        let p = Probe {
            mods_supported: true,
            channels_flag: true,
            ..Probe::default()
        };
        let caps = capabilities(Some(&p), &opts(Mode::Entries, Scope::Project), 5);
        assert!(!caps.mods);
        assert!(caps.channels_flag);
        assert_eq!(caps.install_mode.as_deref(), Some("entries"));
        assert_eq!(caps.scope.as_deref(), Some("project"));
        assert_eq!(caps.probed_at_ms, 5);
    }

    #[test]
    fn capabilities_without_probe_is_empty_but_hooks_are_on() {
        let caps = capabilities(None, &opts(Mode::Plugin, Scope::User), 99);
        assert_eq!(caps.probed_at_ms, 0);
        assert_eq!(caps.claude_bin, None);
        assert_eq!(caps.claude_version, None);
        assert!(!caps.mods);
        assert!(!caps.channels_flag);
        assert!(caps.async_rewake && caps.stop_hook && caps.context_hook);
        assert!(!caps.mcp_negotiation_legacy);
        assert_eq!(caps.install_mode.as_deref(), Some("plugin"));
    }
}
