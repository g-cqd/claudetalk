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
    /// Every harness, in enum order (`Unknown` last).
    pub const ALL: [Harness; 8] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::CopilotCli,
        Self::CopilotVsCode,
        Self::OpenCode,
        Self::Cursor,
        Self::Antigravity,
        Self::Unknown,
    ];

    /// Inverse of `as_str`: `Some(h)` iff `h.as_str() == s` exactly (case-sensitive), else `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.as_str() == s)
    }

    /// Best-effort identification from MCP `clientInfo.name`.
    pub fn from_client_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.contains("claude") {
            Self::ClaudeCode
        } else if n.contains("codex") {
            Self::Codex
        } else if n.contains("cursor") {
            // Cursor sends `cursor-vscode`, so it must be checked before the VS Code names.
            Self::Cursor
        } else if n.contains("visual studio code") || n.contains("vscode") || n.contains("vs code")
        {
            Self::CopilotVsCode
        } else if n.contains("copilot") {
            Self::CopilotCli
        } else if n.contains("opencode") {
            Self::OpenCode
        } else if n.contains("antigravity") || n == "agy" || n.contains("gemini") {
            Self::Antigravity
        } else {
            Self::Unknown
        }
    }

    /// Stable lowercase identifier used in logs and tool output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::CopilotCli => "copilot_cli",
            Self::CopilotVsCode => "copilot_vscode",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
            Self::Antigravity => "antigravity",
            Self::Unknown => "unknown",
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
    /// Daemon owns the agent process: Codex app-server, Copilot ACP, opencode HTTP, agy -p --conversation.
    OwnedSessionApi,
    /// Stop-style hook that blocks/continues the turn while the inbox is non-empty.
    StopContinue,
    /// Hook that injects inbox text as context on the next prompt/tool boundary.
    HookInject,
    /// `inbox_wait` long-poll tool plus server instructions. Works everywhere.
    WaitTool,
}

impl Mechanism {
    /// Every mechanism, in enum (strongest-first) order.
    pub const ALL: [Mechanism; 7] = [
        Self::ChannelPush,
        Self::ModSubmit,
        Self::AsyncRewakeHook,
        Self::OwnedSessionApi,
        Self::StopContinue,
        Self::HookInject,
        Self::WaitTool,
    ];

    /// Inverse of `as_str`: `Some(m)` iff `m.as_str() == s` exactly (case-sensitive), else `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Pull rungs prove liveness by heartbeat files written by the plugin's hooks and mod:
    /// `ModSubmit`, `AsyncRewakeHook`, `StopContinue`, `HookInject`. All other rungs are not pull.
    pub fn is_pull(self) -> bool {
        matches!(
            self,
            Self::ModSubmit | Self::AsyncRewakeHook | Self::StopContinue | Self::HookInject
        )
    }

    /// Can this rung start work in a session that is idle right now?
    pub fn wakes_idle(self) -> bool {
        matches!(
            self,
            Self::ChannelPush | Self::ModSubmit | Self::AsyncRewakeHook | Self::OwnedSessionApi
        )
    }

    /// Stable lowercase identifier used in logs and tool output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChannelPush => "channel_push",
            Self::ModSubmit => "mod_submit",
            Self::AsyncRewakeHook => "async_rewake_hook",
            Self::OwnedSessionApi => "owned_session_api",
            Self::StopContinue => "stop_continue",
            Self::HookInject => "hook_inject",
            Self::WaitTool => "wait_tool",
        }
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
        Harness::Codex | Harness::CopilotCli | Harness::OpenCode | Harness::Antigravity => &[
            (OwnedSessionApi, c.owned_session),
            (StopContinue, c.stop_hook),
            (HookInject, c.context_hook),
        ],
        Harness::CopilotVsCode | Harness::Cursor => {
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
mod tests;
