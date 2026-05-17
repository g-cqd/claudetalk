# ClaudeTalk Network-Open Design

**Version:** Draft 1.0  
**Date:** 2026-05-17  
**Scope:** Evolution of ClaudeTalk 0.4.x from single-machine to multi-machine
messaging for a single user with 1–10 devices.  
**Non-goal:** Multi-user / multi-tenant SaaS.

---

## 1. Current State — What We Are Extending

The following is architecture-as-read from the source, not assumption.

```
Claude Code session (laptop)
  └── Bun stdio MCP server (src/server.ts)
        ├── Opens bun:sqlite → ~/.claudetalk/db.sqlite (WAL mode)
        ├── Upserts presence row in `instances` table
        ├── 30 s heartbeat updates last_seen
        ├── 2 s poll emits logging notifications
        ├── 1 s channel poll fires claude/channel push notifications
        └── Registers ~18 tools via registerTools() / registerChatTools()

Claude Code session (desktop, same box)
  └── Separate Bun stdio MCP server
        └── Same db.sqlite — WAL writer concurrency handles it
              PRAGMA busy_timeout = 500
              PRAGMA wal_autocheckpoint = 256
```

Hooks fire `hooks/check-inbox.ts` on five events. The hook reads the same
SQLite file, advances notification cursors, and emits a system message or
additionalContext nudge. Pseudonyms are `SHA-256(absolute_folder_path)` mapped
to an adjective-animal-hex triple. They are stable per folder, not per machine.

**The shared SQLite file is the entire "network."** Extending to multiple
machines means replacing or augmenting that file with something reachable across
machines. Everything else — tools, schema, hooks, pseudonyms — can be kept or
minimally extended.

---

## 2. Context Assessment

| Knob | Value | Reasoning |
|------|-------|-----------|
| Uncertainty | Medium | Transport/auth choices well-understood; integration with MCP hook lifecycle is novel |
| Dependency density | Low | Single user; no multi-team coordination overhead |
| Regulatory burden | None | Personal tool; no compliance gates |
| Work arrival mode | Batched | Feature shipped in discrete phases; no continuous deployment pressure |

---

## 3. Problem Decomposition

Before comparing architectures, the eight problem areas from the brief are
assessed independently, because the right answer for transport strongly
constrains identity and storage answers.

### 3.1 NAT / Firewall Reality

Most developer machines live behind NAT (home router, VPN kill-switch, cloud
security group). A design that requires any machine to accept inbound TCP
connections will require user-managed firewall rules and will fail silently in
many setups. This is the dominant forcing function: **the design must work
without inbound connectivity on any node.** This rules out pure peer-to-peer
direct TCP and Tailscale mesh-with-direct-fallback as a primary strategy (both
can work but add operational complexity before the user gets a single message
through).

### 3.2 Transport Shortlist

After filtering for NAT-resilience and zero-inbound requirement:

**A. Central WebSocket relay** — every node holds a long-lived outbound WS to a
small Bun service. All traffic flows through the relay. Relay knows metadata
(pseudonyms, chat IDs, timestamps) but not content if messages are encrypted
before sending. Works behind any NAT. Relay can be a Cloudflare Worker (free
tier), a Fly.io shared-CPU micro, or a small Hetzner box.

**B. Polling on hosted SQL** — every node polls a hosted database (Turso LibSQL,
PlanetScale, Neon Postgres, or Cloudflare D1). No persistent connection. Write
goes through the same DB helpers, just against a remote endpoint. SQLite-over-HTTP
(Turso/D1) is the most compelling because schema migrations, WAL mode, and
triggers are already written for SQLite. Eventual consistency with ~1–5 s poll
lag is acceptable for this use case.

**C. Polling on S3-compatible object store** — messages serialised as
append-only JSON objects keyed by `{chat_id}/{sequence}`. Optimistic concurrency
with ETags. No SQL. Privacy-friendly (objects can be encrypted client-side).
Operational simplicity (Cloudflare R2 free tier, no server). Highest dev cost
per feature added because the query model must be re-built.

**D. Iroh (QUIC P2P with relay fallback)** — Iroh provides a QUIC-based
transport where each node connects to iroh.computer's relay and peers can
exchange data directly or via relay. Designed exactly for this use case. Adds a
Rust/WASM or Node.js native addon dependency. Bun's FFI can call native code but
the Iroh JS/TS bindings are immature as of mid-2025. Engineering cost is high
for unproven Bun compatibility.

**E. Overlay network (Tailscale / Cloudflare Tunnel)** — each box exposes its
local MCP as an HTTP endpoint. Tailscale handles the mesh. Requires the user to
install and configure Tailscale on every machine. High bootstrap friction. Not
self-contained within the plugin.

**F. Wrap an existing protocol (Matrix, XMPP, Discord bot)** — uses an existing
identity and transport layer. Cheapest for auth (OAuth to that service). Forces
the message schema into the service's shape. Creates external identity dependency
(what happens if the Discord bot gets banned?). Leaks message content to a third
party unless E2E-encrypted before posting. Viable as a quick prototype, poor as
a foundation.

### 3.3 Authentication

The user must prove they own all their machines without running a user system.

