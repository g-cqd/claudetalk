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
            !l.contains(&ChannelPush) && !l.contains(&ModSubmit) && !l.contains(&AsyncRewakeHook),
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

#[test]
fn harness_identifiers_are_stable() {
    let all = [
        (Harness::ClaudeCode, "claude_code"),
        (Harness::Codex, "codex"),
        (Harness::CopilotCli, "copilot_cli"),
        (Harness::CopilotVsCode, "copilot_vscode"),
        (Harness::OpenCode, "opencode"),
        (Harness::Cursor, "cursor"),
        (Harness::Antigravity, "antigravity"),
        (Harness::Unknown, "unknown"),
    ];
    for (h, s) in all {
        assert_eq!(h.as_str(), s);
    }
}

#[test]
fn mechanism_identifiers_are_stable() {
    let all = [
        (ChannelPush, "channel_push"),
        (ModSubmit, "mod_submit"),
        (AsyncRewakeHook, "async_rewake_hook"),
        (OwnedSessionApi, "owned_session_api"),
        (StopContinue, "stop_continue"),
        (HookInject, "hook_inject"),
        (WaitTool, "wait_tool"),
    ];
    for (m, s) in all {
        assert_eq!(m.as_str(), s);
    }
}

#[test]
fn parse_round_trips_every_mechanism() {
    for m in Mechanism::ALL {
        assert_eq!(Mechanism::parse(m.as_str()), Some(m), "{m:?}");
    }
}

#[test]
fn parse_rejects_unknown_empty_and_case_variants() {
    for s in [
        "",
        "unknown",
        "CHANNEL_PUSH",
        "Channel_Push",
        "mod-submit",
        " mod_submit",
        "mod_submit ",
        "wait_tool\n",
        "channel_push_x",
    ] {
        assert_eq!(Mechanism::parse(s), None, "{s:?}");
    }
}

#[test]
fn is_pull_truth_table() {
    let table = [
        (ChannelPush, false),
        (ModSubmit, true),
        (AsyncRewakeHook, true),
        (OwnedSessionApi, false),
        (StopContinue, true),
        (HookInject, true),
        (WaitTool, false),
    ];
    for (m, pull) in table {
        assert_eq!(m.is_pull(), pull, "{m:?}");
    }
}

#[test]
fn all_has_seven_distinct_entries_in_enum_order() {
    assert_eq!(
        Mechanism::ALL,
        [
            ChannelPush,
            ModSubmit,
            AsyncRewakeHook,
            OwnedSessionApi,
            StopContinue,
            HookInject,
            WaitTool
        ]
    );
    assert!(Mechanism::ALL.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn real_client_names_map_to_harnesses() {
    // Names observed from the real clients (SPEC §2 table).
    let cases = [
        ("codex-mcp-client", Harness::Codex),
        ("github-copilot-developer", Harness::CopilotCli),
        ("Visual Studio Code", Harness::CopilotVsCode),
        ("Visual Studio Code - Insiders", Harness::CopilotVsCode),
        ("antigravity-client", Harness::Antigravity),
        ("opencode", Harness::OpenCode),
        ("cursor-vscode", Harness::Cursor),
    ];
    for (name, h) in cases {
        assert_eq!(Harness::from_client_name(name), h, "{name:?}");
    }
}

#[test]
fn cursor_vscode_is_cursor_not_vs_code() {
    assert_eq!(Harness::from_client_name("cursor-vscode"), Harness::Cursor);
    assert_ne!(
        Harness::from_client_name("cursor-vscode"),
        Harness::CopilotVsCode
    );
}

#[test]
fn antigravity_ladder_with_all_capabilities() {
    assert_eq!(
        ladder(Harness::Antigravity, ALL),
        [OwnedSessionApi, StopContinue, HookInject, WaitTool]
    );
}

#[test]
fn vscode_and_cursor_ladders_never_own_session() {
    for h in [Harness::CopilotVsCode, Harness::Cursor] {
        let l = ladder(h, ALL);
        assert!(!l.contains(&OwnedSessionApi), "{h:?}");
        assert_eq!(l, [StopContinue, HookInject, WaitTool], "{h:?}");
    }
}

#[test]
fn harness_parse_round_trips_every_harness() {
    for h in Harness::ALL {
        assert_eq!(Harness::parse(h.as_str()), Some(h), "{h:?}");
    }
}

#[test]
fn harness_parse_rejects_unknown_empty_and_case_variants() {
    for s in [
        "",
        "Codex",
        "CODEX",
        "claude-code",
        "copilot-cli",
        " codex",
        "codex\n",
    ] {
        assert_eq!(Harness::parse(s), None, "{s:?}");
    }
}
