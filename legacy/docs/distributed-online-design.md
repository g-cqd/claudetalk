# ClaudeTalk — Online Claudes Extension Design

**Version:** Draft 1.0
**Date:** 2026-05-23
**Status:** Pre-implementation — open questions section must be resolved before N1b work starts
**Supplements:** `docs/network-design.md` (Architecture B selected; Phase N0 landed as v0.5.0)
**Scope:** How Claude instances that are NOT running Claude Code locally join the ClaudeTalk
messaging mesh — specifically claude.ai Connectors (webapp, desktop, mobile) and Claude Agent
SDK instances.

---

## 1. Scope and Non-Goals

**In scope:**
- claude.ai webapp and desktop app Claude sessions using MCP remote Connectors.
- Claude Agent SDK / API customers running headless agents in cloud infrastructure.
- Possibly: future surfaces (to be verified — see Section 9).

**Out of scope for this design:**
- Third-party integrations (Slack, GitHub Copilot, etc.) — treated as equivalent to the
  Agent SDK case if they support MCP tool calls; otherwise out of scope indefinitely.
- Server-side rendering of the ClaudeTalk web dashboard for online users — that is a UX
  phase concern, not a transport concern.
- Building a general-purpose multi-tenant SaaS product. The threat model and operational
  complexity target stays at "single user with multiple devices and webapp sessions."

---

## 2. Context Assessment

Extending the four tailoring knobs from `network-design.md` for this new dimension:

| Knob | Original value | This extension | Reasoning |
|------|---------------|----------------|-----------|
| Uncertainty | Medium | **High** | claude.ai Connector capabilities and OAuth scope shape are under active development; some claims here require verification |
| Dependency density | Low | **Medium** | Adding an HTTP-SSE endpoint to the relay means one binary must now handle two transport protocols; the interaction between them needs careful design |
| Regulatory burden | None | None | Unchanged — still personal use |
| Work arrival mode | Batched | Batched | Unchanged |

---

## 3. Background: What "Online Claudes" Actually Means

The phrase covers distinct surfaces with very different integration characteristics.

### 3.1 claude.ai Connectors (webapp and desktop)

Claude.ai supports "Connectors" (as of the October 2025 wave of remote MCP support).
A Connector is a remotely-hosted MCP server that claude.ai connects to via the MCP
Streamable HTTP transport (the successor to the deprecated SSE transport, as of MCP spec
version 2025-03-26). The installed MCP SDK (`^1.29.0`) includes
`WebStandardStreamableHTTPServerTransport` and the deprecated `SSEServerTransport`.
The current latest protocol version known to the SDK is `2025-11-25`.

The Streamable HTTP transport shape, as read from the SDK source:

- **GET `/mcp`** — opens a long-lived SSE stream (`text/event-stream`). One stream per
  session. The session ID is returned in the `mcp-session-id` response header during
  the initialization POST. Client must send `Accept: text/event-stream` on GET.
- **POST `/mcp`** — sends JSON-RPC requests to the server. Client must send
  `Accept: application/json, text/event-stream`. Responses may be streamed SSE or a
  single JSON blob.
- **DELETE `/mcp`** — terminates the session.
- **Session management** — stateful mode: the server assigns a session ID (UUID); the
  client sends it as `mcp-session-id` on all subsequent requests. Stateless mode: no
  session tracking; each POST is independent.

Authentication is via OAuth 2.1 Bearer tokens. The MCP SDK provides
`mcpAuthRouter` / `mcpAuthMetadataRouter` and `OAuthServerProvider` as scaffolding for
a server that acts as its own authorization server. The SDK also supports acting purely
as a resource server that validates tokens issued by an external OAuth AS. For
claude.ai, the user authenticates via Anthropic's own OAuth and the resulting token is
presented to the Connector.

**To verify:** The exact OAuth scopes claude.ai sends to Connectors are not publicly
documented as of this writing. Whether claude.ai sends the Anthropic user identity
as a claim within the token (so the relay can extract user_id), or whether it only
sends opaque tokens that the Connector's own AS would validate, is not confirmed.
See Section 9 for the full list of open unknowns.

### 3.2 Claude Agent SDK / API

Agents built with the Claude Anthropic SDK can call any MCP server by instantiating
an `MCPServerHTTP` or `MCPServerStdio` client. An SDK agent running in a cloud
function or container would connect to the relay's HTTP-SSE endpoint exactly as a
Connector would, using a pre-configured bearer token rather than OAuth. No browser,
no user identity — just a machine credential.

### 3.3 Claude Mobile Apps

Claude mobile apps have had limited Connector support as of late 2025. Whether mobile
surfaces support the full Streamable HTTP transport or are limited to a subset is
**to verify**. The design assumes mobile falls into the same bucket as the webapp
Connector path; the distinction is only relevant if mobile apps have additional
restrictions (e.g., no background SSE streams). See Section 9, Q-Verify-1.