**Option 1: Shared symmetric secret** — a random 256-bit key written to
`~/.claudetalk/network.json` on first `claudetalk auth init`. The user
distributes it to other machines via their preferred mechanism (1Password,
iCloud Keychain, dotfiles repo, `scp`). The relay or cloud store requires this
key (as a bearer token or as a HMAC of each message). Lowest implementation
cost. Zero dependency on any identity provider.

**Option 2: GitHub OAuth device flow** — `claudetalk auth login` opens the
device flow. Token stored in `~/.claudetalk/network.json`. The relay validates
the token against GitHub API to confirm the same GitHub login. Lowest friction
for users who already use GitHub. Relay must call GitHub on every auth check or
cache the user ID. Creates dependency on GitHub availability.

**Option 3: Anthropic API key proxy validation** — risky. The API key is a
powerful credential that should not leave the machine. Even if only hashed, a
leaked relay could harvest hashes. Ruled out.

**Option 4: PAT issued by a `claudetalk auth` service** — requires running and
maintaining an issuer service. Overkill for one-user deployment. Ruled out for
initial versions.

**Recommendation: shared symmetric secret (Option 1).** It is the only option
with zero runtime dependencies on third-party identity services. It can be
upgraded to GitHub OAuth in a later phase if multi-user is ever desired.

### 3.4 Identity Model

Pseudonyms today are `SHA-256(absolute_folder_path)`. Across machines:

- Two machines that both have `~/code/myapp` will get the same pseudonym.
  This is actually desirable: the agent in `~/code/myapp` on the laptop and
  the agent in `~/code/myapp` on the desktop represent the same logical context.
  Messages addressed to that pseudonym reach whichever machine is currently
  running it.
- If you want two separate agents both working `~/code/myapp` simultaneously on
  different machines to be distinct, they must be namespaced by machine.

**The right model depends on the expected usage pattern.** Two likely scenarios:

Scenario A — Continuity: you work on `~/code/myapp` on the laptop, then switch
to the desktop. You want to resume the same conversation. Folder-derived
pseudonym, no machine namespace. Collision IS the feature.

Scenario B — Parallelism: you have both machines running agents on the same
folder simultaneously and you want to distinguish them. Folder-pseudonym is
ambiguous; you need `<machine-id>:<folder-pseudonym>`.

**Recommendation: keep folder-derived pseudonyms as-is for the first phase.
Add a machine ID field to the presence row so that tools like `discover` can
display `(laptop)` / `(desktop)` annotations without changing the routing
identity.** Machine ID is stored in `~/.claudetalk/machine.json` as a UUID
generated once (random, sticky). It is displayed as metadata, not embedded in
the pseudonym itself. This preserves all existing routing logic and allows
a future `namespace_by_machine` config flag to be added later.

### 3.5 Storage and Consistency

The current local schema is the source of truth. For network mode, two strategies
make sense:

**Write-through to canonical store, local cache for reads:** each node writes
directly to the canonical store (remote DB or relay). The local SQLite is a
cache populated by polling. Hooks read from local SQLite, which is fast and
never blocks the hook on network. Acceptable eventual consistency lag: 1–5 s.
This is the architecture of every mobile-app-with-offline-mode: local DB is the
UI layer, remote is the sync layer.

**CRDT replication (Yjs / Automerge):** messages are append-only, which maps
trivially to a CRDT. No merge conflicts. But Yjs/Automerge add ~150 KB of
dependencies, require document state serialization, and add complexity to the
migration story. The gain over "append-only log with timestamps" is minimal for
this use case. Messages cannot be edited or deleted (only reactions change
after the fact, and reactions are also append-only except for removes).
Ruled out as over-engineered.

**Recommendation: write-through to canonical store + pull-on-poll for catch-up
+ WebSocket push for live delivery.** Local SQLite remains the read layer.

### 3.6 Privacy and Encryption

Messages contain code snippets, file paths, error stacks, and proprietary
context. The relay operator (even if it is you running it on a VPS) should not
be able to read message bodies. [Strong] Defense-in-depth requires encryption
even for a personal relay, because the relay may run on a shared host, behind a
CDN with TLS termination, or get compromised.

**Recommended model: libsodium secretbox (XSalsa20-Poly1305) with a key derived
from the shared secret.** Specifically:

- Message body encrypted with `secretbox(key=HKDF(shared_secret, "ct-messages"), nonce=random24)`
- Metadata (pseudonyms, chat IDs, timestamps) transmitted in the clear — the
  relay needs these for routing; they are not sensitive.
- The relay sees: from-pseudonym, to-pseudonym, chat-id, timestamp, encrypted
  blob. It cannot read the body.
- Key rotation: a new shared secret revokes all prior sessions. Simple for
  one-user.

Bun ships with the Web Crypto API (`crypto.subtle`). HKDF and AES-GCM are
available without native modules. For XSalsa20-Poly1305 specifically, the
`@stablelib/xchacha20poly1305` package (pure TS) is a viable alternative that
avoids native addon dependencies.

**For the initial phase: TLS in transit only ("it's all yours anyway" trust
model).** The relay is HTTPS. Encryption at rest in the canonical store. E2E
encryption is Phase 2 work. Document the gap explicitly.

### 3.7 Operational Concerns

