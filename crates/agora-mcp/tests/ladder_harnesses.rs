//! Delivery ladders of the non-Claude harnesses (SPEC M6 1.4): which rungs `ladder_for_caps`
//! reports for Codex, Copilot CLI, Copilot VS Code, Antigravity, opencode and Cursor from the
//! installed `capabilities.json` entry and the heartbeats of the identity.
//!
//! Only the public API is used. The `Unknown` harness names no client, so it stays `[WaitTool]`.

use agora_client::capabilities::{CapabilitiesFile, ClaudeCapabilities, HarnessCapabilities};
use agora_client::heartbeat::SEEN_WINDOW_MS;
use agora_deliver::{Harness, Mechanism};
use agora_mcp::clients::ladder_for_caps;
use std::collections::BTreeMap;

const NOW: i64 = 1_800_000_000_000;
const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;

/// The six harnesses that have their own `capabilities.json` entry.
const OTHERS: [Harness; 6] = [
    Harness::Codex,
    Harness::CopilotCli,
    Harness::CopilotVsCode,
    Harness::Antigravity,
    Harness::OpenCode,
    Harness::Cursor,
];

/// The harnesses that have an owned-session rung (`ladder` drops it for the VS Code and Cursor).
const OWNED_SESSION_HARNESSES: [Harness; 4] = [
    Harness::Codex,
    Harness::CopilotCli,
    Harness::OpenCode,
    Harness::Antigravity,
];

/// A capabilities file whose only entry is `entry` under `h`.
fn file_with(h: Harness, entry: HarnessCapabilities) -> CapabilitiesFile {
    let mut file = CapabilitiesFile::default();
    file.set_harness(h, Some(entry));
    file
}

/// A `HarnessCapabilities` entry with stop and context hooks installed.
fn hooks_installed() -> HarnessCapabilities {
    HarnessCapabilities {
        stop_hook: true,
        context_hook: true,
        ..Default::default()
    }
}

/// A heartbeat map holding one mechanism's latest beat.
fn one_beat(m: Mechanism, t: i64) -> BTreeMap<Mechanism, i64> {
    BTreeMap::from([(m, t)])
}

fn ladder(
    h: Harness,
    caps: Option<&CapabilitiesFile>,
    beats: &BTreeMap<Mechanism, i64>,
) -> Vec<Mechanism> {
    ladder_for_caps(h, false, caps, beats, NOW)
}

#[test]
fn no_file_is_wait_tool_for_every_other_harness() {
    let none = BTreeMap::new();
    for h in OTHERS {
        assert_eq!(ladder(h, None, &none), [Mechanism::WaitTool], "{h:?}");
    }
}

#[test]
fn installed_stop_and_context_hooks_give_both_rungs() {
    let none = BTreeMap::new();
    for h in OTHERS {
        let file = file_with(h, hooks_installed());
        assert_eq!(
            ladder(h, Some(&file), &none),
            [
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool
            ],
            "{h:?}"
        );
    }
}

#[test]
fn untrusted_codex_hooks_do_not_count_but_trusted_ones_do() {
    let none = BTreeMap::new();
    let untrusted = HarnessCapabilities {
        hooks_trusted: Some(false),
        ..hooks_installed()
    };
    let file = file_with(Harness::Codex, untrusted);
    assert_eq!(
        ladder(Harness::Codex, Some(&file), &none),
        [Mechanism::WaitTool]
    );

    let trusted = HarnessCapabilities {
        hooks_trusted: Some(true),
        ..hooks_installed()
    };
    let file = file_with(Harness::Codex, trusted);
    assert_eq!(
        ladder(Harness::Codex, Some(&file), &none),
        [
            Mechanism::StopContinue,
            Mechanism::HookInject,
            Mechanism::WaitTool
        ]
    );
}

#[test]
fn untrusted_codex_hooks_do_not_block_fresh_beats_from_restoring_their_rungs() {
    let untrusted = HarnessCapabilities {
        hooks_trusted: Some(false),
        ..hooks_installed()
    };
    let file = file_with(Harness::Codex, untrusted);
    let beats = BTreeMap::from([
        (Mechanism::StopContinue, NOW - SECOND_MS),
        (Mechanism::HookInject, NOW - SECOND_MS),
    ]);
    assert_eq!(
        ladder(Harness::Codex, Some(&file), &beats),
        [
            Mechanism::StopContinue,
            Mechanism::HookInject,
            Mechanism::WaitTool
        ]
    );
}

#[test]
fn hook_inject_seen_three_days_ago_counts_without_the_file() {
    let seen = one_beat(Mechanism::HookInject, NOW - 3 * DAY_MS);
    for h in OTHERS {
        assert_eq!(
            ladder(h, None, &seen),
            [Mechanism::HookInject, Mechanism::WaitTool],
            "{h:?}"
        );
    }
    let too_old = one_beat(Mechanism::HookInject, NOW - SEEN_WINDOW_MS - 1);
    assert_eq!(
        ladder(Harness::Codex, None, &too_old),
        [Mechanism::WaitTool]
    );
}

#[test]
fn a_bridge_beat_ten_seconds_ago_puts_owned_session_first_where_supported() {
    let bridge = one_beat(Mechanism::OwnedSessionApi, NOW - 10 * SECOND_MS);
    for h in OWNED_SESSION_HARNESSES {
        let file = file_with(h, hooks_installed());
        assert_eq!(
            ladder(h, Some(&file), &bridge),
            [
                Mechanism::OwnedSessionApi,
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool,
            ],
            "{h:?}"
        );
    }
    for h in [Harness::CopilotVsCode, Harness::Cursor] {
        let file = file_with(h, hooks_installed());
        assert_eq!(
            ladder(h, Some(&file), &bridge),
            [
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool
            ],
            "{h:?}"
        );
    }
}

#[test]
fn a_bridge_beat_five_minutes_ago_is_stale() {
    let stale = one_beat(Mechanism::OwnedSessionApi, NOW - 5 * MINUTE_MS);
    for h in OWNED_SESSION_HARNESSES {
        let file = file_with(h, hooks_installed());
        assert_eq!(
            ladder(h, Some(&file), &stale),
            [
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool
            ],
            "{h:?}"
        );
    }
}

#[test]
fn the_install_flag_alone_is_not_a_live_bridge() {
    let none = BTreeMap::new();
    let entry = HarnessCapabilities {
        owned_session: true,
        ..Default::default()
    };
    for h in OWNED_SESSION_HARNESSES {
        let file = file_with(h, entry.clone());
        assert_eq!(
            ladder(h, Some(&file), &none),
            [Mechanism::WaitTool],
            "{h:?}"
        );
    }
}

#[test]
fn unknown_harness_is_wait_tool_whatever_the_caps_and_beats() {
    let file = CapabilitiesFile {
        claude: Some(ClaudeCapabilities {
            mods: true,
            async_rewake: true,
            stop_hook: true,
            context_hook: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let every_beat = BTreeMap::from([
        (Mechanism::HookInject, NOW),
        (Mechanism::StopContinue, NOW),
        (Mechanism::OwnedSessionApi, NOW),
    ]);
    assert_eq!(
        ladder_for_caps(Harness::Unknown, true, Some(&file), &every_beat, NOW),
        [Mechanism::WaitTool]
    );
}