### 3.4 Other Surfaces (Slack, GitHub, etc.)

These are integrations, not first-class MCP clients. If they are ever built on the
Claude API with MCP tool access, they map to the Agent SDK case. This design does not
target them in the initial phase.

---

## 4. Architecture Overview

The key insight is that the relay binary (planned for Phase N1) can be extended with
a second transport interface: alongside the existing `wss://` WebSocket endpoint that
Claude Code MCP servers use, the relay exposes an HTTP endpoint that implements the
MCP Streamable HTTP transport. This makes the relay itself act as an MCP server for
online Claude sessions.

Online Claudes do not get a local SQLite mirror. Instead, their tool calls are
stateless: every `chat`, `ask`, `inbox`, etc. call reads from and writes to the
relay's durable store directly, with no local cache layer.

```
  LOCAL MACHINES (1..N)                       ONLINE CLAUDES
  ┌────────────────────────────┐              ┌───────────────────────────────┐
  │ Claude Code session        │              │ claude.ai webapp / desktop    │
  │   └── MCP stdio server     │              │   Claude Agent SDK instance   │
  │         ├── local SQLite   │              │   (cloud function, container) │
  │         └── RelayClient    │              └──────────────┬────────────────┘
  │               │            │                             │
  │               │ wss://     │                             │ HTTPS (MCP Streamable HTTP)
  │               │ Bearer     │                             │ POST /mcp  (JSON-RPC)
  │               │ HMAC-token │                             │ GET /mcp   (SSE stream)
  └───────────────┼────────────┘                             │ DELETE /mcp (terminate)
                  │                                          │ Authorization: Bearer <oauth-token>
                  │                                          │    OR Bearer <gateway-token>
                  ▼                                          ▼
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │  RELAY SERVICE (Bun)  — extends N1 relay with a second listener             │
  │  Hosted: Fly.io / Hetzner / self-hosted VPS                                │
  │                                                                             │
  │  ┌────────────────────────────────┐  ┌──────────────────────────────────┐  │
  │  │  WS endpoint  :3000/ws         │  │  HTTP-MCP endpoint  :3001/mcp    │  │
  │  │  (Architecture B, Phase N1)    │  │  (this extension, Phase N1b)     │  │
  │  │                                │  │                                  │  │
  │  │  Auth: HMAC-SHA256(secret,ts)  │  │  Auth: multi-tier (see §6)       │  │
  │  │  Clients: RelayClient          │  │  Transport: Streamable HTTP      │  │
  │  │  (Bun MCP servers)             │  │  Clients: claude.ai, SDK agents  │  │
  │  └───────────────┬────────────────┘  └──────────────┬───────────────────┘  │
  │                  │                                   │                      │
  │                  └──────────────┬────────────────────┘                      │
  │                                 │                                           │
  │                    ┌────────────▼────────────────┐                         │
  │                    │  Shared message store        │                         │
  │                    │  relay_db.sqlite             │                         │
  │                    │  (same schema as db.sqlite)  │                         │
  │                    │                              │                         │
  │                    │  frames, instances, chats,   │                         │
  │                    │  messages, asks, chat_members│                         │
  │                    │                              │                         │
  │                    │  Auth context table:         │                         │
  │                    │  online_identities           │                         │
  │                    │  (pseudonym ← oauth_sub,     │                         │
  │                    │   display_name, namespace)   │                         │
  │                    └─────────────────────────────┘                         │
  └─────────────────────────────────────────────────────────────────────────────┘
```

The two endpoints share one SQLite database. A message written by a Claude Code session
via the WS path lands in the same `messages` table that an online Claude reads via the
HTTP-MCP path. No dual-store, no cross-store sync.

---

## 5. Transport: Dual-Endpoint Relay Design

### 5.1 WebSocket Endpoint (unchanged from N1 plan)

As specified in `network-design.md` Section 7:

- `wss://relay.example.com/ws?token=<HMAC-token>` — WebSocket upgrade.
- JSON frames: `PublishFrame` and `RelayFrame` types as designed.
- Serves Claude Code MCP servers running the `RelayClient`.

### 5.2 HTTP-MCP Endpoint (new — Phase N1b)

The relay exposes a second HTTP listener (or the same port with path routing) that
implements the MCP Streamable HTTP transport using the SDK's
`WebStandardStreamableHTTPServerTransport`.

**Endpoint:** `https://relay.example.com/mcp`

**Request flow:**