**Cost:** Cloudflare Workers + D1 (SQLite) — free tier covers ~100K requests/day
and 5 GB storage. For 1–10 machines doing 1 s polling each, that is 86,400
requests/machine/day. At 10 machines: 864,000 requests/day, which exceeds the
free tier. Workers paid plan is $5/month with 10M requests included. Fly.io
shared-CPU-1x with 256 MB RAM is $1.94/month. Hetzner CX11 (2 vCPU, 2 GB RAM)
is €3.79/month. The relay is a small Bun process — any of these work.

For Turso (LibSQL hosted): free tier is 500 databases, 9 GB storage, 1B row
reads/month. Easily sufficient for personal use at zero cost.

**Bootstrapping UX goal:** `claudetalk auth init` on Machine 1 generates a
shared secret and prints a `claudetalk auth add-machine` command (or QR code)
that the user runs on Machine 2. Machines 2+ do not need to run a relay — they
connect to the relay URL stored in `~/.claudetalk/network.json`. Target: under
2 minutes from zero to two machines seeing each other.

**Backwards compatibility:** local-only mode must work with zero changes. Network
mode activates only when `~/.claudetalk/network.json` exists and has a
`relay_url` (or equivalent). All 18 existing tools work unchanged. Two new tools
are added: `connect_network` (manual trigger) and `peers` (with a `--remote`
flag). The hook reads local SQLite exactly as today; when online, a background
sync goroutine keeps local SQLite current.

---

## 4. Candidate Architectures

### Architecture A — Hosted SQL Relay (Turso/D1) with Background Sync

```
 Machine 1 (laptop)                    Machine 2 (desktop)
 ┌─────────────────────────────┐       ┌─────────────────────────────┐
 │ Claude Code session         │       │ Claude Code session         │
 │   └── MCP server (Bun)      │       │   └── MCP server (Bun)      │
 │         ├── local SQLite    │       │         ├── local SQLite    │
 │         │   (read/UI layer) │       │         │   (read/UI layer) │
 │         └── SyncWorker      │       │         └── SyncWorker      │
 │               │ poll 5 s    │       │               │ poll 5 s    │
 └───────────────┼─────────────┘       └───────────────┼─────────────┘
                 │                                     │
                 │ HTTPS (LibSQL wire protocol)        │
                 ▼                                     ▼
         ┌───────────────────────────────────────────────┐
         │  Turso (hosted LibSQL / SQLite-compatible)    │
         │  Single database: claudetalk-<user-id>        │
         │  Same schema as local db.sqlite               │
         │  Auth: bearer token (shared secret hash)      │
         └───────────────────────────────────────────────┘
```

**How it works:**
- A `SyncWorker` runs inside the MCP server process as a `setInterval` every 5 s.
- On each tick: push local rows created since last-sync-cursor to Turso; pull
  rows from Turso created by other machines since last-pull-cursor.
- Local SQLite is the authoritative read source for all tool calls and hooks.
- Writes to messages/asks go to local SQLite first (instant), then async to Turso.
- Conflicts: message IDs are local-autoincrement and will collide across machines.
  Fix: add a `global_id UUID` column (or a `machine_id:local_id` composite key)
  to the network schema. The local schema keeps its autoincrement IDs; the sync
  layer maps them.
- Push latency: up to 5 s poll interval (configurable down to 2 s).
- No persistent connection required. Works behind any NAT, through VPN, behind
  corporate proxies.

**Strengths:**
- LibSQL is wire-protocol-compatible with SQLite. Existing query code largely
  reuses. Turso's TypeScript SDK supports the same `Database.run()` / `.query()`
  pattern.
- No relay binary to write or maintain.
- Free tier covers personal use at low poll rates (5 s poll, 10 machines:
  ~172,800 reqs/day — within Turso free limits for row reads).
- Offline resilience: local SQLite buffers indefinitely. Sync on reconnect.
- Schema already exists and is tested.

**Weaknesses:**
- ID collision requires a migration (adding `global_id` or changing primary keys).
  This touches the most fundamental invariant in the codebase.
- Pull latency is 5 s worst-case. Not real-time.
- Encryption: Turso sees plaintext unless you encrypt bodies before insert.
  Adding E2E encryption must happen before trusting Turso with content.
- Turso is a third-party service dependency.

**Dev cost estimate:** Medium. The ID migration is the hardest part.
SyncWorker is ~300 LOC. No new binary.

---

### Architecture B — Central WebSocket Relay with Local SQLite Mirror

```
 Machine 1 (laptop)                    Machine 2 (desktop)
 ┌─────────────────────────────┐       ┌─────────────────────────────┐
 │ Claude Code session         │       │ Claude Code session         │
 │   └── MCP server (Bun)      │       │   └── MCP server (Bun)      │
 │         ├── local SQLite    │       │         ├── local SQLite    │
 │         │   (read/UI layer) │       │         │   (read/UI layer) │
 │         └── RelayClient     │       │         └── RelayClient     │
 │               │ WS (TLS)   │       │               │ WS (TLS)    │
 └───────────────┼─────────────┘       └───────────────┼─────────────┘
                 │                                     │
                 ▼                                     ▼
         ┌──────────────────────────────────────────────┐
         │  Relay Service (Bun, ~200 LOC)               │
         │  Hosted: Fly.io shared-CPU / Hetzner / etc.  │
         │                                              │
         │  In-memory: Map<room, Set<WebSocket>>        │
         │  Durable store: SQLite (local) or D1/Turso   │
         │  Auth: HMAC-SHA256(shared_secret, timestamp) │
         │                                              │
         │  Routes:                                     │
         │    /ws?token=...  — WebSocket upgrade        │
         │    /pull?since=N  — HTTP catch-up (REST)     │
         └──────────────────────────────────────────────┘
```

