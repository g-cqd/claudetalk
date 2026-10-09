//! Who is calling: client records and the harness mapping.
//!
//! The FIRST request of every client is logged once at debug (method, protocol version,
//! clientInfo name/version, never arguments). stdio: one client per process. HTTP: keyed
//! by (token id, clientInfo name) in a bounded registry; a legacy HTTP request without
//! clientInfo uses the last record of its token.

use crate::engine::Transport;
use crate::version::{ClientInfo, Era, truncate_at_char_boundary};
use agora_client::capabilities::CapabilitiesFile;
use agora_client::heartbeat;
use agora_deliver::{Capabilities, Harness, Mechanism, ladder};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

/// Registry capacity; the oldest record is evicted beyond it.
pub const MAX_RECORDS: usize = 256;
/// Records kept per token; that token's oldest is evicted beyond it.
pub const MAX_RECORDS_PER_TOKEN: usize = 16;
/// `ClientRecord::first_method` is kept to this many bytes.
const MAX_METHOD_BYTES: usize = 128;
/// The method written to the debug log is kept to this many bytes.
const MAX_LOG_METHOD_BYTES: usize = 64;
/// Capabilities whose JSON form is longer than this are stored as `{}`.
const MAX_CAPS_BYTES: usize = 4096;

/// Token key used for stdio sessions (which have no bearer token).
const STDIO_KEY: &str = "stdio";

/// One observed client.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRecord {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    /// "modern" | "legacy"
    pub era: &'static str,
    /// "stdio" | "http"
    pub transport: &'static str,
    pub first_method: String,
    pub first_seen_ms: i64,
    /// `Harness::as_str()` of `harness_of(name)`.
    pub harness: &'static str,
    pub capabilities: Value,
}

/// (token key, clientInfo name).
type Key = (String, String);

#[derive(Default)]
struct Inner {
    /// Insertion order = first-seen order; the front is the oldest.
    records: VecDeque<(Key, ClientRecord)>,
    /// Per token key: the key of the most recently observed record.
    last: HashMap<String, Key>,
}

/// Bounded registry of observed clients.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock only means another request panicked mid-update; the data is still
        // a valid registry, so keep serving it.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record a client: returns the existing record when (`token`, `info.name`) was seen, else
    /// inserts a new one (first_method = `method`, first_seen_ms = `now_ms`, era / transport
    /// as given) and logs it once at debug. Either way the token's last pointer moves to this
    /// client. A known client takes the fast path: nothing is built, logged or evicted.
    /// Limits: `first_method` 128 bytes; capabilities over 4 KiB are stored as `{}`; the token
    /// keeps at most `MAX_RECORDS_PER_TOKEN` (its oldest is evicted); the registry at most
    /// `MAX_RECORDS` (the oldest overall is evicted).
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &self,
        token: Option<&str>,
        info: &ClientInfo,
        protocol_version: &str,
        era: Era,
        transport: Transport,
        method: &str,
        caps: &Value,
        now_ms: i64,
    ) -> ClientRecord {
        let token_key = token.unwrap_or(STDIO_KEY).to_string();
        let key: Key = (token_key.clone(), info.name.clone());
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.last.insert(token_key.clone(), key.clone());
        if let Some((_, existing)) = inner.records.iter().find(|(k, _)| *k == key) {
            return existing.clone();
        }
        let era_str = match era {
            Era::Modern => "modern",
            Era::Legacy => "legacy",
        };
        let transport_str = match transport {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        };
        let capabilities = if serde_json::to_string(caps).is_ok_and(|s| s.len() <= MAX_CAPS_BYTES) {
            caps.clone()
        } else {
            json!({})
        };
        let first_method = truncate_at_char_boundary(method, MAX_METHOD_BYTES);
        let logged_method = truncate_at_char_boundary(method, MAX_LOG_METHOD_BYTES);
        let record = ClientRecord {
            name: info.name.clone(),
            version: info.version.clone(),
            protocol_version: protocol_version.to_string(),
            era: era_str,
            transport: transport_str,
            first_method,
            first_seen_ms: now_ms,
            harness: harness_of(&info.name).as_str(),
            capabilities,
        };
        // Every field here came off the wire: Debug formatting escapes control characters.
        tracing::debug!(
            client = ?info.name,
            version = ?info.version,
            protocol = ?protocol_version,
            era = era_str,
            transport = transport_str,
            method = ?logged_method,
            "first request from client"
        );
        let token_records = inner
            .records
            .iter()
            .filter(|(k, _)| k.0 == token_key)
            .count();
        if token_records >= MAX_RECORDS_PER_TOKEN
            && let Some(oldest) = inner.records.iter().position(|(k, _)| k.0 == token_key)
        {
            inner.records.remove(oldest);
        }
        inner.records.push_back((key, record.clone()));
        if inner.records.len() > MAX_RECORDS
            && let Some((evicted, _)) = inner.records.pop_front()
            && inner.last.get(&evicted.0) == Some(&evicted)
        {
            inner.last.remove(&evicted.0);
        }
        record
    }

    /// The most recently observed record of `token`.
    pub fn last_for(&self, token: Option<&str>) -> Option<ClientRecord> {
        let inner = self.lock();
        let key = inner.last.get(token.unwrap_or(STDIO_KEY))?;
        inner
            .records
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, record)| record.clone())
    }

    /// Every record, oldest first.
    pub fn all(&self) -> Vec<ClientRecord> {
        let mut out: Vec<ClientRecord> = self
            .lock()
            .records
            .iter()
            .map(|(_, record)| record.clone())
            .collect();
        out.sort_by_key(|r| r.first_seen_ms);
        out
    }
}