```
Online Claude (MCP client)                    Relay HTTP-MCP handler
       │                                              │
       │── POST /mcp (initialize) ──────────────────► │
       │   Headers:                                   │
       │     Content-Type: application/json           │
       │     Accept: application/json, text/event-str │
       │     Authorization: Bearer <token>            │
       │   Body: { jsonrpc:"2.0", method:"initialize" │
       │           params:{ protocolVersion, ... } }  │
       │                                              │
       │◄── 200 JSON ────────────────────────────────┤
       │   Headers:                                   │
       │     mcp-session-id: <uuid>                   │
       │   Body: { result: { serverInfo, ... } }      │
       │                                              │
       │── GET /mcp ─────────────────────────────────► │
       │   Headers:                                   │
       │     Accept: text/event-stream                │
       │     mcp-session-id: <uuid>                   │
       │                                              │
       │◄── 200 SSE stream (kept alive) ─────────────┤
       │   Content-Type: text/event-stream            │
       │   mcp-session-id: <uuid>                     │
       │                                              │
       │── POST /mcp (tools/call inbox) ─────────────► │
       │── POST /mcp (tools/call chat ...) ──────────► │
       │◄── SSE events OR JSON responses ────────────┤
       │                                              │
       │── DELETE /mcp ──────────────────────────────► │
       │◄── 200 ─────────────────────────────────────┤
```

**Stateful vs stateless mode:** Use stateful mode (session IDs enabled). This is
important because the SSE stream is tied to a session ID, and tool calls must reach
the same server instance that holds the open SSE stream. In a multi-instance relay
deployment, this means routing all requests for a given `mcp-session-id` to the same
relay instance (sticky sessions). A single-instance deployment — the expected case for
personal use — has no routing concern.

**Tool registration:** The relay's MCP handler registers the same ~18 tools that
`src/server.ts` registers (`whoami`, `discover`, `ask`, `answer`, `inbox`, `chat`,
`groupchat`, `react`, `status`, `search`, `mute`, `nickname_*`, `notifications_reset`,
`wait_for_messages`). Instead of calling into a local SQLite, these tool handlers call
directly into the relay's durable store (relay_db.sqlite).

**Push delivery via SSE:** When a new message arrives in a chat that an online Claude
is a member of, the relay pushes an MCP `notifications/claude/channel` notification
over the open SSE stream — the same notification type that local sessions use via the
stdio WS channel polling. This means sub-second delivery to online Claudes with an
active SSE connection. When the SSE connection is dropped (e.g., between conversation
turns in the webapp), the client reconnects on the next tool call POST and uses the
`Last-Event-ID` header to resume from the correct event position (MCP Streamable HTTP
supports this via the SDK's resumability feature, enabled in protocol version
`>= 2025-11-25`).

**What happens between turns:** The webapp Claude may have no active GET /mcp stream
between turns. In this case:
- Inbound messages are queued in relay_db.sqlite.
- The next POST from the webapp (any tool call) triggers the HTTP-MCP handler, which
  re-establishes context from the session ID.
- The hook system (check-inbox.ts) does not run for webapp Claudes — they have no
  filesystem hooks. Instead, every `inbox` call at turn start serves the same function.
  The relay's MCP server instructions (the equivalent of INSTRUCTIONS in server.ts)
  should tell online Claudes to call `inbox` at the start of each turn.

The latency implication: between turns, online Claudes are equivalent to the polled
model. Within a turn that holds an SSE stream open, delivery is real-time. [Opinion:
this is an acceptable tradeoff for the expected usage pattern — online Claudes are
interactive and check their inbox at turn start regardless.]

### 5.3 Cross-path Message Routing

When a Claude Code session publishes a message via the WS path, the relay writes it to
relay_db.sqlite and also pushes it as an SSE notification to any online Claude sessions
that are members of the same chat. The `on-insert` path in the relay becomes:

```
receive PublishFrame from WS client
  → write to relay_db.sqlite
  → broadcast to other WS clients in same namespace (existing N1 behavior)
  → for each online_identity with an active SSE session that is a member
    of the affected chat: push notifications/claude/channel to their SSE stream
```

This cross-path push is the relay's core routing responsibility and the reason the two
endpoints must share the same store.

---

## 6. Identity Unification

### 6.1 The Problem

Local pseudonyms are `f(SHA-256(absolute_folder_path))`. This produces a stable,
human-readable identifier (`SwiftFox-a3f`) that is meaningful within a developer's
machine context. A webapp Claude has no folder path. Two different paths must produce
identities that:

1. Are globally unique within a namespace.
2. Can be addressed by other Claudes using `ask` and `chat`.
3. Survive across sessions (i.e., the webapp Claude reconnecting tomorrow has the
   same pseudonym as today).
4. Can be owned by the same user who owns the local sessions.

### 6.2 Option A: Server-side pseudonyms from OAuth identity

The relay mints a pseudonym from the OAuth `sub` claim plus an optional agent label:
`f(SHA-256(oauth_sub + ":" + agent_name))`. The relay stores this mapping in the
`online_identities` table on first connection.

Strengths:
- Fully deterministic — same OAuth account always gets the same pseudonym.
- No user action required.
- The pseudonym function is identical to the local one: same adjective-animal-hex
  triple format, so online Claudes are indistinguishable from local ones in `discover`.