**How it works:**
- Each MCP server process opens one outbound WebSocket to the relay on startup
  (with auth token). Reconnects with exponential back-off.
- When a tool call writes a message locally, it also sends a `publish` frame to
  the relay over the open WebSocket.
- The relay broadcasts the frame to all other connected sessions in the same
  namespace (same shared secret).
- Receiving MCP servers write incoming frames into their local SQLite.
- On reconnect after offline period, the `RelayClient` fires a REST `GET /pull`
  to catch up missed messages. The relay's durable store (its own SQLite or
  Turso) holds the canonical log.
- Hooks read local SQLite as today — no change.

**Push latency:** near real-time (~50–150 ms) when both machines are online.

**Strengths:**
- Real-time delivery. The 1 s channel poll in `server.ts` could be eliminated
  (replaced by WS push into local SQLite).
- Relay is a single small Bun binary (~200 LOC). Easy to audit.
- ID collision handled by the relay: relay assigns globally-unique message IDs.
  Local SQLite stores the relay-assigned ID in a `remote_id` column.
- Relay can be end-to-end encrypted: relay never sees decrypted bodies.
- Offline buffering: local SQLite holds outbound queue; flush on reconnect.

**Weaknesses:**
- Requires a deployed Bun service. Users must host something.
- WebSocket reconnection logic adds complexity (back-off, heartbeat, session
  resumption).
- The relay is a single point of failure. If it is down, network messages do not
  flow until it recovers. (Local-only mode still works.)
- Relay needs durable storage for catch-up. If relay is stateless, it cannot
  serve pull requests after restart.

**Dev cost estimate:** Medium-High. The relay binary is small but
`RelayClient` reconnect logic, outbound queue, and catch-up REST are ~500 LOC.
Plus IaC/deployment documentation.

---

### Architecture C — S3-Compatible Object Store (Cloudflare R2)

```
 Machine 1 (laptop)                    Machine 2 (desktop)
 ┌─────────────────────────────┐       ┌─────────────────────────────┐
 │ Claude Code session         │       │ Claude Code session         │
 │   └── MCP server (Bun)      │       │   └── MCP server (Bun)      │
 │         ├── local SQLite    │       │         ├── local SQLite    │
 │         └── ObjectSyncWorker│       │         └── ObjectSyncWorker│
 │               │ HTTPS       │       │               │ HTTPS       │
 └───────────────┼─────────────┘       └───────────────┼─────────────┘
                 │                                     │
                 ▼                                     ▼
         ┌──────────────────────────────────────────────┐
         │  Cloudflare R2 bucket                        │
         │  Objects:                                    │
         │    events/{unix-ms}-{machine-id}-{rand}.json │
         │  Encrypted with AES-GCM client-side          │
         │  Auth: R2 API token (stored in network.json) │
         └──────────────────────────────────────────────┘
```

**How it works:**
- Every write (new message, new ask, answer) produces an encrypted JSON event
  object uploaded to R2. Object key embeds millisecond timestamp for ordering.
- Pull: each node lists objects newer than its last-sync watermark and downloads
  them. Objects are decrypted and written to local SQLite.
- Presence: a `presence/{pseudonym}.json` object updated by heartbeat.
- No relay binary. No SQL service.

**Strengths:**
- Zero server to maintain. R2 free tier: 10 GB storage, 10M reads/month,
  1M writes/month. Zero cost for personal use.
- E2E encryption is natural: objects encrypted before upload. R2 sees ciphertext.
- NAT-resilience: pure outbound HTTPS.
- Object store is extremely durable (11-nines).

**Weaknesses:**
- No SQL query model. Presence, discover, search all require downloading and
  deserializing lists of objects. At low message volume this works; at thousands
  of messages, listing objects becomes slow.
- Ordering: `LIST` on R2 is lexicographic by key. Millisecond-timestamp-prefixed
  keys give ordering but not transactional consistency. Two machines writing in
  the same millisecond produce unordered concurrent objects.
- No server-side filtering. Every poll downloads object metadata for all new
  events, then fetches bodies. Much more data transfer than SQL.
- Highest dev cost per feature. Every existing SQL query must be replaced with
  object-store logic.
- R2 ListObjects has eventual consistency (not bounded), which can cause missed
  events in high-concurrency bursts.

**Dev cost estimate:** Very High. Essentially re-writing the storage layer.

---

## 5. Tradeoff Table