/// `Harness::from_client_name(name)`.
pub fn harness_of(name: &str) -> Harness {
    Harness::from_client_name(name)
}

/// `agora_deliver::ladder(h, caps)` with `caps.channels_loaded = push_enabled` and every other
/// capability false (M3).
pub fn ladder_for(h: Harness, push_enabled: bool) -> Vec<Mechanism> {
    ladder(
        h,
        Capabilities {
            channels_loaded: push_enabled,
            ..Default::default()
        },
    )
}

/// The ladder for `h` given the install-time `caps` (`capabilities.json`, `None` when the file is
/// absent) and the heartbeats `beats` of this identity at `now_ms`. `beats` must be the map of
/// harness `h` alone, `heartbeat::read_all_for(paths, pk, h)`: beats are stored per harness, so
/// no other harness's beat can reach this ladder through that read.
///
/// `caps` is the whole file. Inclusion rule: a rung is in the ladder when it is installed per
/// `caps` OR its heartbeat was seen within 7 days (`agora_client::heartbeat::seen`).
/// - `ClaudeCode` reads `caps.claude`: `mods`, `async_rewake`, `stop_hook` and `context_hook` are
///   `caps.<flag> || seen(<rung>)`, `channels_loaded` is `push_enabled`, `owned_session` is false.
/// - Every other known harness reads `caps.harness(h)` (SPEC 1.4). Its `stop_hook` and
///   `context_hook` count when installed and not `hooks_trusted == Some(false)` (Codex writes
///   hooks that do not run until they are trusted), or when seen. `owned_session` is true only
///   while a bridge is alive (`heartbeat::armed` on `OwnedSessionApi`, a 90 s window), never from
///   the install flag. `channels_loaded`, `mods` and `async_rewake` are false.
///   `ladder` keeps `OwnedSessionApi` only for Codex, Copilot CLI, opencode and Antigravity.
/// - `Unknown` names no client, so its ladder is `[WaitTool]`.
///
/// Arming is not decided here. Whether a pull rung is armed right now (its shorter heartbeat
/// window) is decided per tick by the delivery tracker, from `heartbeat::armed`.
pub fn ladder_for_caps(
    h: Harness,
    push_enabled: bool,
    caps: Option<&CapabilitiesFile>,
    beats: &BTreeMap<Mechanism, i64>,
    now_ms: i64,
) -> Vec<Mechanism> {
    let seen = heartbeat::seen(beats, now_ms);
    if h == Harness::ClaudeCode {
        let caps = caps.and_then(|f| f.claude.as_ref());
        return ladder(
            h,
            Capabilities {
                channels_loaded: push_enabled,
                mods: caps.is_some_and(|c| c.mods) || seen.contains(&Mechanism::ModSubmit),
                async_rewake: caps.is_some_and(|c| c.async_rewake)
                    || seen.contains(&Mechanism::AsyncRewakeHook),
                stop_hook: caps.is_some_and(|c| c.stop_hook)
                    || seen.contains(&Mechanism::StopContinue),
                context_hook: caps.is_some_and(|c| c.context_hook)
                    || seen.contains(&Mechanism::HookInject),
                owned_session: false,
            },
        );
    }
    let (stop_installed, context_installed) = match caps.and_then(|f| f.harness(h)) {
        Some(c) if c.hooks_trusted != Some(false) => (c.stop_hook, c.context_hook),
        _ => (false, false),
    };
    let armed = heartbeat::armed(beats, now_ms);
    ladder(
        h,
        Capabilities {
            channels_loaded: false,
            mods: false,
            async_rewake: false,
            owned_session: armed.contains(&Mechanism::OwnedSessionApi),
            stop_hook: stop_installed || seen.contains(&Mechanism::StopContinue),
            context_hook: context_installed || seen.contains(&Mechanism::HookInject),
        },
    )
}

