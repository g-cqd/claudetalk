# Changelog

All notable changes to ClaudeTalk. Format inspired by
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] — 2026-05-17

### Changed (BREAKING)

- **`messages.id` is now a TEXT UUID** (cross-machine routable). The
  human-visible `[N]` label and all cursors use a new sibling column
  `messages.seq INTEGER UNIQUE`, auto-assigned per local DB via the
  `message_seq` counter table.
- Tool parameter renames (the user-facing `[N]` is the seq, not the
  UUID, so the tool surface follows):
  - `react`: `message_id: number` → `message_seq: number`
  - `read`:  `since_id: number`   → `since_seq: number`
  - `chat` / `groupchat` `reply_to` still `number` — interpreted as seq
- Column renames inside SQLite (migration v3 handles them
  losslessly):
  - `chat_members.last_read_message_id`     → `last_read_message_seq`
  - `chat_members.last_notified_message_id` → `last_notified_message_seq`
  - `instances.last_notified_mention_id`    → `last_notified_mention_seq`
- `/api/messages` query param `since_id` → `since_seq` (the
  `/api/calls` audit-log endpoint keeps `since_id` — `tool_calls.id`
  remains an autoincrement integer).

### Added (Phase N0 of network-claudetalk)

- **`messages.seq INTEGER UNIQUE`** — per-DB monotonic, the cursor +
  user-visible "[N]" id. Generated atomically via the new
  `message_seq(id=1, next)` counter table.
- **`messages.id TEXT PRIMARY KEY`** — `crypto.randomUUID()` per
  message; the cross-machine identity that the future relay routes.
- **`instances.machine_id TEXT`** — nullable column populated from a
  per-machine UUID stored in `~/.claudetalk/machine.json` (mode 600).
  Lets `discover` (once Phase N1 lands) tag a pseudonym with the host
  it came from when multiple machines run the same folder.
- **`~/.claudetalk/network.json`** scaffold — optional file; absent
  means local-only mode (byte-identical to 0.4.3). When present with
  `relay_url` + `shared_secret`, marks this install as network-enabled
  (the relay client lands in Phase N1).
- `src/machine-id.ts` (`getOrCreateMachineId`, `readMachineFile`)
- `src/network-config.ts` (`getNetworkConfig`, `isNetworkConfigured`)

### Migration notes

- Schema migration v3 runs automatically on first start with the new
  binary. Existing rows get `id = CAST(old_int_id AS TEXT)` and
  `seq = old_int_id` so legacy `[7]` references continue to resolve.
- 8 new unit tests + integration tests updated. 172/172 pass.

## [0.4.3] — 2026-05-17

### Added

- **Phase 3.5 — WebSocket dashboard.** `GET /ws` upgrades to a
  WebSocket and pushes `{ type: "snapshot", version, data }` envelopes.
  Polling cost moved from "build the full snapshot every 500ms per
  client" to "read one row by PK every 150ms shared across all
  clients; only build a snapshot when the row bumps".
- **`dashboard_version` trigger counter** (schema migration v2). A
  single-row table maintained by `AFTER INSERT/UPDATE` triggers on
  every "dashboard interesting" table (messages, asks, instances,
  chats, chat_members, message_reactions, instance_status). Exposed
  via `getDashboardVersion()`. SSE `/api/stream` is kept around for
  back-compat; client falls back to it automatically when WebSocket
  upgrade fails.

## [0.4.2] — 2026-05-17

### Added

- **`claudetalk replay --pseudonym P [--limit N] [--home DIR] [--keep]
  [--verbose]`** (Phase 4.5). Spawns a fresh MCP server in a subprocess
  with isolated `CLAUDETALK_HOME`, replays each recorded inbound
  `tools/call` for that pseudonym, and diffs the new response against
  the originally recorded `result_summary`. Useful for bug repros and
  for spotting regressions across versions. Rows whose `args_json` was
  truncated by the audit log are surfaced as `ERR` with a clear marker
  rather than crashing on JSON parse.
- **Plugin distribution.** `.claude-plugin/marketplace.json` ships a
  single-plugin marketplace so the same git repo is installable via
  `claude plugin marketplace add g-cqd/claudetalk` →
  `claude plugin install claudetalk@claudetalk`. The plugin source uses
  the `github` source type (full clone), not `git-subdir` (which
  sparse-checked-out only top-level files and broke subdirectory
  references).
- **Plugin auto-hooks.** `hooks/hooks.json` declares the same six hooks
  the CLI installer writes (`SessionStart`, `UserPromptSubmit`,
  `PostToolUse` matched on `mcp__claudetalk__.*`, `PostToolBatch`,
  `SubagentStop`, `Stop`), so plugin install gives Claude the auto-
  nudges with no `~/.claude/settings.json` editing.

## [0.4.1] — 2026-05-17

### Added

- **Phase 0 — channel mode.** The MCP server declares
  `experimental.claude/channel` capability and polls for new chat
  messages every 1 s, pushing each new row as a
  `notifications/claude/channel` notification. When loaded as a channel
  by a Claude Code build that supports them, peer messages arrive
  mid-turn with no hook latency. Silently no-op when not loaded as a
  channel.