| Dimension | A: Hosted SQL (Turso) | B: WS Relay | C: Object Store (R2) |
|-----------|----------------------|--------------|-----------------------|
| **Push latency** | 2–10 s (poll) | 50–200 ms (live) | 3–15 s (poll) |
| **Privacy (default)** | Relay reads plaintext | Relay reads plaintext | E2E by default |
| **Privacy (hardened)** | E2E with pre-insert encrypt | E2E trivial (relay never sees body) | Already E2E |
| **Infra cost** | $0 (Turso free) | $2–6/mo (Fly/Hetzner) | $0 (R2 free) |
| **Dev cost** | Medium (ID migration + SyncWorker) | Medium-High (relay + RelayClient) | Very High (full storage rewrite) |
| **Bootstrap friction** | Low (one API token) | Medium (deploy relay, then token) | Low (one R2 token) |
| **NAT resilience** | Full (outbound HTTPS) | Full (outbound WS) | Full (outbound HTTPS) |
| **Offline behavior** | Write-through local; sync on reconnect | WS reconnect + pull REST | Upload on reconnect |
| **Schema reuse** | High (LibSQL ≈ SQLite) | Medium (relay has own storage) | Low (object model) |
| **Local-only unchanged** | Yes | Yes | Yes |
| **E2E encryption** | Phase 2 | Phase 2 (trivial to add) | Phase 1 |
| **New runtime deps** | `@libsql/client` | `ws` (already available in Bun) | `@aws-sdk/client-s3` or `@cloudflare/r2` |
| **Single point of failure** | Turso availability | Relay availability | R2 availability |
| **Test coverage carryover** | High (same SQLite schema) | Medium | Low |
| **Discoverability/search** | Full SQL | Full SQL (local mirror) | Not viable |

---

## 6. Recommended Architecture

**Architecture B — Central WebSocket Relay with Local SQLite Mirror,
with E2E encryption in Phase 2.**

**Rationale:**

1. Real-time delivery matters. The hook polling model fires at best every few
   seconds (on PostToolUse events); a 5 s database poll on top of that means a
   user on Machine 1 may wait 10–15 s to see a message sent from Machine 2.
   The claude/channel push already exists in the server (`server.ts` lines
   144–207). A WebSocket relay can feed that same push path directly, giving
   sub-200 ms cross-machine latency. [Moderate: real-time messaging UX research
   consistently shows that latency above 1–2 s breaks the conversational feel.]

2. Schema reuse is highest. Local SQLite stays unchanged. The relay's durable
   store uses the same schema. The `SyncWorker` / `RelayClient` is an additive
   layer, not a replacement.

3. The relay is small and auditable. A Bun WebSocket server that validates a
   HMAC token, routes frames by namespace, and stores them in SQLite is ~200
   LOC. It can be reviewed in an afternoon. The user runs it; they own the
   trust boundary.

4. E2E encryption is a clean second step. Because the relay already receives
   frames as opaque objects (pseudonym, chat_id, encrypted_body), adding
   encryption does not require changing the relay or its protocol. Client encrypts
   before send, client decrypts after receive. The relay is not modified.

5. Architecture A's Turso dependency is a managed service outside the user's
   control. Turso's free tier is generous now but their pricing has changed once.
   Running a small Bun relay on a €4/month VPS is more predictable.

6. Architecture C's object store model requires re-implementing every SQL query
   as object-store enumeration. The existing 154+ tests would need to be rewritten.
   The dev cost is disproportionate to the benefit.

**Where Architecture A is better:** if the user has zero desire to run a relay
and is comfortable trusting Turso with plaintext messages, Architecture A ships
faster. The ID migration is the only hard technical risk. If that risk can be
isolated, A is a valid alternative. The recommendation is B, but A is flagged
as a lower-friction starting point if deployment friction is the blocker.

---

## 7. Recommended Architecture — Detail

### 7.1 Component Diagram

```
 MACHINE (any, 1..N)
 ┌──────────────────────────────────────────────────────────────┐
 │                                                              │
 │  Claude Code                                                 │
 │    └── MCP stdio server (src/server.ts — unchanged API)      │
 │          │                                                   │
 │          ├── Local SQLite (~/.claudetalk/db.sqlite)          │
 │          │     read layer for all tools + hooks              │
 │          │                                                   │
 │          └── RelayClient (new: src/relay-client.ts)          │
 │                │                                             │
 │                │  outbound wss://relay.example.com/ws        │
 │                │  Auth: Authorization: Bearer HMAC(secret,ts)│
 │                │  Protocol: JSON frames over WebSocket        │
 │                │                                             │
 │                ├── send: on every insertMessage / insertAsk  │
 │                │         (writes local THEN sends to relay)  │
 │                │                                             │
 │                ├── recv: on frame arrival, write to local DB │
 │                │         then signal claude/channel push      │
 │                │                                             │
 │                └── catch-up: on connect, GET /pull?since=N   │
 │                              to replay missed frames         │
 │                                                              │
 │  hooks/check-inbox.ts — UNCHANGED                            │
 │    reads local SQLite exactly as today                       │
 │                                                              │
 └──────────────────────────────────────────────────────────────┘
                       │ wss (TLS)
                       ▼
 ┌──────────────────────────────────────────────────────────────┐
 │  RELAY SERVICE (~200 LOC Bun)                                │
 │  Hosted: Fly.io / Hetzner / self-hosted                      │
 │                                                              │
 │  WebSocket handler:                                          │
 │    - Validates HMAC token (shared_secret, timestamp, ±30 s)  │
 │    - Assigns connection to namespace (hash of shared_secret) │
 │    - Broadcasts incoming frames to all OTHER connections      │
 │      in the same namespace                                   │
 │    - Persists frame to relay_db.sqlite                       │
 │      (frame_id, namespace, pseudonym, chat_id,               │
 │       encrypted_body, ts)                                    │
 │                                                              │
 │  HTTP handler GET /pull?since=frame_id&ns=<namespace_hash>   │
 │    - Returns frames in ns with frame_id > since              │
 │    - Auth: same HMAC token                                   │
 │                                                              │
 └──────────────────────────────────────────────────────────────┘
```

