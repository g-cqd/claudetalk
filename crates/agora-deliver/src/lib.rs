//! Delivery ladder: which mechanisms to use per harness, and when to escalate.
//!
//! Pure logic, no I/O. Every rung is only a trigger to drain the inbox, so
//! rungs may all be armed at once; the inbox ack makes delivery idempotent.
//! A message that stays unacked past a rung's patience escalates to the next
//! rung. The last rung (`WaitTool`) works on any MCP client and is the floor.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
    CopilotCli,
    CopilotVsCode,
    OpenCode,
    Cursor,
    /// Google Antigravity CLI (`agy`), successor to Gemini CLI.
    Antigravity,
    Unknown,
}

impl Harness {
    /// Best-effort identification from MCP `clientInfo.name`.
    pub fn from_client_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.contains("claude") {
            Self::ClaudeCode
        } else if n.contains("codex") {
            Self::Codex
        } else if n.contains("visual studio code") || n.contains("vscode") || n.contains("vs code")
        {
            Self::CopilotVsCode
        } else if n.contains("copilot") {
            Self::CopilotCli
        } else if n.contains("opencode") {
            Self::OpenCode
        } else if n.contains("cursor") {
            Self::Cursor
        } else if n.contains("antigravity") || n == "agy" || n.contains("gemini") {
            Self::Antigravity
        } else {
            Self::Unknown
        }
    }
}

/// A way to get the agent to look at its inbox, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mechanism {
    /// Claude Code `notifications/claude/channel` (wakes idle).
    ChannelPush,
    /// Claude Code mod `$.prompt.submit` (wakes idle).
    ModSubmit,
    /// Claude Code `asyncRewake` hook, exit 2 (wakes idle).
    AsyncRewakeHook,
    /// Daemon owns the agent process: Codex app-server, Copilot ACP, opencode HTTP.
    OwnedSessionApi,
    /// Stop-style hook that blocks/continues the turn while the inbox is non-empty.
    StopContinue,
    /// Hook that injects inbox text as context on the next prompt/tool boundary.
    HookInject,
    /// `inbox_wait` long-poll tool plus server instructions. Works everywhere.
    WaitTool,
}

impl Mechanism {
    /// Can this rung start work in a session that is idle right now?
    pub fn wakes_idle(self) -> bool {
        matches!(
            self,
            Self::ChannelPush | Self::ModSubmit | Self::AsyncRewakeHook | Self::OwnedSessionApi
        )
    }
}

/// What the install/probe step found available for one agent session.
#[derive(Debug, Default, Clone, Copy)]
pub struct Capabilities {
    pub channels_loaded: bool,
    pub mods: bool,
    pub async_rewake: bool,
    pub owned_session: bool,
    pub stop_hook: bool,
    pub context_hook: bool,
}

/// Ordered rungs for a harness given the capabilities actually present.
/// `WaitTool` is always last and always present.
pub fn ladder(h: Harness, c: Capabilities) -> Vec<Mechanism> {
    use Mechanism::*;
    let candidates: &[(Mechanism, bool)] = match h {
        Harness::ClaudeCode => &[
            (ChannelPush, c.channels_loaded),
            (ModSubmit, c.mods),
            (AsyncRewakeHook, c.async_rewake),
            (StopContinue, c.stop_hook),
            (HookInject, c.context_hook),
        ],
        Harness::Codex | Harness::CopilotCli | Harness::OpenCode => &[
            (OwnedSessionApi, c.owned_session),
            (StopContinue, c.stop_hook),
            (HookInject, c.context_hook),
        ],
        Harness::CopilotVsCode | Harness::Cursor | Harness::Antigravity => {
            &[(StopContinue, c.stop_hook), (HookInject, c.context_hook)]
        }
        Harness::Unknown => &[],
    };
    let mut out: Vec<Mechanism> = candidates
        .iter()
        .filter(|(_, on)| *on)
        .map(|(m, _)| *m)
        .collect();
    out.push(WaitTool);
    out
}

/// Per-message delivery state tracked by the daemon.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attempt {
    /// Number of rungs already tried (0 = nothing sent yet).
    pub rungs_tried: usize,
    /// Time of the last attempt, in ms.
    pub last_ms: i64,
}

/// How long to wait for an ack before escalating, per rung (idle-waking rungs
/// get a shorter patience: if they worked, the ack comes quickly).
pub fn patience_ms(m: Mechanism) -> i64 {
    if m.wakes_idle() { 15_000 } else { 60_000 }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Try this rung now.
    Fire(Mechanism),
    /// Wait; nothing to do until `at_ms`.
    Hold { at_ms: i64 },
    /// Every rung has been tried; leave it to the wait tool / next hook.
    Exhausted,
}