/// The ladder as a JSON array of `Mechanism::as_str()` strings.
pub fn ladder_json(l: &[Mechanism]) -> Value {
    Value::Array(l.iter().map(|m| Value::from(m.as_str())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_client::capabilities::ClaudeCapabilities;
    use serde_json::json;

    fn info(name: &str, version: &str) -> ClientInfo {
        ClientInfo {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    fn observe_as(
        reg: &Registry,
        token: Option<&str>,
        name: &str,
        method: &str,
        now_ms: i64,
    ) -> ClientRecord {
        reg.observe(
            token,
            &info(name, "1.0"),
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            method,
            &json!({}),
            now_ms,
        )
    }

    #[test]
    fn first_observe_wins_and_repeat_observe_returns_the_existing_record() {
        let reg = Registry::new();
        let first = reg.observe(
            Some("t_a"),
            &info("claude-code", "2.1"),
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "initialize",
            &json!({"roots": {}}),
            100,
        );
        // Same key, different everything else: the record must not change.
        let second = reg.observe(
            Some("t_a"),
            &info("claude-code", "9.9"),
            "2026-07-28",
            Era::Modern,
            Transport::Stdio,
            "tools/list",
            &json!({}),
            200,
        );
        assert_eq!(second.version, "2.1");
        assert_eq!(second.protocol_version, "2025-11-25");
        assert_eq!(second.era, "legacy");
        assert_eq!(second.transport, "http");
        assert_eq!(second.first_method, "initialize");
        assert_eq!(second.first_seen_ms, 100);
        assert_eq!(second.capabilities, first.capabilities);
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn last_for_tracks_the_most_recently_observed_record_per_token() {
        let reg = Registry::new();
        assert!(reg.last_for(Some("t_a")).is_none());
        observe_as(&reg, Some("t_a"), "codex", "initialize", 1);
        observe_as(&reg, Some("t_a"), "claude-code", "initialize", 2);
        observe_as(&reg, Some("t_b"), "cursor", "initialize", 3);
        assert_eq!(
            reg.last_for(Some("t_a")).map(|r| r.name).as_deref(),
            Some("claude-code")
        );
        // A repeat observe of a known key moves the per-token pointer too.
        observe_as(&reg, Some("t_a"), "codex", "tools/list", 4);
        assert_eq!(
            reg.last_for(Some("t_a")).map(|r| r.name).as_deref(),
            Some("codex")
        );
        assert_eq!(
            reg.last_for(Some("t_b")).map(|r| r.name).as_deref(),
            Some("cursor")
        );
        assert!(reg.last_for(Some("t_c")).is_none());
    }

    #[test]
    fn stdio_uses_its_own_token_key() {
        let reg = Registry::new();
        observe_as(&reg, None, "claude-code", "initialize", 1);
        assert_eq!(
            reg.last_for(None).map(|r| r.name).as_deref(),
            Some("claude-code")
        );
        assert!(reg.last_for(Some("t_a")).is_none());
    }

    #[test]
    fn all_is_sorted_by_first_seen() {
        let reg = Registry::new();
        observe_as(&reg, Some("t_a"), "later", "initialize", 50);
        observe_as(&reg, Some("t_a"), "earlier", "initialize", 10);
        let names: Vec<String> = reg.all().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["earlier", "later"]);
    }

    #[test]
    fn registry_evicts_the_oldest_record_beyond_the_cap() {
        let reg = Registry::new();
        // Spread the records over tokens so that no token reaches its own cap of 16.
        let token_of = |i: usize| format!("t_{}", i / MAX_RECORDS_PER_TOKEN);
        for i in 0..=MAX_RECORDS {
            let token = token_of(i);
            observe_as(
                &reg,
                Some(token.as_str()),
                &format!("client-{i}"),
                "initialize",
                i as i64,
            );
        }
        let all = reg.all();
        assert_eq!(all.len(), MAX_RECORDS);
        assert!(all.iter().all(|r| r.name != "client-0"));
        assert_eq!(all.first().map(|r| r.name.as_str()), Some("client-1"));
        let last = token_of(MAX_RECORDS);
        assert_eq!(
            reg.last_for(Some(last.as_str())).map(|r| r.name).as_deref(),
            Some(format!("client-{MAX_RECORDS}").as_str())
        );
    }

    #[test]
    fn a_token_keeps_at_most_16_records_and_evicts_its_own_oldest() {
        let reg = Registry::new();
        observe_as(&reg, Some("t_b"), "other", "initialize", 0);
        for i in 0..=MAX_RECORDS_PER_TOKEN {
            observe_as(
                &reg,
                Some("t_a"),
                &format!("client-{i}"),
                "initialize",
                i as i64 + 1,
            );
        }
        let all = reg.all();
        assert_eq!(all.len(), MAX_RECORDS_PER_TOKEN + 1);
        assert!(all.iter().any(|r| r.name == "other"));
        assert!(all.iter().all(|r| r.name != "client-0"));
        assert!(all.iter().any(|r| r.name == "client-1"));
        assert_eq!(
            reg.last_for(Some("t_a")).map(|r| r.name).as_deref(),
            Some("client-16")
        );
        assert_eq!(
            reg.last_for(Some("t_b")).map(|r| r.name).as_deref(),
            Some("other")
        );
    }

    #[test]
    fn first_method_is_kept_to_128_bytes_at_a_char_boundary() {
        let reg = Registry::new();
        // 127 ASCII bytes, then a 2-byte char: it would end at byte 129 > 128.
        let method = format!("{}é{}", "m".repeat(127), "x");
        let rec = observe_as(&reg, Some("t_a"), "a", &method, 0);
        assert_eq!(rec.first_method, "m".repeat(127));
        let short = observe_as(&reg, Some("t_a"), "b", "tools/list", 0);
        assert_eq!(short.first_method, "tools/list");
    }

    #[test]
    fn capabilities_over_4_kib_are_stored_as_an_empty_object() {
        let reg = Registry::new();
        let big = json!({"blob": "x".repeat(MAX_CAPS_BYTES)});
        let rec = reg.observe(
            Some("t_a"),
            &info("big", "1"),
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "initialize",
            &big,
            0,
        );
        assert_eq!(rec.capabilities, json!({}));
        let small = json!({"roots": {"listChanged": true}});
        let rec = reg.observe(
            Some("t_a"),
            &info("small", "1"),
            "2025-11-25",
            Era::Legacy,
            Transport::Http,
            "initialize",
            &small,
            1,
        );
        assert_eq!(rec.capabilities, small);
    }

    #[test]
    fn a_known_client_returns_its_original_record_and_moves_the_token_pointer() {
        let reg = Registry::new();
        observe_as(&reg, Some("t_a"), "claude-code", "initialize", 1);
        observe_as(&reg, Some("t_a"), "codex", "initialize", 2);
        let again = reg.observe(
            Some("t_a"),
            &info("claude-code", "9"),
            "2026-07-28",
            Era::Modern,
            Transport::Stdio,
            "tools/list",
            &json!({"huge": "x".repeat(5000)}),
            3,
        );
        assert_eq!(again.version, "1.0");
        assert_eq!(again.first_method, "initialize");
        assert_eq!(again.first_seen_ms, 1);
        assert_eq!(again.capabilities, json!({}));
        assert_eq!(reg.all().len(), 2);
        assert_eq!(
            reg.last_for(Some("t_a")).map(|r| r.name).as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn era_and_transport_strings() {
        let reg = Registry::new();
        let modern_stdio = reg.observe(
            None,
            &info("a", ""),
            "2026-07-28",
            Era::Modern,
            Transport::Stdio,
            "ping",
            &json!({}),
            0,
        );
        assert_eq!(
            (modern_stdio.era, modern_stdio.transport),
            ("modern", "stdio")
        );
        let legacy_http = observe_as(&reg, Some("t_a"), "b", "initialize", 0);
        assert_eq!((legacy_http.era, legacy_http.transport), ("legacy", "http"));
    }

    #[test]
    fn harness_mapping_of_client_names() {
        assert_eq!(harness_of("claude-code"), Harness::ClaudeCode);
        assert_eq!(harness_of("claude-code").as_str(), "claude_code");
        let reg = Registry::new();
        let rec = observe_as(&reg, Some("t_a"), "claude-code", "initialize", 0);
        assert_eq!(rec.harness, "claude_code");
        assert_eq!(
            observe_as(&reg, Some("t_a"), "mystery", "initialize", 0).harness,
            "unknown"
        );
    }

    #[test]
    fn ladder_for_uses_push_only_capability() {
        assert_eq!(
            ladder_for(Harness::ClaudeCode, true),
            [Mechanism::ChannelPush, Mechanism::WaitTool]
        );
        assert_eq!(
            ladder_for(Harness::ClaudeCode, false),
            [Mechanism::WaitTool]
        );
        assert_eq!(ladder_for(Harness::Codex, true), [Mechanism::WaitTool]);
    }

    #[test]
    fn ladder_json_is_the_array_of_mechanism_strings() {
        let l = ladder_for(Harness::ClaudeCode, true);
        assert_eq!(ladder_json(&l), json!(["channel_push", "wait_tool"]));
        assert_eq!(ladder_json(&[]), json!([]));
    }

    const NOW: i64 = 1_800_000_000_000;
    const HOUR_MS: i64 = 60 * 60 * 1000;

    fn full_caps() -> CapabilitiesFile {
        CapabilitiesFile {
            claude: Some(ClaudeCapabilities {
                mods: true,
                async_rewake: true,
                stop_hook: true,
                context_hook: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn beats_of(m: Mechanism, t: i64) -> BTreeMap<Mechanism, i64> {
        BTreeMap::from([(m, t)])
    }

    #[test]
    fn claude_without_caps_or_beats_is_the_m3_ladder() {
        let none = BTreeMap::new();
        for push in [true, false] {
            assert_eq!(
                ladder_for_caps(Harness::ClaudeCode, push, None, &none, NOW),
                ladder_for(Harness::ClaudeCode, push)
            );
        }
    }

    #[test]
    fn full_caps_yield_every_claude_rung_in_order() {
        let none = BTreeMap::new();
        assert_eq!(
            ladder_for_caps(Harness::ClaudeCode, true, Some(&full_caps()), &none, NOW),
            [
                Mechanism::ChannelPush,
                Mechanism::ModSubmit,
                Mechanism::AsyncRewakeHook,
                Mechanism::StopContinue,
                Mechanism::HookInject,
                Mechanism::WaitTool,
            ]
        );
    }

    #[test]
    fn push_disabled_drops_channel_push_even_with_caps() {
        let none = BTreeMap::new();
        let l = ladder_for_caps(Harness::ClaudeCode, false, Some(&full_caps()), &none, NOW);
        assert!(!l.contains(&Mechanism::ChannelPush));
        assert_eq!(l.last(), Some(&Mechanism::WaitTool));
        assert_eq!(l.len(), 5);
    }

    #[test]
    fn a_recent_beat_includes_its_rung_without_caps() {
        let beats = beats_of(Mechanism::HookInject, NOW - HOUR_MS);
        assert_eq!(
            ladder_for_caps(Harness::ClaudeCode, false, None, &beats, NOW),
            [Mechanism::HookInject, Mechanism::WaitTool]
        );
    }

    #[test]
    fn a_beat_older_than_seven_days_is_ignored() {
        let beats = beats_of(Mechanism::HookInject, NOW - heartbeat::SEEN_WINDOW_MS - 1);
        assert_eq!(
            ladder_for_caps(Harness::ClaudeCode, false, None, &beats, NOW),
            [Mechanism::WaitTool]
        );
    }

    #[test]
    fn a_beat_from_the_future_beyond_skew_is_ignored() {
        let beats = beats_of(Mechanism::HookInject, NOW + 61_000);
        assert_eq!(
            ladder_for_caps(Harness::ClaudeCode, false, None, &beats, NOW),
            [Mechanism::WaitTool]
        );
    }

    #[test]
    fn codex_ignores_the_claude_entry() {
        // The `claude` entry is Claude Code's alone; a Codex ladder reads `other["codex"]`.
        let none = BTreeMap::new();
        for push in [true, false] {
            assert_eq!(
                ladder_for_caps(Harness::Codex, push, Some(&full_caps()), &none, NOW),
                [Mechanism::WaitTool]
            );
        }
    }

    const PK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_claude_beat_never_arms_a_codex_rung() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = agora_daemon::Paths::new(home.path());
        let claude = Harness::ClaudeCode;
        heartbeat::beat_for(&paths, PK, claude, Mechanism::StopContinue, NOW).expect("stop beat");
        heartbeat::beat_for(&paths, PK, claude, Mechanism::OwnedSessionApi, NOW).expect("bridge");
        let codex = heartbeat::read_all_for(&paths, PK, Harness::Codex);
        assert_eq!(
            ladder_for_caps(Harness::Codex, false, None, &codex, NOW),
            [Mechanism::WaitTool]
        );
        let own = heartbeat::read_all_for(&paths, PK, claude);
        let l = ladder_for_caps(claude, false, None, &own, NOW);
        assert!(l.contains(&Mechanism::StopContinue));
    }

    #[test]
    fn a_codex_bridge_beat_arms_codex_and_no_other_harness() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = agora_daemon::Paths::new(home.path());
        heartbeat::beat_for(&paths, PK, Harness::Codex, Mechanism::OwnedSessionApi, NOW)
            .expect("bridge beat");
        let codex = heartbeat::read_all_for(&paths, PK, Harness::Codex);
        let l = ladder_for_caps(Harness::Codex, false, None, &codex, NOW);
        assert!(l.contains(&Mechanism::OwnedSessionApi));
        let copilot = heartbeat::read_all_for(&paths, PK, Harness::CopilotCli);
        let l = ladder_for_caps(Harness::CopilotCli, false, None, &copilot, NOW);
        assert!(!l.contains(&Mechanism::OwnedSessionApi));
    }
}
