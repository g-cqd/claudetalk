//! Unit tests for `paths.rs`: every client and scope path, plus environment overrides.

use super::*;
use InstructionsKind::{Block, OwnedFile};

const HOME: &str = "/home/u";
const PROJ: &str = "/work/p";

/// Resolve with `pairs` as the only set environment variables, home `/home/u`, project `/work/p`.
fn try_resolve(pairs: &[(&str, &str)]) -> Result<ClientPaths, CliError> {
    let env = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(*value))
    };
    ClientPaths::from_env(Path::new(HOME), Path::new(PROJ), &env)
}

/// `try_resolve` for an environment that must resolve.
fn resolve(pairs: &[(&str, &str)]) -> ClientPaths {
    try_resolve(pairs).expect("environment resolves")
}

/// Resolve with `AGORA_VSCODE_USER_DIR` fixed, so per-client paths do not depend on the host OS.
fn fixed() -> ClientPaths {
    resolve(&[("AGORA_VSCODE_USER_DIR", "/vs/User")])
}

fn want(
    mcp: &str,
    hooks: Option<&str>,
    instructions: Option<&str>,
    kind: InstructionsKind,
) -> Targets {
    Targets {
        mcp: PathBuf::from(mcp),
        hooks: hooks.map(PathBuf::from),
        instructions: instructions.map(PathBuf::from),
        instructions_kind: kind,
    }
}

#[test]
fn defaults_without_environment() {
    let p = resolve(&[]);
    assert_eq!(p.codex_home, PathBuf::from("/home/u/.codex"));
    assert_eq!(p.copilot_home, PathBuf::from("/home/u/.copilot"));
    assert_eq!(
        p.vscode_user_dir,
        vscode_default_user_dir(Path::new(HOME), None, cfg!(target_os = "macos"))
    );
    assert_eq!(p.gemini_dir, PathBuf::from("/home/u/.gemini"));
    assert_eq!(
        p.opencode_config_dir,
        PathBuf::from("/home/u/.config/opencode")
    );
    assert_eq!(p.cursor_dir, PathBuf::from("/home/u/.cursor"));
    assert_eq!(p.user_home, PathBuf::from(HOME));
    assert_eq!(p.project_dir, PathBuf::from(PROJ));
}

#[test]
fn codex_paths_for_both_scopes() {
    let p = fixed();
    assert_eq!(
        p.codex(Scope::User),
        want(
            "/home/u/.codex/config.toml",
            Some("/home/u/.codex/hooks.json"),
            Some("/home/u/.codex/AGENTS.md"),
            Block
        )
    );
    assert_eq!(
        p.codex(Scope::Project),
        want(
            "/work/p/.codex/config.toml",
            Some("/work/p/.codex/hooks.json"),
            Some("/work/p/AGENTS.md"),
            Block
        )
    );
}

#[test]
fn copilot_cli_paths_for_both_scopes() {
    let p = fixed();
    assert_eq!(
        p.copilot_cli(Scope::User),
        want(
            "/home/u/.copilot/mcp-config.json",
            Some("/home/u/.copilot/hooks/agora.json"),
            Some("/home/u/.copilot/copilot-instructions.md"),
            Block
        )
    );
    assert_eq!(
        p.copilot_cli(Scope::Project),
        want(
            "/work/p/.github/mcp.json",
            Some("/work/p/.github/hooks/agora.json"),
            Some("/work/p/.github/copilot-instructions.md"),
            Block
        )
    );
}

#[test]
fn copilot_vscode_paths_for_both_scopes() {
    let p = fixed();
    assert_eq!(
        p.copilot_vscode(Scope::User),
        want(
            "/vs/User/mcp.json",
            Some("/home/u/.copilot/hooks/agora.json"),
            Some("/home/u/.copilot/instructions/agora.instructions.md"),
            OwnedFile
        )
    );
    assert_eq!(
        p.copilot_vscode(Scope::Project),
        want(
            "/work/p/.vscode/mcp.json",
            Some("/work/p/.github/hooks/agora.json"),
            Some("/work/p/.github/copilot-instructions.md"),
            Block
        )
    );
}

#[test]
fn agy_paths_for_both_scopes() {
    let p = fixed();
    assert_eq!(
        p.agy(Scope::User),
        want(
            "/home/u/.gemini/config/mcp_config.json",
            Some("/home/u/.gemini/config/hooks.json"),
            Some("/home/u/.gemini/AGENTS.md"),
            Block
        )
    );
    assert_eq!(
        p.agy(Scope::Project),
        want(
            "/work/p/.agents/mcp_config.json",
            Some("/work/p/.agents/hooks.json"),
            Some("/work/p/AGENTS.md"),
            Block
        )
    );
}

#[test]
fn opencode_paths_for_both_scopes_have_no_hooks() {
    let p = fixed();
    assert_eq!(
        p.opencode(Scope::User),
        want(
            "/home/u/.config/opencode/opencode.json",
            None,
            Some("/home/u/.config/opencode/AGENTS.md"),
            Block
        )
    );
    assert_eq!(
        p.opencode(Scope::Project),
        want(
            "/work/p/opencode.json",
            None,
            Some("/work/p/AGENTS.md"),
            Block
        )
    );
}