### 7.2 Protocol Frames (JSON over WebSocket)

```typescript
// Outbound from client to relay (publish)
type PublishFrame = {
  kind: "msg" | "ask" | "answer" | "presence" | "reaction";
  namespace: string;         // SHA-256(shared_secret) — for relay routing
  from: string;              // pseudonym
  chat_id?: string;          // for msg
  ask_id?: number;           // for ask/answer (relay-assigned global_id)
  body: string;              // plaintext (Phase 1) or ciphertext (Phase 2)
  machine_id: string;        // for presence annotations in discover
  ts: number;                // client Unix ms
  local_id: number;          // client's local SQLite row id (for dedup)
};

// Relay assigns a monotonic frame_id and broadcasts:
type RelayFrame = PublishFrame & {
  frame_id: number;          // relay-assigned, globally monotonic
  relay_ts: number;          // relay receive time
};

// Pull response
type PullResponse = {
  frames: RelayFrame[];
  next_since: number;
};
```

### 7.3 ID Strategy

The primary key collision between machines is resolved by:

- `messages` table gains `remote_id TEXT UNIQUE` column (migration version 3).
  Value is `"{frame_id}"` when a message arrived from the relay, NULL for
  local-origin messages not yet synced.
- `asks` table gains `remote_id TEXT UNIQUE` similarly.
- When the relay client receives a frame for a message already in local SQLite
  (same `from` + `local_id` + `ts`), it updates `remote_id` rather than
  inserting a duplicate. This makes the sync idempotent.
- The relay's durable store uses `frame_id` as the primary key — no collision.

This is a non-breaking migration: existing rows have `remote_id = NULL`. Local-only
mode never writes `remote_id`.

### 7.4 Machine Identity

Add `~/.claudetalk/machine.json`:

```json
{
  "machine_id": "8f3a...",       // random UUID, generated once
  "hostname": "my-laptop",       // display hint, not used for routing
  "created_at": "2026-05-17T..."
}
```

The `instances` table gains a `machine_id TEXT` column. `discover` shows
`(hostname)` as a suffix on pseudonyms when multiple machines share the same
pseudonym. This is additive and non-breaking.

### 7.5 Auth Token

```
token = base64url(
  pseudonym_hash (32 bytes) ||
  timestamp_seconds (8 bytes, big-endian) ||
  HMAC-SHA256(shared_secret, pseudonym_hash || timestamp_seconds)  (32 bytes)
)
```

Relay validates: timestamp within ±30 s of relay time (anti-replay). HMAC
checks membership in the namespace. Token is sent as `Authorization: Bearer <token>`
on every WebSocket upgrade and every REST request.

### 7.6 Offline Behavior

The `RelayClient` maintains an outbound queue (in-memory array of `PublishFrame`s).
On disconnect, writes continue to local SQLite immediately; the outbound queue
accumulates. On reconnect: flush the outbound queue in order, then pull from the
relay since the last received `frame_id`.

Local SQLite is always the source of truth for tools and hooks. The relay is a
delivery mechanism, not a database.

### 7.7 Encryption (Phase 2)

Phase 1 is TLS-only. Phase 2 adds client-side encryption using Web Crypto:

```typescript
// Key derivation (once, from shared_secret)
const encKey = await crypto.subtle.importKey(
  "raw",
  await crypto.subtle.deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: new Uint8Array(32), info: new TextEncoder().encode("ct-messages-v1") },
    await crypto.subtle.importKey("raw", sharedSecretBytes, "HKDF", false, ["deriveBits"]),
    256
  ),
  { name: "AES-GCM" },
  false,
  ["encrypt", "decrypt"]
);

// Encrypt before publish
const iv = crypto.getRandomValues(new Uint8Array(12));
const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, encKey, new TextEncoder().encode(body));
```

The relay stores the ciphertext. Receiving clients decrypt. The relay schema
adds a `encrypted: boolean` column — clients reject frames where `encrypted`
does not match their config.

---

## 8. Phase Rollout

### Phase N0 — Foundation (no network yet)

**What ships:**
- `machine.json` generation on first run.
- `instances.machine_id` migration (version 3, additive).
- `messages.remote_id` and `asks.remote_id` migration (version 3, same migration).
- `~/.claudetalk/network.json` config file structure (loaded by server, ignored
  if absent — preserves local-only behavior 100%).
- Two new CLI commands: `claudetalk network init` (generates shared secret, writes
  config) and `claudetalk network status` (shows relay URL, connected machines).
- No relay, no sync. All existing tests pass unchanged.

**Success metrics:**
- All 154+ existing tests still pass.
- `doctor` command reports correct machine_id.
- `network.json` absent → server behavior byte-for-byte identical to 0.4.x.

**Kill criterion:** if `machine.json` generation conflicts with existing
`~/.claudetalk` file permissions on any platform, block on this before Phase 1.

---

### Phase N1 — Relay + Basic Sync (no encryption)

**What ships:**
- `src/relay-client.ts` — WebSocket client with reconnect, outbound queue, and
  pull-on-connect.
- Relay service (`relay/src/index.ts`) — ~200 LOC Bun WS server.
- Relay deployment configuration (Fly.io `fly.toml` or Docker Compose for
  self-hosting).