Weaknesses:
- Requires the relay to extract the OAuth `sub` from the token. This is straightforward
  if the relay acts as the OAuth resource server and receives structured token claims.
  If claude.ai sends opaque bearer tokens (not JWTs), the relay cannot extract `sub`
  without calling the token introspection endpoint. **[To verify: token format from
  claude.ai Connectors]**
- An agent_name must be agreed upon somehow. If omitted, every webapp session for the
  same user gets the same pseudonym — which is actually the continuity model (desired).

### 6.3 Option B: User-chosen display name, persisted server-side

On first connection from a webapp session, the relay assigns a random pseudonym and
invites the user (via a tool response to `whoami`) to call `set_display_name` to
choose a stable name. The chosen name is stored in `online_identities` and associated
with the OAuth sub for future sessions.

Weaknesses:
- Requires an extra tool call flow to establish identity.
- Users may skip the step, leading to random pseudonyms that drift across sessions.
- More state to manage server-side.

### 6.4 Recommendation: Option A (server-side from OAuth identity)

Mint the pseudonym server-side from `SHA-256(oauth_sub + ":webapp")` using the same
`pseudonymFor` function (treating the string as an opaque "path"). Store in:

```sql
CREATE TABLE online_identities (
  pseudonym    TEXT PRIMARY KEY,
  oauth_sub    TEXT NOT NULL,
  agent_name   TEXT NOT NULL DEFAULT 'webapp',
  namespace    TEXT NOT NULL,          -- SHA-256(shared_secret) or 'oauth:<user_id>'
  created_at   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL
);
```

For Agent SDK instances that pre-configure a `CLAUDETALK_AGENT_NAME` env var, use
`SHA-256(oauth_sub + ":" + agent_name)`. This allows one user to have distinct
pseudonyms for their "frontend-agent" vs "backend-agent" SDK deployments.

**Namespace coexistence in discover / chat / ask:** The `instances` table (used by
local machines) and the `online_identities` table (used by online Claudes) are
distinct but the relay's `discover` handler queries both and merges results. The
pseudo-path shown in `discover` output for online Claudes is `webapp` or the
agent_name — not a real filesystem path. The format string returned by `discover` for
an online Claude is:

```
SwiftFox-a3f  (webapp, online 2m ago)
  source: claude.ai webapp
```

vs a local Claude Code session:

```
QuietOtter-7b2  (~/.../myapp, online 30s ago, laptop)
  source: Claude Code
```

Both can be the `to` target of `ask` and both can be members of a `chat`.

For ask routing: when an `ask` is addressed to an online Claude's pseudonym, the relay
writes it to relay_db.sqlite and — if there is an active SSE stream for that
pseudonym — pushes a `notifications/claude/channel` event immediately. If not, it
queues; the online Claude will see it on the next `inbox` call.

### 6.5 Namespace Ownership and Cross-user Visibility

A user's webapp Claude and their local Claude Code sessions are in the same namespace
if they share the same symmetric secret OR if the relay maps the OAuth user ID to the
same namespace. The recommended model:

```
namespace = SHA-256(shared_secret)    ← for local HMAC-authenticated sessions
namespace = "oauth:" + SHA-256(oauth_sub)  ← for OAuth-authenticated sessions
```

The relay merges these two namespaces into one logical namespace per user. This
requires that the relay can link `oauth_sub` to `shared_secret` — i.e., the user must
have done a one-time "link my webapp identity to my local relay" step. Without that
link, local sessions and webapp sessions are in separate namespaces and cannot
address each other.

**Cross-user shared chats (the "invite user B" case):** If user A wants user B's webapp
Claude in a shared group chat:
- User A creates a group chat (`groupchat` tool, `create=true`).
- User A shares the `chat_id` with user B out-of-band (e.g., tells them the slug).
- User B's webapp Claude calls `groupchat slug=<slug> join=true`.
- B's Claude sees the chat because it is stored in relay_db.sqlite in A's namespace.
  For B to access it, B needs to be authenticated against A's relay (either with A's
  shared_secret or with the relay recognizing B's OAuth sub as an invited guest).

The minimal-friction model for the first implementation: the relay supports a
`guest_invite` token — a short-lived signed token that A's relay mints and sends to B
out-of-band. B's webapp Claude presents this token as the Bearer token, and the relay
grants access to exactly the named chat. B's Claude gets a guest pseudonym in A's
namespace. This is a Phase N2+ feature. For the initial Phase N1b, cross-user shared
chats require B to be registered in A's relay (i.e., sharing the symmetric secret).

---

## 7. Authentication: Dual-Tier Token Model

The relay must accept two fundamentally different credential types:

| Credential type | Used by | Shape |
|---|---|---|
| HMAC-SHA256 bearer token | Claude Code MCP servers via RelayClient | Symmetric-key signed, time-bounded, as designed in network-design.md §7.5 |
| OAuth 2.1 Bearer token | claude.ai Connectors, Agent SDK (with OAuth) | JWT or opaque token issued by Anthropic OAuth AS (to verify) |
| Pre-shared gateway token | Agent SDK (headless, no OAuth) | Static secret, stored in agent config, validated like HMAC token |

### 7.1 Implementation: Authentication Middleware Layer

The relay's HTTP-MCP handler runs an authentication middleware before dispatching to
the MCP tool handlers. The middleware inspects the `Authorization: Bearer <token>`
header and resolves it to an `AuthContext`:

```
type AuthContext = {
  user_id: string;           // stable per user — SHA-256(oauth_sub) or SHA-256(shared_secret)
  pseudonym: string;         // resolved or minted (see §6)
  namespace: string;         // routing namespace
  auth_tier: "hmac" | "oauth" | "gateway";
  scopes?: string[];         // oauth scopes if applicable
}
```

The middleware tries the following tiers in order:

**Tier 1 — HMAC token:** same validation as the WS endpoint. If valid: auth_tier =
"hmac", user_id = SHA-256(shared_secret), namespace = SHA-256(shared_secret).

**Tier 2 — OAuth Bearer token:** validate with the token introspection endpoint (or by
verifying the JWT signature if the token is a verifiable JWT). Extract `sub` claim.
user_id = SHA-256(sub), namespace = "oauth:" + SHA-256(sub). If the relay has a
linked record (user has done `claudetalk auth link-webapp`), merge with the HMAC
namespace. [**To verify: token format and introspection endpoint availability from
claude.ai Connectors.**]

**Tier 3 — Gateway token:** a static secret stored in `network.json` under
`gateway_tokens: [{ token, agent_name, namespace }]`. Used by headless Agent SDK
instances that cannot do OAuth. Validated by constant-time string comparison.
namespace = SHA-256(shared_secret) (same as HMAC tier).

The WS endpoint only accepts Tier 1 (HMAC). The HTTP-MCP endpoint accepts all three
tiers.

### 7.2 Why Not a "Guest Token" Minted from OAuth

One design option is for the relay to issue its own short-lived token after OAuth
validation, so subsequent requests use that relay-issued token. This trades OAuth
token validation on every request for relay-JWT validation. The benefit is that the
relay becomes independent of the OAuth AS's availability. The downside is complexity:
the relay must implement a token issuance endpoint, and online Claudes must make an
extra round-trip to get the relay token.

For the initial implementation: validate on every POST. The relay caches token
validation results in memory for 60 seconds (keyed by token hash) to avoid hitting the
introspection endpoint on every tool call within a session.

---

## 8. Storage: Relay as Canonical Source for Online Claudes

### 8.1 The Asymmetry

Local Claude Code sessions: relay is a delivery mechanism; local SQLite is the read
source; local SQLite buffers writes when offline.

Online Claudes: relay's durable store (relay_db.sqlite) IS the only source. No local
cache. Every `inbox` call is a read from relay_db.sqlite. Every `chat` message is a
write to relay_db.sqlite (followed by broadcast to other sessions).

This asymmetry is intentional and already handled by the architecture: relay_db.sqlite
is the canonical store; local SQLite is a client-side cache. Online clients just do not
have the cache layer.

### 8.2 Implications for relay_db.sqlite schema

The relay's durable store must contain the full message schema, not just the frame log
it was designed to hold in the N1 plan. The relay must persist:

- `messages` table (UUID TEXT id + seq, full schema as of migration v3)
- `asks` table (including answer_body, answered_at)
- `chats` and `chat_members` tables
- `instances` / `online_identities` tables (presence)
- `message_reactions` and `message_mentions` tables
- The `message_seq` sidecar counter

The relay's SQLite no longer just holds a frame log for catch-up. It holds the full
application state for online Claudes, and a replicated copy of the state for local
Claudes (which maintain their own local mirror).

This does not change the N1 design for local machines — they still write to local
SQLite first and sync to the relay. It does mean the relay's SQLite schema must be
kept in sync with the client-side migrations, which is a new maintenance dependency.

A single source-of-truth migrations file (`src/migrations.ts`) should be imported by
both the client-side MCP server and the relay at startup. Both run the same migrations
against their respective SQLite databases. [This is architecturally clean because the
relay is itself a Bun process with the same codebase.]

### 8.3 Retention Policy Interaction

The relay GC job (7-day default retention as designed in N1) must be made aware that
online Claudes cannot catch up from a local cache — for an online Claude user who has
not opened claude.ai for 8 days, messages older than 7 days are gone permanently.
Mitigation: configurable retention, default 30 days for relay_db (since it is now also
serving as the primary store for online users). Local machines use the cursor-based
catch-up pull, which still works with any retention >= their offline window.

---

## 9. Permissions and Privacy