#[test]
fn cursor_paths_for_both_scopes() {
    let p = fixed();
    assert_eq!(
        p.cursor(Scope::User),
        want(
            "/home/u/.cursor/mcp.json",
            Some("/home/u/.cursor/hooks.json"),
            None,
            Block
        )
    );
    assert_eq!(
        p.cursor(Scope::Project),
        want(
            "/work/p/.cursor/mcp.json",
            Some("/work/p/.cursor/hooks.json"),
            Some("/work/p/.cursor/rules/agora.mdc"),
            OwnedFile
        )
    );
}

#[test]
fn targets_dispatches_every_client_and_has_none_for_claude() {
    let p = fixed();
    for scope in [Scope::User, Scope::Project] {
        assert_eq!(p.targets(Client::Claude, scope), None);
        assert_eq!(p.targets(Client::Codex, scope), Some(p.codex(scope)));
        assert_eq!(
            p.targets(Client::CopilotCli, scope),
            Some(p.copilot_cli(scope))
        );
        assert_eq!(
            p.targets(Client::CopilotVsCode, scope),
            Some(p.copilot_vscode(scope))
        );
        assert_eq!(p.targets(Client::Agy, scope), Some(p.agy(scope)));
        assert_eq!(p.targets(Client::OpenCode, scope), Some(p.opencode(scope)));
        assert_eq!(p.targets(Client::Cursor, scope), Some(p.cursor(scope)));
    }
}

#[test]
fn codex_home_env_moves_only_user_scope() {
    let p = resolve(&[("CODEX_HOME", "/opt/codex")]);
    assert_eq!(p.codex_home, PathBuf::from("/opt/codex"));
    assert_eq!(
        p.codex_user_config(),
        PathBuf::from("/opt/codex/config.toml")
    );
    assert_eq!(
        p.codex(Scope::User).mcp,
        PathBuf::from("/opt/codex/config.toml")
    );
    assert_eq!(
        p.codex(Scope::User).hooks,
        Some(PathBuf::from("/opt/codex/hooks.json"))
    );
    assert_eq!(
        p.codex(Scope::Project).mcp,
        PathBuf::from("/work/p/.codex/config.toml")
    );
}

#[test]
fn copilot_home_env_wins_and_leaves_vscode_literal() {
    let p = resolve(&[("COPILOT_HOME", "/cp"), ("XDG_CONFIG_HOME", "/xdg")]);
    assert_eq!(p.copilot_home, PathBuf::from("/cp"));
    assert_eq!(
        p.copilot_cli(Scope::User).mcp,
        PathBuf::from("/cp/mcp-config.json")
    );
    assert_eq!(
        p.copilot_cli(Scope::User).hooks,
        Some(PathBuf::from("/cp/hooks/agora.json"))
    );
    assert_eq!(
        p.copilot_vscode(Scope::User).hooks,
        Some(PathBuf::from("/home/u/.copilot/hooks/agora.json"))
    );
}

#[test]
fn copilot_home_falls_back_to_xdg_then_home() {
    assert_eq!(
        resolve(&[("XDG_CONFIG_HOME", "/xdg")]).copilot_home,
        PathBuf::from("/xdg/.copilot")
    );
    assert_eq!(resolve(&[]).copilot_home, PathBuf::from("/home/u/.copilot"));
    assert_eq!(
        resolve(&[("COPILOT_HOME", ""), ("XDG_CONFIG_HOME", "/xdg")]).copilot_home,
        PathBuf::from("/xdg/.copilot")
    );
}

#[test]
fn xdg_config_home_moves_opencode() {
    assert_eq!(
        resolve(&[("XDG_CONFIG_HOME", "/xdg")]).opencode_config_dir,
        PathBuf::from("/xdg/opencode")
    );
}

#[test]
fn vscode_default_user_dir_per_platform() {
    let home = Path::new(HOME);
    assert_eq!(
        vscode_default_user_dir(home, Some(Path::new("/xdg")), true),
        PathBuf::from("/home/u/Library/Application Support/Code/User")
    );
    assert_eq!(
        vscode_default_user_dir(home, Some(Path::new("/xdg")), false),
        PathBuf::from("/xdg/Code/User")
    );
    assert_eq!(
        vscode_default_user_dir(home, None, false),
        PathBuf::from("/home/u/.config/Code/User")
    );
}

#[test]
fn agora_vscode_user_dir_override_moves_the_user_dir() {
    assert_eq!(
        resolve(&[("AGORA_VSCODE_USER_DIR", "/vs/User")]).vscode_user_dir,
        PathBuf::from("/vs/User")
    );
}

#[test]
fn empty_env_values_count_as_unset() {
    let empty = resolve(&[
        ("CODEX_HOME", ""),
        ("COPILOT_HOME", ""),
        ("XDG_CONFIG_HOME", ""),
        ("AGORA_VSCODE_USER_DIR", ""),
    ]);
    assert_eq!(empty, resolve(&[]));
}

#[test]
fn relative_env_value_is_a_usage_error_naming_the_variable() {
    for name in [
        "CODEX_HOME",
        "COPILOT_HOME",
        "XDG_CONFIG_HOME",
        "AGORA_VSCODE_USER_DIR",
    ] {
        match try_resolve(&[(name, "./x")]) {
            Err(CliError::Usage(message)) => {
                assert_eq!(message, format!("{name} must be an absolute path"));
            }
            other => panic!("{name}: expected a usage error, got {other:?}"),
        }
    }
}