- `claudetalk network connect` tool (new MCP tool) — validates relay connectivity.
- `discover` tool shows `(hostname)` annotation for remote peers.
- `peers --remote` flag lists instances seen across machines.

**Dependencies:**
- Phase N0 complete (machine_id, remote_id columns, config structure).

**Success metrics:**
- Two machines both online: message sent from Machine 1 appears in Machine 2's
  `inbox` within 500 ms.
- Machine 2 offline for 1 h then reconnects: all messages sent during absence
  appear within 10 s of reconnect.
- Machine 1 sends messages with relay down: messages persist locally and sync
  when relay returns.
- Relay crash: both machines continue local-only mode silently.

**Kill criterion:** if the outbound queue grows unbounded (relay stays down for
days), cap queue at 1,000 frames and log a warning. Older frames drop silently
— the pull-on-connect catch-up from relay durable store still delivers them.

---

### Phase N2 — E2E Encryption

**What ships:**
- HKDF key derivation from shared secret (Web Crypto, no native deps).
- Encrypt-before-publish, decrypt-after-receive in `RelayClient`.
- `messages.encrypted` flag in local SQLite.
- Relay schema: `encrypted BOOLEAN NOT NULL DEFAULT 0`.
- `claudetalk doctor` warns if relay URL is set but encryption is disabled.

**Dependencies:**
- Phase N1 complete (relay working).

**Success metrics:**
- Wireshark on the relay host shows message bodies as base64 ciphertext.
- Messages decrypt correctly across machines with the same shared secret.
- Machines with different shared secrets cannot decrypt each other's messages
  (they are in different namespaces and the relay doesn't deliver the frames
  cross-namespace).

**Kill criterion:** if Web Crypto HKDF behavior is inconsistent across Bun
versions on different machines, fall back to a pure-TS HKDF implementation
(`@noble/hashes`).

---

### Phase N3 — UX Polish and Hardening

**What ships:**
- `claudetalk auth add-machine` prints a one-liner or QR code that the user
  runs on a second machine (embeds relay URL + shared secret in a safe format).
- Relay monitoring: relay exposes `/health` and `/metrics` (frame count, connected
  clients, error rate).
- Rate limiting in the relay (max frames per second per namespace).
- WAL GC extended to purge `remote_id`-tagged rows that are older than 30 days
  (already handled by `gc` command, just needs awareness of `remote_id`).
- GitHub OAuth opt-in (replace shared-secret with GitHub device flow for users
  who prefer it).

**Dependencies:**
- Phase N2 complete.

**Success metrics:**
- Bootstrap from zero to two machines seeing each other in under 2 minutes.
- Relay `/health` returns 200 within 1 s.
- `claudetalk doctor` reports relay latency and frame-sync status.

---

## 9. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ID collision between machines corrupts local DB | Medium | High | Phase N0 adds `remote_id` dedup; relay assigns globally-unique frame IDs; sync is idempotent |
| Relay downtime blocks cross-machine messaging | Medium | Medium | Local-only mode always works; relay is single point for *network* delivery only, not for operation |
| Shared secret leak (dotfiles repo, iCloud sync) | Low | High | Document that `network.json` must not be committed to public repos; add `.gitignore` entry automatically on `network init` |
| Bun WebSocket reconnect logic has edge cases | Medium | Low | Use 100 ms → 30 s exponential back-off with jitter; cap at 30 s; test with network partition scenarios |
| Turso / Cloudflare pricing change (if A adopted) | Low | Medium | Mitigation for Architecture A only; Architecture B (relay) is self-hosted and immune |
| Migration version 3 breaks existing installations | Low | High | Migration uses `addColumnIfMissing` pattern already in use; `remote_id` is nullable; existing rows are unaffected |
| Relay SQLite grows unbounded | Low | Low | Relay-side GC job prunes frames older than configurable retention (default 7 days); `frame_id` cursor per namespace ensures catch-up still works for recent offline periods |
| Clock skew between machines invalidates HMAC tokens | Low | Medium | ±30 s window; most machines use NTP; document requirement |
| Web Crypto HKDF inconsistency across Bun versions | Low | Medium | Integration test: derive the same key on two machines and encrypt/decrypt a known payload; fail CI if divergence detected |
| User has no place to deploy relay | Medium | Medium | Provide one-click Fly.io deploy button; document Hetzner and Cloudflare Workers alternatives; allow Architecture A as fallback |

---

## 10. Observability

The existing audit log (`tool_calls` table) captures every MCP tool invocation
with latency. Extend it for network mode:

- Add `relay_latency_ms` to the audit log for frames that arrive from the relay.
- `claudetalk metrics` gains a `--network` flag showing: frames sent, frames
  received, relay round-trip latency p50/p95, reconnect count, queue depth.
- Relay exposes `/metrics` with Prometheus-compatible text: `claudetalk_relay_connected_clients`, `claudetalk_relay_frames_total`, `claudetalk_relay_errors_total`.
- `claudetalk doctor` pings the relay and reports latency.

---

## 11. Rollback Strategy

Each phase is independently rollback-safe:

- **Phase N0 rollback:** delete `machine.json`; drop `machine_id` column (SQLite
  does not support DROP COLUMN before 3.35; alternative: ignore the column, which
  is zero-cost). Downgrade to 0.4.x binary — local-only mode unaffected.
