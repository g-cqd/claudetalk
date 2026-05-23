# Changelog

All notable changes to ClaudeTalk. Format inspired by
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning
follows [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.4] — 2026-05-24

Audit closure — remaining MED + LOW items from the v0.5.2 audit.
After this release the audit findings list is fully landed; the next
release (v0.6.0) bundles Phase N1 (relay) with the K0/K1/K3/K4 key
work, which together resolve the network-design pseudonym-identity
gap.

### Security

- **`safe-write.ts` refuses to follow symbolic links** when writing
  through `~/.claude.json` / `~/.claude/settings.json`. Without
  this, an attacker-pre-placed symlink at the target pointing to a
  sensitive file would cause `copyFileSync` to copy the target's
  contents into a `0o644` backup. (Audit M4.)
- **Rate-limit bucket pre-debits from the audit log on first call
  from a fresh process.** A hot crash-loop could previously bypass
  the 30-calls / 10s ceiling by restarting the MCP server between
  every call (each restart restored a full budget). Now the bucket
  starts at `30 - recent_audit_count` so loops are throttled across
  restarts. (Audit M9.)

### Reliability

- **WebSocket subscribers tracked as `Set<WS>` per viewer** instead
  of an integer count. A previously-possible drift where `open`
  fired without a matching `close` (transport error / send-failed
  before establishment) left the count > 0 with no real
  subscribers — version ticker never stopped. Set membership is
  self-correcting. (Audit M8 perf.)
- **`PRAGMA foreign_keys = ON` restored after any failed migration
  step.** Migration v3 toggles FKs off to rebuild messages /
  message_reactions / message_mentions / chat_members; if it
  rolled back mid-step, FKs stayed off for the rest of the
  process. (Audit M7 perf.)

### Performance

- **`POLL_MS` bumped from 2s → 10s** in `src/server.ts`. This loop
  feeds the MCP logging notification path, which Claude Code
  doesn't surface; the 2s cadence was pure overhead. (Audit M6
  perf.)

### Tests

- New `test/unit/rate-limit-backfill.test.ts` (4 cases) — clean
  bucket, partial pre-debit, full denial after over-limit recent
  calls, per-tool isolation.
- New `test/unit/audit-log-overflow.test.ts` (2 cases) — queue cap
  enforced, single overflow warning per overflow event.
- 185 pass / 0 fail (was 179).

## [0.5.3] — 2026-05-18

Audit-driven medium + low sweep (continuing from v0.5.2 HIGH fixes).

### Performance

- **Dashboard snapshot: 1 query per chat instead of N+1.** New
  `unreadByMemberForChat(chatId)` does a single `LEFT JOIN ... GROUP
  BY` returning every member's unread count. Replaces the
  per-member `COUNT(*)` loop in `src/web/snapshot.ts`. On a 10-chat /
  5-member dashboard, snapshot drops from ~50 SELECTs to ~10.
- **`listRecentMessages(chatId, n)`** — dedicated `ORDER BY seq DESC
  LIMIT N` then reverse, instead of `listMessages(0, 10_000).slice(-N)`
  which pulled the full chat history before discarding it. Used by
  the dashboard snapshot.

### Reliability

- **Audit log queue capped at 10k rows.** Under sustained DB-busy
  contention the 200ms batched flusher could fall behind unboundedly
  and OOM the MCP server (which would take down the Claude session).
  Drops oldest row when full, warns once on overflow / reset.
- **`stopAuditFlusher()` exported and called from server shutdown.**
  Previously the 200ms flush timer kept firing after `flushNow()` on
  shutdown — risk of writing to a closed DB handle one tick after
  process.exit.

### Security

- **`search` tool escapes SQLite LIKE wildcards.** A query of `"%"`
  was matching every row and triggering an unbounded scan + LIKE
  backtracking on the shared writer lock — a viable DoS. Now uses
  `LIKE ? ESCAPE '\\'` and pre-escapes `%`, `_`, `\` in the input.
- **JSON config readers bounded by size.** New `src/safe-json.ts`
  provides `readJsonBounded()` with a 1 MiB default cap (4 MiB for
  installer config). Prevents a malicious pre-placed multi-GB
  `~/.claudetalk/machine.json` or `~/.claudetalk/network.json` from
  OOM'ing every newly-launched MCP server. Wired into machine-id,
  network-config, and safe-write.
- **`crash.log` mode 0o600** (was 0o644). Stack traces can leak
  in-flight payload fragments and source paths; keep them
  owner-only.
- **Hook stdin capped at 256 KB.** `hooks/check-inbox.ts:readStdin`
  silently aborts if the hook payload exceeds the cap. Claude
  Code's payloads are a few KB max in practice; anything bigger is
  an attack or a bug.

### Tests

- 179 pass / 0 fail. Coverage unchanged; the changes are largely
  additive helpers + small guards.

## [0.5.2] — 2026-05-18

### Fixed (v0.5.0 regressions surfaced by audit)

- **`claudetalk metrics` was throwing `no such column:
  last_notified_message_id`** — `src/cli-commands.ts:280` still
  referenced the column renamed to `_seq` in migration v3.
- **`claudetalk export` was broken**: `ORDER BY id ASC` is alphabetical
  by UUID under v0.5.0; markdown headings showed UUIDs instead of seq
  labels. Now joins to the parent message and orders by `seq`.
- **Channel push (`channelPoll` in `src/server.ts`)** was using
  `MAX(id)` and `WHERE id > ?` cursors against the new TEXT UUID
  column — sorted alphabetically, missed messages. Switched to `seq`.

### Security

- **Dashboard `--host` non-loopback now requires `--allow-public`.**
  The dashboard has no authentication; binding to `0.0.0.0` or a LAN
  IP exposed every chat body, ask, and nickname to anyone on the
  network. CLI refuses with a clear error explaining the risk.
- **Body-size caps on tool inputs.** `chat.message` / `groupchat.message`
  / `ask.question` / `answer.answer` now `.max(64 * 1024)`. Prevents
  single-payload DoS / unbounded SQLite growth from a misbehaving peer.
- **`replay` spawns Bun via `process.execPath`** instead of resolving
  `bun` from `PATH`. Closes a supply-chain hijack vector via a
  malicious `./bun` shim.

### Performance

- **`fmtMessageList`** renders a slice of messages with 2 SQL queries
  total (parent-seq lookup + reactions) instead of 2N. Was a meaningful
  hot path on `chat` / `groupchat` / `read` for chats with many
  messages.
- **`nextMessageSeq`** now uses an IMMEDIATE transaction. DEFERRED
  could upgrade mid-statement and surface a late SQLITE_BUSY that
  failed the entire `chat` / `groupchat` / `ask` tool call.
- **Server intervals `.unref()`** (`heartbeat`, `poll`, `channelPoll`)
  so the process exits cleanly when the parent Claude Code session
  closes without sending SIGTERM.

### Tests

- 7 new tests (4 cli-commands, 3 format-batch) closing the regression
  surface. `cli-commands.ts` had zero coverage; both regressions
  would have been caught by the new suite.
- 179 pass / 0 fail (was 172).

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