/// Decide the next step for an unacked message. Rung 0 fires immediately.
/// The final rung (`WaitTool`) is passive, so exhausting the ladder is a normal state.
pub fn next_action(ladder: &[Mechanism], a: Attempt, now_ms: i64) -> Action {
    match ladder.get(a.rungs_tried) {
        None => Action::Exhausted,
        Some(&m) if a.rungs_tried == 0 => Action::Fire(m),
        Some(&m) => {
            let prev = ladder[a.rungs_tried - 1];
            let due = a.last_ms + patience_ms(prev);
            if now_ms >= due {
                Action::Fire(m)
            } else {
                Action::Hold { at_ms: due }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Mechanism::*;

    const ALL: Capabilities = Capabilities {
        channels_loaded: true,
        mods: true,
        async_rewake: true,
        owned_session: true,
        stop_hook: true,
        context_hook: true,
    };

    #[test]
    fn claude_uses_every_rung_strongest_first() {
        assert_eq!(
            ladder(Harness::ClaudeCode, ALL),
            [
                ChannelPush,
                ModSubmit,
                AsyncRewakeHook,
                StopContinue,
                HookInject,
                WaitTool
            ]
        );
    }

    #[test]
    fn missing_capabilities_degrade_to_next_best() {
        let c = Capabilities {
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        };
        // Channels not enabled, no mods: falls straight to hooks.
        assert_eq!(
            ladder(Harness::ClaudeCode, c),
            [StopContinue, HookInject, WaitTool]
        );
        // Nothing at all still leaves the guaranteed floor.
        assert_eq!(
            ladder(Harness::ClaudeCode, Capabilities::default()),
            [WaitTool]
        );
        assert_eq!(ladder(Harness::Unknown, ALL), [WaitTool]);
    }

    #[test]
    fn other_harnesses_never_get_claude_only_rungs() {
        for h in [
            Harness::Codex,
            Harness::CopilotCli,
            Harness::CopilotVsCode,
            Harness::Cursor,
            Harness::Antigravity,
            Harness::OpenCode,
        ] {
            let l = ladder(h, ALL);
            assert!(
                !l.contains(&ChannelPush)
                    && !l.contains(&ModSubmit)
                    && !l.contains(&AsyncRewakeHook),
                "{h:?}"
            );
            assert_eq!(*l.last().unwrap(), WaitTool);
        }
        assert_eq!(ladder(Harness::Codex, ALL)[0], OwnedSessionApi);
        assert_eq!(ladder(Harness::CopilotVsCode, ALL)[0], StopContinue);
    }

    #[test]
    fn waittool_is_last_and_passive() {
        for h in [Harness::ClaudeCode, Harness::Codex, Harness::Unknown] {
            assert_eq!(ladder(h, ALL).last(), Some(&WaitTool));
        }
        assert!(!WaitTool.wakes_idle());
        assert!(ChannelPush.wakes_idle() && !HookInject.wakes_idle());
    }

    #[test]
    fn escalation_waits_for_patience_then_advances() {
        let l = ladder(Harness::ClaudeCode, ALL);
        let mut a = Attempt::default();
        assert_eq!(next_action(&l, a, 0), Action::Fire(ChannelPush));
        a = Attempt {
            rungs_tried: 1,
            last_ms: 0,
        };
        assert_eq!(next_action(&l, a, 5_000), Action::Hold { at_ms: 15_000 });
        assert_eq!(next_action(&l, a, 15_000), Action::Fire(ModSubmit));
        // Walk the whole ladder; non-waking rungs wait longer.
        let a = Attempt {
            rungs_tried: 4,
            last_ms: 100_000,
        };
        assert_eq!(next_action(&l, a, 100_001), Action::Hold { at_ms: 160_000 });
        assert_eq!(next_action(&l, a, 160_000), Action::Fire(HookInject));
        let done = Attempt {
            rungs_tried: l.len(),
            last_ms: 0,
        };
        assert_eq!(next_action(&l, done, 10), Action::Exhausted);
    }

    #[test]
    fn client_names_map_to_harnesses() {
        assert_eq!(
            Harness::from_client_name("claude-code"),
            Harness::ClaudeCode
        );
        assert_eq!(
            Harness::from_client_name("codex-mcp-client"),
            Harness::Codex
        );
        assert_eq!(
            Harness::from_client_name("Visual Studio Code"),
            Harness::CopilotVsCode
        );
        assert_eq!(
            Harness::from_client_name("github-copilot-cli"),
            Harness::CopilotCli
        );
        assert_eq!(
            Harness::from_client_name("antigravity-cli"),
            Harness::Antigravity
        );
        assert_eq!(Harness::from_client_name("agy"), Harness::Antigravity);
        assert_eq!(
            Harness::from_client_name("antigravity-client"),
            Harness::Antigravity
        );
        // Legacy Gemini CLI clients are treated as Antigravity.
        assert_eq!(
            Harness::from_client_name("gemini-cli-mcp-client"),
            Harness::Antigravity
        );
        assert_eq!(Harness::from_client_name("mystery"), Harness::Unknown);
    }
}