- **Phase N1 rollback:** remove `relay_url` from `network.json`; `RelayClient`
  never starts. Local-only mode resumes immediately. Relay can be stopped without
  client-side changes.
- **Phase N2 rollback:** set `encrypt: false` in `network.json`; messages sent
  in plaintext again. Previously-encrypted messages in local SQLite remain
  readable by machines that still have the key.
- Schema rollback: all new columns are nullable or have defaults. Downgrading
  the binary and ignoring new columns is safe; the migrated schema is a strict
  superset of the 0.4.x schema.

---

## 12. What Stays Unchanged

This is the backwards-compatibility contract. Every item listed here must pass
its existing tests unchanged after every phase:

- `pseudonymFor()` — same inputs, same outputs.
- All 18 existing tools — same API surface, same behavior when `network.json` absent.
- `hooks/check-inbox.ts` — reads local SQLite, emits the same summarise() output.
- `~/.claudetalk/db.sqlite` path and WAL configuration.
- `claudetalk install/uninstall/doctor/tail/web/log/gc/export/metrics/replay`.
- The `claude plugin install claudetalk@claudetalk` flow.
- Local-machine discover/ask/answer/chat/groupchat between sessions on the same
  machine.

---

## 13. Open Questions — Decisions Required Before Implementation

The following are design forks where the right answer depends on your preferences.
They are listed in priority order (the first two block Phase N0).

**Q1 — Primary key migration strategy.**
The `remote_id TEXT UNIQUE` approach keeps local autoincrement IDs unchanged and
adds a relay-assigned global ID as a secondary key. The alternative is to switch
`messages.id` to TEXT (UUID) from the start. The UUID approach is cleaner long-term
but requires touching every SQL query. The `remote_id` approach is less invasive
but adds complexity to the sync dedup logic. Which do you prefer?

**Q2 — Relay hosting.**
Do you want to self-host the relay (Hetzner/Fly.io), or does Architecture A
(Turso-hosted SQL, no relay binary) become the recommendation if you want
zero-server? The relay approach is ~$2/month and gives real-time delivery.
Turso gives zero infra cost but adds poll latency. If you want to start with
Turso and migrate to relay later, that is possible but adds a phase.

**Q3 — Shared secret distribution.**
How will you distribute the shared secret to additional machines?
Options: (a) manual copy-paste of the base64 secret, (b) `claudetalk auth add-machine`
prints a one-time URL that the second machine fetches over HTTPS from the relay,
(c) iCloud Keychain / 1Password CLI integration. Option (b) requires the relay
to be deployed first (chicken-and-egg). Option (a) is the simplest but most
error-prone.

**Q4 — Pseudonym collision policy.**
Do you want `~/code/myapp` on laptop and `~/code/myapp` on desktop to be the
same pseudonym (continuity model) or different pseudonyms (parallelism model)?
The design above defaults to continuity (same pseudonym, machine annotations in
`discover`). If you want parallelism, pseudonyms must be namespaced by machine
ID — which is a breaking change to all existing message routing.

**Q5 — Retention on the relay.**
How long should the relay keep frames for catch-up? 7 days means a machine
offline for a week can still catch up. 30 days means a month. Longer retention
means larger relay SQLite. For personal use, 7 days is likely sufficient (you
would not leave a machine offline for longer and expect a full replay).

**Q6 — Encryption default.**
Should E2E encryption be on-by-default in Phase N2, requiring explicit opt-out?
Or off-by-default, requiring opt-in? Given that messages contain code and file
paths, on-by-default is the safer choice. But it means key management must be
airtight before shipping Phase N2. Off-by-default gives more time to test the
encryption path.

**Q7 — GitHub OAuth as alternative auth.**
Is GitHub OAuth (device flow) worth implementing as an alternative to shared
secret in Phase N3? It would allow `claudetalk auth login` without manually
distributing secrets. But it adds a dependency on GitHub availability and
requires the relay to call GitHub's API. Relevant only if you ever want a
frictionless setup for a non-technical second user.

---

## 14. Evidence Citations

- [Strong] SQLite WAL mode supports concurrent readers and a single writer;
  this is the foundation of the existing multi-session local architecture and
  extends naturally to a local-mirror model.
- [Moderate] WebSocket-based relay with persistent connections gives sub-200 ms
  delivery; this is the standard architecture for chat systems (Slack, Discord
  internal architecture) and is well-validated at much larger scale.
- [Strong] Defense-in-depth requires encryption even for personal relays;
  OWASP ASVS L1 requires transport encryption; L2 adds storage encryption. The
  relay is an additional trust boundary that must be treated as potentially
  compromised.
- [Moderate] Append-only message logs map to CRDT last-writer-wins with
  timestamps; the total ordering is achievable via relay-assigned monotonic IDs
  without full CRDT complexity (Kleppmann, "Designing Data-Intensive Applications").
- [Promising] Iroh (QUIC P2P with relay fallback) is an architecturally
  elegant fit but has limited production deployments as of 2025 and unclear
  Bun compatibility; flagged as a candidate for Phase N3 if WS relay proves
  operationally inconvenient.
- [Opinion] The "it's all yours anyway" trust model (TLS only) is acceptable
  for Phase N1 given the personal-use scope, but E2E encryption in Phase N2
  is a firm commitment, not optional, given the message content sensitivity.