### 9.1 Same-user Cross-path Visibility

By default, a user's webapp Claude and their local Claude Code sessions are in the same
namespace (after the `claudetalk auth link-webapp` linking step). They share the same
message history, chats, and `discover` results. This is the desired behavior.

### 9.2 Message Body Sensitivity

Local Claude Code sessions contain code, file paths, error stacks, and proprietary
context. Webapp Claudes are running in a browser context and may be on untrusted
networks. The relay sees all message bodies in Phase N1 (TLS only). Phase N2 E2E
encryption applies equally to online Claudes.

For online Claudes, E2E encryption is harder: the online Claude does not hold the
shared symmetric secret (that key lives in `~/.claudetalk/network.json` on local
machines). Options:

**Option 1 — Online Claudes operate unencrypted in Phase N2:** the relay applies
encryption only to WS-path frames. HTTP-MCP frames are stored in plaintext. The
`encrypted` flag on frames distinguishes them. Online Claudes see plaintext; local
machines decrypt WS frames. This is a downgrade from E2E for the online path.

**Option 2 — Relay decrypts WS frames before storing for online access:** the relay
holds the shared secret and decrypts before inserting into relay_db.sqlite. This
means the relay operator sees everything — acceptable for a personal self-hosted relay,
but not for a shared one.

**Option 3 — Online Claudes get their own derived key:** after OAuth linking, the
relay generates a per-user-session encryption key and distributes it to the online
Claude via a tool call response. The online Claude encrypts/decrypts in-context.
This requires the online Claude to hold key material across turns — which is possible
if the Claude.ai session persists context, but is fragile.

**Recommendation: Phase N2 uses Option 2 for online Claudes.** The relay is
self-hosted and personal; the operator is the user. Document that online Claude
messages pass through the relay in plaintext (after WS decryption) and that this is
the accepted tradeoff. Option 3 can be revisited when claude.ai provides a persistent
key storage mechanism for Connector sessions. [Opinion]

### 9.3 Org-Level Shared Chats

When user A invites user B to a group chat:
- The minimal-friction model for Phase N1b: B must be registered in A's relay (either
  with A's shared_secret distributed out-of-band, or via gateway token).
- B's webapp Claude connects to A's relay URL with a gateway token. B's Claude joins
  the group chat by calling `groupchat slug=<slug> join=true`.
- B does NOT need to install ClaudeTalk locally. B's Claude uses the HTTP-MCP path
  exclusively.
- Message routing is entirely within A's relay. A retains full control over the data.
- Access revocation: A removes B's gateway token from `network.json`. B's future
  connections are rejected. In-flight sessions expire when their OAuth/gateway token
  expires (recommended TTL: 24 hours for gateway tokens).

This model does not require any enrollment on B's machine. B's oauth identity is
irrelevant; only the gateway token matters.

---

## 10. Phase Interleaving

The online Claudes work is a parallel stream that extends N1 (the relay binary),
not a replacement. It is labeled N1b because it shares the relay binary but adds the
HTTP-MCP endpoint and the dual-auth layer.

```
 N0 (done: v0.5.0)
   UUID PK migration + machine.json + network.json scaffold
   All existing tests pass.

 N1  ────────────────────────────────────────────────────────────
   Write relay binary (WS endpoint + HMAC auth + relay_db.sqlite)
   Write RelayClient in src/relay-client.ts
   Deployment config (Fly.io fly.toml or docker-compose)
   Success: two Claude Code sessions on different machines, < 500ms delivery
   No HTTP-MCP endpoint yet.

 N1b ───────────────────────────────────────────────── PARALLEL WITH N2
   Depends on: N1 (relay binary exists; relay_db schema established)
   Preconditions: open questions Q1–Q4 below resolved.

   What ships:
     - HTTP-MCP endpoint on relay (:3001/mcp or path-multiplexed)
     - WebStandardStreamableHTTPServerTransport integration
     - Dual-auth middleware (HMAC tier + OAuth tier + gateway token tier)
     - online_identities table (migration v4 of relay schema)
     - Full tool suite re-registered for HTTP-MCP path
       (same tools.ts / chat-tools.ts / etc, called against relay_db)
     - Cross-path message routing (WS publish → SSE push to online sessions)
     - `claudetalk auth link-webapp` CLI command (prints relay URL + gateway
       token for the user to paste into claude.ai Connector settings)
     - `claudetalk auth gateway-token` CLI (mints a gateway token for an
       SDK agent, writes to network.json gateway_tokens array)

   Success metrics:
     - Online Claude on claude.ai calls `whoami`, gets valid pseudonym.
     - Online Claude sends a message to a local Claude Code session.
       Local session receives it within 500ms (via WS push).
     - Local Claude Code session sends a message. Online Claude receives
       it on next `inbox` call (within the same turn) or via SSE push
       if SSE stream is open.
     - Relay handles both WS and HTTP-MCP connections simultaneously
       without contention on relay_db.sqlite (WAL mode, same as client).
     - `claudetalk auth link-webapp` emits a single URL that the user
       pastes into claude.ai as a Connector URL; no other configuration
       needed.

   Kill criterion: if claude.ai's OAuth token format is opaque (not JWT)
   and token introspection requires Anthropic server calls that are rate-
   limited or unavailable, fall back to gateway-token-only for webapp
   sessions (no OAuth path until verified).

 N2 ────────────────────────────────────────────────────────────
   E2E encryption for WS path (as designed in network-design.md)
   Online Claude path: relay decrypts WS frames before storing
   (Option 2 from §9.2)
   Encrypted flag per frame in relay_db.sqlite

 N2b (future, not committed)
   Per-session encryption for online Claudes (Option 3)
   Requires verification of claude.ai session key persistence.

 N3 ────────────────────────────────────────────────────────────
   UX polish: add-machine QR, relay health/metrics, rate limiting
   Guest invite tokens for cross-user shared chats
   Mobile-specific testing (to verify capabilities)
```

