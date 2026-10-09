//! Where each client keeps its files (SPEC 4.2 and the table in 4.3). Pure: no filesystem access.

use super::claude::Scope;
use super::client::Client;
use crate::CliError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The per-client base directories, resolved once from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    /// `$CODEX_HOME` (non-empty, absolute) else `<home>/.codex`.
    pub codex_home: PathBuf,
    /// `$COPILOT_HOME` (non-empty, absolute) else `$XDG_CONFIG_HOME/.copilot` (XDG non-empty,
    /// absolute) else `<home>/.copilot`.
    pub copilot_home: PathBuf,
    /// `AGORA_VSCODE_USER_DIR` else macOS `<home>/Library/Application Support/Code/User`, other
    /// unix `$XDG_CONFIG_HOME/Code/User` or `<home>/.config/Code/User`.
    pub vscode_user_dir: PathBuf,
    /// `<home>/.gemini`.
    pub gemini_dir: PathBuf,
    /// `$XDG_CONFIG_HOME/opencode` or `<home>/.config/opencode`.
    pub opencode_config_dir: PathBuf,
    /// `<home>/.cursor`.
    pub cursor_dir: PathBuf,
    /// `$HOME`.
    pub user_home: PathBuf,
    /// The project directory (process cwd, canonicalized) for `Scope::Project`.
    pub project_dir: PathBuf,
}

/// How a client's instruction file is managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionsKind {
    /// A shared file (AGENTS.md ...): only the marker block `markers::upsert_block` writes is ours.
    Block,
    /// A whole file we own (`agora.mdc`, `agora.instructions.md`): created and deleted by us.
    OwnedFile,
}

/// The files one client uses in one scope (table in SPEC 4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Targets {
    /// The MCP config file.
    pub mcp: PathBuf,
    /// The hooks file; `None` when the client has no command hooks (opencode).
    pub hooks: Option<PathBuf>,
    /// The instructions file; `None` when there is none (cursor, user scope).
    pub instructions: Option<PathBuf>,
    pub instructions_kind: InstructionsKind,
}

impl ClientPaths {
    /// Resolve the directories above. `env` looks up one environment variable (inject a closure in
    /// tests); an unset or empty value counts as unset. A relative value is a usage error naming the
    /// variable: the client would read that relative path while agora edited the real home config.
    pub fn from_env(
        user_home: &Path,
        project_dir: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<ClientPaths, CliError> {
        let xdg: Option<PathBuf> = absolute_var(env, "XDG_CONFIG_HOME")?;
        let xdg = xdg.as_deref();
        Ok(ClientPaths {
            codex_home: absolute_var(env, "CODEX_HOME")?
                .unwrap_or_else(|| user_home.join(".codex")),
            copilot_home: absolute_var(env, "COPILOT_HOME")?
                .or_else(|| xdg.map(|xdg| xdg.join(".copilot")))
                .unwrap_or_else(|| user_home.join(".copilot")),
            vscode_user_dir: absolute_var(env, "AGORA_VSCODE_USER_DIR")?.unwrap_or_else(|| {
                vscode_default_user_dir(user_home, xdg, cfg!(target_os = "macos"))
            }),
            gemini_dir: user_home.join(".gemini"),
            opencode_config_dir: config_dir(xdg, user_home, "opencode"),
            cursor_dir: user_home.join(".cursor"),
            user_home: user_home.to_path_buf(),
            project_dir: project_dir.to_path_buf(),
        })
    }

    /// Codex: user config `<codex_home>/config.toml` (also where hook trust lives for BOTH scopes,
    /// see `codex_user_config`), `hooks.json`, `AGENTS.md` block; project: `<proj>/.codex/config.toml`,
    /// `<proj>/.codex/hooks.json`, `<proj>/AGENTS.md` block.
    pub fn codex(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.codex_user_config(),
                hooks: Some(self.codex_home.join("hooks.json")),
                instructions: Some(self.codex_home.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join(".codex/config.toml"),
                hooks: Some(self.project_dir.join(".codex/hooks.json")),
                instructions: Some(self.project_dir.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
        }
    }

    /// Copilot CLI per the table in SPEC 4.3. The project MCP file is `<proj>/.github/mcp.json`:
    /// VERIFIED-LOCAL 0.0.369 reads no project MCP file at all (its hooks come only from
    /// `<gitroot>/.github/hooks`, SPEC 2.2); `.github/mcp.json` is the documented project location
    /// from Copilot CLI 1.0.61 (DOCS-ONLY), so it is the one the table chooses. User hooks are
    /// `$COPILOT_HOME/hooks/agora.json` (DOCS-ONLY for >= 0.0.422; the file is shared with VS Code
    /// when COPILOT_HOME is unset, since both then resolve to `<home>/.copilot`).
    pub fn copilot_cli(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.copilot_home.join("mcp-config.json"),
                hooks: Some(self.copilot_home.join("hooks/agora.json")),
                instructions: Some(self.copilot_home.join("copilot-instructions.md")),
                instructions_kind: InstructionsKind::Block,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join(".github/mcp.json"),
                hooks: Some(self.project_dir.join(".github/hooks/agora.json")),
                instructions: Some(self.project_dir.join(".github/copilot-instructions.md")),
                instructions_kind: InstructionsKind::Block,
            },
        }
    }