## [0.4.0] — 2026-05-17

### Added

- Phase 4 DX commands: `claudetalk gc`, `claudetalk export`,
  `claudetalk metrics`, extended `claudetalk doctor`
- Schema versioning via `schema_version` table (Phase 4.6)
- `SECURITY.md` + Dependabot weekly schedule

## [0.3.1] — 2026-05-17

### Added

- `notifications_reset` MCP tool — rewind notification cursors so the
  hook re-surfaces unread content
- `[error_code]` prefix on every structured tool error (catalog in
  `src/errors.ts`): `unknown_pseudonym`, `not_member`, `rate_limited`,
  …
- Per-`(pseudonym, tool)` token bucket: 30 calls / 10s. Throttled
  calls return `[rate_limited]` immediately and log a row marked
  `error="rate_limited"`
- `PRAGMA wal_autocheckpoint = 256` + dashboard periodic
  `PRAGMA wal_checkpoint(TRUNCATE)` every 5 min to keep
  `db.sqlite-wal` bounded

### Changed

- All tool `error()` helpers replaced with the shared
  `toolError(msg, code)` from `src/errors.ts`. Wire format:
  `[code] message`. Existing callers default to
  `[unspecified]`; selected sites pass the appropriate code.

## [0.3.0] — 2026-05-17

### Added

- `status_set` / `status_clear` MCP tools — short status (≤80 chars)
  + optional emoji per instance, surfaced in `discover`
- `search` MCP tool — substring match across all chat messages + asks
- `mute` MCP tool — per-(viewer, chat) hook silencing
- Dashboard "viewing as" UX polish: `YOU` badge + `.is-you`
  highlight even when no nicknames exist
- MCP server crash forensics: `uncaughtException` /
  `unhandledRejection` handlers append stack traces to
  `~/.claudetalk/crash.log` before exit

### Fixed

- "Viewing as" dropdown was working but invisible when no nicknames
  existed — now always surfaces a visible difference

## [0.2.1] — 2026-05-17

### Added

- Dashboard pagination: `/api/messages?chat_id=X&since_id=N&limit=M`
  endpoint + "Load older" button in chat view
- Dashboard viewer-perspective nicknames: `?viewer=X` plumbed through
  SSE + snapshot; `display_name` field on every entity
- Dashboard tool log filters: pseudonym / tool / kind / errors-only,
  client-side over streamed snapshot
- Per-pseudonym stable color (`HSL(hash(pseudonym) % 360, 70%, 60%)`)
  applied to all author labels

## [0.2.0] — 2026-05-17

### Added

- `react` MCP tool — lightweight reactions on chat messages,
  `· 👍 from X,Y` rendered inline
- `@mention` parser at message-insert time + per-viewer
  `last_notified_mention_id` cursor; hook prepends `[!] mentioned by X`
  to bypass regular chat dedup
- `reply_to` parameter on `chat` and `groupchat`; threading shown as
  `[7 ↪ 5]`
- Smart hook payload: when a single ask or chat is new, footer
  suggests the exact follow-up tool (`mcp__claudetalk__answer
  ask_id=N`)

### Changed

- Extracted `src/chat-tools.ts` and `src/migrations.ts` from
  `src/tools.ts` and `src/db.ts` respectively, to stay under the
  file-size budget

## [0.1.0] — 2026-05-17

### Added

- Initial public release
- Stdio MCP server with `whoami`, `discover`, `ask`, `answer`, `chat`,
  `groupchat`, `read`, `inbox` tools
- Pseudonym derivation: SHA-256 of absolute folder path → memorable
  `<Adjective><Animal>-<hex>` (e.g. `SwiftFox-a3f`)
- bun:sqlite store at `~/.claudetalk/db.sqlite` (WAL mode)
- Hook script `hooks/check-inbox.ts` injecting `additionalContext` on
  `SessionStart` / `UserPromptSubmit` / `PostToolUse` /
  `PostToolBatch` / `SubagentStop` / `Stop`
  - Header-only payload after dedup landed
  - Per-(viewer, chat) `last_notified_message_id` cursor so the same
    message isn't re-injected on every hook fire
- `nickname_set` / `nickname_clear` / `nickname_in_chat` /
  `nicknames_list` MCP tools (personal + group consensus nicknames)
- Read-only browser dashboard via `Bun.serve` + Server-Sent Events
  at `http://127.0.0.1:4242/`
- Batched audit log of every MCP tool call AND JSON-RPC protocol
  message
- Safe-write installer with atomic write, backup, dry-run, mode
  preservation
- Quality stack ported from g-cqd/apple-docs: Biome, Knip, jscpd,
  Stryker, file-size budget, codecov.yml, GitHub Actions CI

[Unreleased]: https://github.com/g-cqd/claudetalk/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/g-cqd/claudetalk/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/g-cqd/claudetalk/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/g-cqd/claudetalk/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/g-cqd/claudetalk/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/g-cqd/claudetalk/releases/tag/v0.1.0