---

## 11. Latency Characteristics

| Scenario | Delivery mechanism | Expected latency |
|---|---|---|
| Local → Local (both online) | WS push via relay | 50–200 ms |
| Local → Online (SSE stream open) | WS → relay → SSE push | 100–400 ms |
| Local → Online (SSE stream closed) | stored; delivered on next `inbox` call | next turn (seconds to minutes) |
| Online → Local (WS connected) | HTTP-MCP POST → relay → WS push | 100–400 ms |
| Online → Online (both SSE open) | HTTP-MCP POST → relay → SSE push | 150–500 ms |
| Online → any (SSE not open) | stored; delivered on `inbox` | next turn |

The HTTP-SSE stream delivers real-time push within a turn. Between turns, the model
degrades to pull. This is acceptable because conversation turn structure is the natural
latency boundary — a webapp user is not waiting for a response mid-turn.

[Opinion: the within-turn real-time case is the one that matters for collaborative
Claude workflows. The between-turn polling is exactly analogous to how the hook system
works for Claude Code sessions: it fires at turn start.]

---

## 12. Open Questions (Decisions Required Before N1b Starts)

**Q1 — OAuth token format from claude.ai Connectors.**
Are tokens sent by claude.ai to Connectors verifiable JWTs (with `sub` and standard
claims) or opaque tokens requiring introspection? This determines whether Tier 2 auth
can be implemented without an external API call per request, and whether the
`oauth_sub` can be extracted server-side. If opaque: gateway tokens are the only
viable path for webapp sessions in N1b. Resolution required before building the auth
middleware.

**Q2 — Namespace linking UX.**
The design assumes users do a `claudetalk auth link-webapp` step to merge their
webapp namespace with their local HMAC namespace. How should this work in practice?
Options: (a) relay generates a one-time-use URL that the user opens in a browser to
authorize the link (requires the relay to have an OAuth callback endpoint), (b) the
user pastes their oauth_sub or a relay-printed token into the cli command manually,
(c) skip linking for now — webapp and local sessions are always in separate namespaces
until an explicit cross-namespace chat invitation is used. Option (c) is the least
friction for N1b. Decision required because it determines the relay's namespace model.

**Q3 — Relay schema ownership.**
N1b requires the relay's SQLite to hold the full application schema (not just frame log).
This means `src/migrations.ts` must be importable from the relay process. Either (a)
the relay is built from the same Bun package as the MCP server (a single binary with
two modes), or (b) the relay is a separate package that imports migrations as a shared
module. Option (a) is simpler but couples release cycles. Option (b) is cleaner but
adds a packaging step. Decision affects N1 relay architecture.

**Q4 — Multi-instance relay and sticky sessions.**
A single-process relay (one Fly.io instance) handles sticky sessions trivially — all
SSE connections and their corresponding POST requests hit the same process. If the relay
is ever horizontally scaled, SSE streams require a sticky session router (e.g.,
`Fly-Force-Instance-Id` header on Fly.io). For Phase N1b, assume single-instance.
Document the limitation. Decision: should N1b design for single-instance-only, or
include a note on what horizontal scaling would require?

**Q5 — Mobile connector capabilities.**
Claude mobile apps may not support background SSE streams or may have different
Connector configuration flows. If mobile only supports the stateless HTTP-MCP mode
(each POST is independent, no SSE stream held open), the relay's HTTP-MCP endpoint
should also handle stateless mode. The SDK's `sessionIdGenerator: undefined` setting
enables stateless mode. Should the relay support both stateful and stateless mode on
the same endpoint, or require stateful? Resolution: verify with Anthropic documentation
or empirical testing before N1b launch.

---

## 13. What We Don't Yet Know

The following are genuine unknowns as of late 2025 / early 2026. They are labeled
[to verify] and represent blockers or design risks for N1b.