    /// VS Code per the table in SPEC 4.3 (user hooks file under `<home>/.copilot/hooks`, a literal home).
    /// VS Code does not read `COPILOT_HOME`, so its user hooks and instructions ignore that variable.
    pub fn copilot_vscode(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.vscode_user_dir.join("mcp.json"),
                hooks: Some(self.user_home.join(".copilot/hooks/agora.json")),
                instructions: Some(
                    self.user_home
                        .join(".copilot/instructions/agora.instructions.md"),
                ),
                instructions_kind: InstructionsKind::OwnedFile,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join(".vscode/mcp.json"),
                hooks: Some(self.project_dir.join(".github/hooks/agora.json")),
                instructions: Some(self.project_dir.join(".github/copilot-instructions.md")),
                instructions_kind: InstructionsKind::Block,
            },
        }
    }

    /// Antigravity: `<gemini_dir>/config/mcp_config.json`, `<gemini_dir>/config/hooks.json`,
    /// `<gemini_dir>/AGENTS.md`; project `<proj>/.agents/mcp_config.json`, `<proj>/.agents/hooks.json`,
    /// `<proj>/AGENTS.md`.
    pub fn agy(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.gemini_dir.join("config/mcp_config.json"),
                hooks: Some(self.gemini_dir.join("config/hooks.json")),
                instructions: Some(self.gemini_dir.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join(".agents/mcp_config.json"),
                hooks: Some(self.project_dir.join(".agents/hooks.json")),
                instructions: Some(self.project_dir.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
        }
    }

    /// opencode: `<cfg>/opencode.json` (no hooks), `<cfg>/AGENTS.md`; project `<proj>/opencode.json`,
    /// `<proj>/AGENTS.md`. (`opencode.jsonc` is never edited: the client module checks for it.)
    pub fn opencode(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.opencode_config_dir.join("opencode.json"),
                hooks: None,
                instructions: Some(self.opencode_config_dir.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join("opencode.json"),
                hooks: None,
                instructions: Some(self.project_dir.join("AGENTS.md")),
                instructions_kind: InstructionsKind::Block,
            },
        }
    }

    /// Cursor: `<cursor_dir>/mcp.json`, `<cursor_dir>/hooks.json`, no user instructions; project
    /// `<proj>/.cursor/mcp.json`, `<proj>/.cursor/hooks.json`, `<proj>/.cursor/rules/agora.mdc` (owned).
    /// The user scope's `instructions_kind` is `Block` only as a placeholder: its instructions are `None`.
    pub fn cursor(&self, scope: Scope) -> Targets {
        match scope {
            Scope::User => Targets {
                mcp: self.cursor_dir.join("mcp.json"),
                hooks: Some(self.cursor_dir.join("hooks.json")),
                instructions: None,
                instructions_kind: InstructionsKind::Block,
            },
            Scope::Project => Targets {
                mcp: self.project_dir.join(".cursor/mcp.json"),
                hooks: Some(self.project_dir.join(".cursor/hooks.json")),
                instructions: Some(self.project_dir.join(".cursor/rules/agora.mdc")),
                instructions_kind: InstructionsKind::OwnedFile,
            },
        }
    }

    /// Dispatch to the per-client fn above; `None` for `Client::Claude` (it has its own paths).
    pub fn targets(&self, client: Client, scope: Scope) -> Option<Targets> {
        match client {
            Client::Claude => None,
            Client::Codex => Some(self.codex(scope)),
            Client::CopilotCli => Some(self.copilot_cli(scope)),
            Client::CopilotVsCode => Some(self.copilot_vscode(scope)),
            Client::Agy => Some(self.agy(scope)),
            Client::OpenCode => Some(self.opencode(scope)),
            Client::Cursor => Some(self.cursor(scope)),
        }
    }

    /// `<codex_home>/config.toml`: the USER Codex config, where trust entries are written whatever the scope.
    pub fn codex_user_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }
}

/// The variable's value as an absolute path. Unset or empty is `None`; a relative value is a usage
/// error naming the variable, so it is never silently replaced by the real home default.
fn absolute_var(
    env: &dyn Fn(&str) -> Option<OsString>,
    name: &str,
) -> Result<Option<PathBuf>, CliError> {
    let Some(value) = env(name).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Err(CliError::Usage(format!("{name} must be an absolute path")))
    }
}

/// `$XDG_CONFIG_HOME/<name>` when XDG is set, else `<home>/.config/<name>`.
fn config_dir(xdg: Option<&Path>, user_home: &Path, name: &str) -> PathBuf {
    match xdg {
        Some(xdg) => xdg.join(name),
        None => user_home.join(".config").join(name),
    }
}

/// VS Code's default user settings directory. `macos` is a parameter so both branches can be
/// tested on any host; `from_env` passes `cfg!(target_os = "macos")`.
fn vscode_default_user_dir(user_home: &Path, xdg: Option<&Path>, macos: bool) -> PathBuf {
    if macos {
        user_home.join("Library/Application Support/Code/User")
    } else {
        config_dir(xdg, user_home, "Code/User")
    }
}

#[cfg(test)]
mod tests;
