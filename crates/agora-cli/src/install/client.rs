//! The agent clients `agora install` knows (SPEC 4.2).

use crate::CliError;
use agora_deliver::Harness;

/// An agent client. CLI names: `claude codex copilot-cli copilot-vscode agy opencode cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Client {
    Claude,
    Codex,
    CopilotCli,
    CopilotVsCode,
    Agy,
    OpenCode,
    Cursor,
}

impl Client {
    /// Every client, in the order `--client all` runs them.
    pub const ALL: [Client; 7] = [
        Self::Claude,
        Self::Codex,
        Self::CopilotCli,
        Self::CopilotVsCode,
        Self::Agy,
        Self::OpenCode,
        Self::Cursor,
    ];

    /// The CLI name: `claude`, `codex`, `copilot-cli`, `copilot-vscode`, `agy`, `opencode`, `cursor`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::CopilotCli => "copilot-cli",
            Self::CopilotVsCode => "copilot-vscode",
            Self::Agy => "agy",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
        }
    }

    /// The delivery harness: Claude -> ClaudeCode, Codex -> Codex, CopilotCli -> CopilotCli,
    /// CopilotVsCode -> CopilotVsCode, Agy -> Antigravity, OpenCode -> OpenCode, Cursor -> Cursor.
    /// This is the key of its entry in `capabilities.json` (`Harness::as_str()`).
    pub fn harness(self) -> Harness {
        match self {
            Self::Claude => Harness::ClaudeCode,
            Self::Codex => Harness::Codex,
            Self::CopilotCli => Harness::CopilotCli,
            Self::CopilotVsCode => Harness::CopilotVsCode,
            Self::Agy => Harness::Antigravity,
            Self::OpenCode => Harness::OpenCode,
            Self::Cursor => Harness::Cursor,
        }
    }

    /// The `agora hook --dialect` value its hooks use: codex -> `codex`, copilot-cli and
    /// copilot-vscode -> `copilot` (VS Code shares the Copilot hooks file), agy -> `agy`,
    /// cursor -> `cursor`; `None` for claude and opencode (no dialect hooks).
    #[cfg(test)]
    pub fn dialect(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex"),
            Self::CopilotCli | Self::CopilotVsCode => Some("copilot"),
            Self::Agy => Some("agy"),
            Self::Cursor => Some("cursor"),
            Self::Claude | Self::OpenCode => None,
        }
    }
}

/// Parse a `--client` value: trim each comma-separated item, `all` expands to `Client::ALL`,
/// duplicates are dropped keeping the first occurrence's order, an empty item or unknown name ->
/// `CliError::Usage` listing the valid names. The result is never empty.
pub fn parse_clients(s: &str) -> Result<Vec<Client>, CliError> {
    let mut clients: Vec<Client> = Vec::new();
    let mut all = false;
    for raw in s.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if item == "all" {
            all = true;
            continue;
        }
        let client = Client::ALL
            .into_iter()
            .find(|c| c.as_str() == item)
            .ok_or_else(|| {
                CliError::Usage(format!("unknown client '{item}'; valid: {}", valid_names()))
            })?;
        if !clients.contains(&client) {
            clients.push(client);
        }
    }
    if all {
        return Ok(Client::ALL.to_vec());
    }
    if clients.is_empty() {
        return Err(CliError::Usage(format!(
            "no client given; valid: {}",
            valid_names()
        )));
    }
    Ok(clients)
}

/// The accepted `--client` names, comma separated, for usage errors.
fn valid_names() -> String {
    Client::ALL
        .iter()
        .map(|c| c.as_str())
        .chain(std::iter::once("all"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_message(s: &str) -> String {
        match parse_clients(s).unwrap_err() {
            CliError::Usage(m) => m,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn single_name_parses() {
        assert_eq!(parse_clients("codex").unwrap(), vec![Client::Codex]);
    }

    #[test]
    fn comma_list_keeps_order() {
        assert_eq!(
            parse_clients("cursor,codex,agy").unwrap(),
            vec![Client::Cursor, Client::Codex, Client::Agy]
        );
    }

    #[test]
    fn surrounding_spaces_and_empty_items_are_ignored() {
        assert_eq!(
            parse_clients("  codex ,, copilot-cli ,").unwrap(),
            vec![Client::Codex, Client::CopilotCli]
        );
    }

    #[test]
    fn duplicates_are_dropped_keeping_first_position() {
        assert_eq!(
            parse_clients("codex,claude,codex, claude").unwrap(),
            vec![Client::Codex, Client::Claude]
        );
    }

    #[test]
    fn all_expands_to_every_client_in_order() {
        assert_eq!(parse_clients("all").unwrap(), Client::ALL.to_vec());
    }

    #[test]
    fn all_mixed_with_names_expands_to_every_client() {
        assert_eq!(
            parse_clients("codex, all ,cursor").unwrap(),
            Client::ALL.to_vec()
        );
    }

    #[test]
    fn unknown_name_is_a_usage_error_listing_valid_names() {
        assert_eq!(
            usage_message("codex,nope"),
            "unknown client 'nope'; valid: claude, codex, copilot-cli, copilot-vscode, agy, \
             opencode, cursor, all"
        );
    }

    #[test]
    fn names_are_case_sensitive() {
        assert!(usage_message("Codex").starts_with("unknown client 'Codex'"));
    }

    #[test]
    fn empty_or_blank_input_is_a_usage_error() {
        for s in ["", " , ,"] {
            assert!(usage_message(s).starts_with("no client given"), "{s:?}");
        }
    }

    #[test]
    fn as_str_round_trips_through_parse() {
        for c in Client::ALL {
            assert_eq!(parse_clients(c.as_str()).unwrap(), vec![c]);
        }
    }

    #[test]
    fn harness_mapping() {
        assert_eq!(Client::Claude.harness(), Harness::ClaudeCode);
        assert_eq!(Client::Codex.harness(), Harness::Codex);
        assert_eq!(Client::CopilotCli.harness(), Harness::CopilotCli);
        assert_eq!(Client::CopilotVsCode.harness(), Harness::CopilotVsCode);
        assert_eq!(Client::Agy.harness(), Harness::Antigravity);
        assert_eq!(Client::OpenCode.harness(), Harness::OpenCode);
        assert_eq!(Client::Cursor.harness(), Harness::Cursor);
    }

    #[test]
    fn dialect_mapping() {
        assert_eq!(Client::Codex.dialect(), Some("codex"));
        assert_eq!(Client::CopilotCli.dialect(), Some("copilot"));
        assert_eq!(Client::CopilotVsCode.dialect(), Some("copilot"));
        assert_eq!(Client::Agy.dialect(), Some("agy"));
        assert_eq!(Client::Cursor.dialect(), Some("cursor"));
        assert_eq!(Client::Claude.dialect(), None);
        assert_eq!(Client::OpenCode.dialect(), None);
    }
}