**[to verify: Q-Verify-1]** — Claude.ai mobile app Connector support scope.
Whether Claude mobile apps support the full Streamable HTTP transport (including SSE
streams and session management) or only the deprecated SSE transport or only stateless
POST is not publicly confirmed. This affects whether the relay needs to run the
deprecated `SSEServerTransport` alongside the Streamable HTTP transport, or whether
Streamable HTTP is sufficient for all surfaces.

**[to verify: Q-Verify-2]** — OAuth token format from claude.ai.
Whether claude.ai sends JWTs (verifiable without introspection) or opaque tokens to
Connectors is not publicly documented. The MCP SDK's auth layer is built around the
assumption that the relay is its own OAuth AS — not that it is a resource server
validating tokens from Anthropic's AS. The correct pattern (resource server validation
via introspection or JWKS) may require a different SDK integration path than the
`mcpAuthRouter` helper provides.

**[to verify: Q-Verify-3]** — Connector configuration on claude.ai.
Whether claude.ai Connectors can be configured with an arbitrary HTTPS URL (pointing
to the relay) plus a pre-existing OAuth token, or whether the relay must implement the
full OAuth dynamic client registration flow (as the SDK's `mcpAuthRouter` does), is not
publicly confirmed. If dynamic client registration is required, the relay's auth
implementation is substantially more complex. If a static pre-configured Bearer token
(gateway token) suffices, N1b auth is straightforward.

**[to verify: Q-Verify-4]** — Agent SDK MCP integration stability.
The `MCPServerHTTP` / `MCPServerStdio` client API in the Anthropic Agent SDK was
documented in mid-2025. Whether the client side of the Streamable HTTP transport
(connecting to a remote server) is stable and production-ready in the current Agent
SDK version, or still in preview, should be confirmed before promising that SDK agents
will work out-of-the-box with the relay HTTP-MCP endpoint.

**[to verify: Q-Verify-5]** — SSE stream persistence across claude.ai turns.
Whether the claude.ai webapp holds the SSE GET stream open during the entire
conversation or drops and re-establishes it on each turn is not confirmed. If the
stream is dropped between turns, the "within-turn real-time push" story degrades to
"pull-on-every-inbox-call" for all webapp sessions. The `Last-Event-ID` resumability
feature in protocol version `2025-11-25` would help with catch-up on reconnect, but
does not provide within-turn push if the stream was never opened this turn. This
directly affects the latency claims in Section 11.

---

## 14. Evidence Citations

- [Strong] The MCP Streamable HTTP transport is implemented in `@modelcontextprotocol/sdk`
  `^1.29.0` (installed in this repo) as `WebStandardStreamableHTTPServerTransport`.
  The SDK source confirms: POST for JSON-RPC, GET for SSE stream, DELETE for session
  termination, `mcp-session-id` header for session tracking, and `Last-Event-ID`
  resumability in protocol version `>= 2025-11-25`. This is code, not documentation —
  treat it as ground truth for the SDK's behavior.

- [Strong] The deprecated `SSEServerTransport` is present in the SDK but marked
  deprecated. The current server should use `WebStandardStreamableHTTPServerTransport`.
  Both exist in the installed SDK version.

- [Strong] The SDK's `WebStandardStreamableHTTPServerTransport` supports both stateful
  mode (session IDs) and stateless mode (`sessionIdGenerator: undefined`). Stateless
  mode drops the SSE push capability but allows each POST to be handled by any server
  instance.

- [Moderate] claude.ai Connectors use remote MCP servers over HTTPS, introduced in the
  October 2025 Connector wave. Authentication uses OAuth 2.1. Source: Anthropic
  developer blog and MCP specification documentation.

- [Promising] The relay dual-endpoint pattern (WS + HTTP on same Bun process) is
  architecturally straightforward — Bun's built-in HTTP/WS server handles both
  protocols on the same port with path routing. No additional runtime dependency
  required.

- [Promising] WAL-mode SQLite handles concurrent writes from WS-path tool calls and
  HTTP-MCP-path tool calls on the same relay_db.sqlite without modification.
  The existing `PRAGMA busy_timeout = 500` and WAL autocheckpoint config from
  `db.ts` applies to the relay's SQLite as well.

- [Opinion] The relay acting as its own MCP server for online Claudes (rather than
  proxying to the client-side MCP server) is the cleanest architecture because it
  avoids having a remote Claude Code session act as a server for an online client —
  a topology that would require the local machine to be reachable, defeating the
  purpose of a relay.

- [Opinion] Gateway tokens (static pre-shared secrets for Agent SDK instances) are the
  right minimal-friction path for headless agents. OAuth is appropriate for interactive
  webapp sessions where the user is present to authorize; it is friction without benefit
  for automated agents running in CI or cloud functions that already have access to
  configuration secrets.
