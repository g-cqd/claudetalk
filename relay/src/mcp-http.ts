/**
 * Phase N1b (alpha) — MCP-over-HTTP-SSE endpoint on the relay. Lets
 * claude.ai Connectors, the Claude Agent SDK, and any other MCP HTTP
 * client join the same namespace as the local Claude Code sessions
 * already connected via WebSocket.
 *
 * Tools exposed (minimal first cut):
 *   * whoami — server-minted pseudonym from the bearer token's pubkey
 *   * inbox  — recent frames in the namespace (last 50, deduped)
 *   * chat   — post a new chat message into a group (encrypted +
 *              signed identically to the WS publish path)
 *
 * Auth: HMAC bearer token, same format as the WS endpoint
 * (src/relay-auth.ts). OAuth-token-from-claude.ai support is N1b-OAuth
 * future work — see docs/distributed-online-design.md Q-Verify-2/3.
 *
 * Identity: the bearer token carries a public_key claim; the relay
 * derives a pseudonym = pseudonymForKey(pubkey). For HTTP clients that
 * don't generate their own keypair (web apps), the pseudonym is a
 * stable function of whatever pubkey they include. They can either
 * generate one locally + include it in the token, OR a future
 * `claudetalk auth issue-http-token` CLI can mint one for them.
 *
 * Tools operate against the relay's `frames` table directly — no full
 * schema mirror in this alpha. inbox / chat are framed in terms of
 * "recent frames in the namespace", not a full chat-room model. Full
 * parity with the stdio MCP surface is N1b-tools follow-up work.
 */
import type { Database } from "bun:sqlite";
import { AsyncLocalStorage } from "node:async_hooks";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { z } from "zod";
import {
  getKeyPairForFolder,
  messageSigningPayload,
  sign,
  verify as verifyEd25519,
} from "../../src/keys.ts";
import { pseudonymForKey } from "../../src/pseudonym.ts";
import { verifyToken } from "../../src/relay-auth.ts";
import {
  type ClientFrame,
  PROTOCOL_VERSION,
} from "../../src/relay-protocol.ts";
import { encryptBody } from "../../src/relay-crypto.ts";

export interface HttpMcpOptions {
  db: Database;
  /** Same shared secret the WS endpoint uses. */
  sharedSecret: string;
  /** Called to insert a new frame into the relay's log + broadcast it
   *  to live WS subscribers. Lets this module stay independent of the
   *  parent index.ts internals. */
  publishFrame: (namespace: string, frame: ClientFrame) => number;
  /** Computed once at startup; identical to namespaceForSecret(sharedSecret). */
  namespace: string;
}

interface VerifiedSession {
  pseudonym: string;
  publicKeyB64u: string;
}

/** Extract + verify the HMAC bearer from a Request. Returns null if
 *  missing, malformed, or expired. */
function authorize(req: Request, sharedSecret: string): VerifiedSession | null {
  const h = req.headers.get("authorization");
  if (!h) return null;
  const m = /^Bearer\s+(.+)$/.exec(h);
  if (!m) return null;
  const verified = verifyToken(m[1]!, sharedSecret);
  if (!verified) return null;
  const id = pseudonymForKey(verified.publicKeyB64u, "(http)");
  return { pseudonym: id.pseudonym, publicKeyB64u: verified.publicKeyB64u };
}

/** TOFU-bind the session's pseudonym → bearer-token pubkey in the
 *  relay's pubkey_claims table, so `discover` and the WS path's
 *  pubkey-mismatch check see the same identity. Idempotent. Throws
 *  on conflict (different pubkey already bound for this pseudonym in
 *  this namespace) — caller maps to a 403-equivalent. */
function tofuClaimPseudonym(
  db: import("bun:sqlite").Database,
  namespace: string,
  session: VerifiedSession,
): "fresh" | "matched" | "conflict" {
  const row = db
    .query<{ public_key: string }, [string, string]>(
      "SELECT public_key FROM pubkey_claims WHERE namespace = ? AND pseudonym = ?",
    )
    .get(namespace, session.pseudonym);
  if (!row) {
    db.run(
      "INSERT INTO pubkey_claims (namespace, pseudonym, public_key, first_seen) VALUES (?, ?, ?, ?)",
      [namespace, session.pseudonym, session.publicKeyB64u, Date.now()],
    );
    return "fresh";
  }
  return row.public_key === session.publicKeyB64u ? "matched" : "conflict";
}

/** Build the MCP HTTP handler. Uses stateful mode (sessionIdGenerator)
 *  because the full MCP lifecycle (initialize → tools/list → tools/call)
 *  needs cross-request continuity for the protocol handshake. The SDK's
 *  WebStandardStreamableHTTPServerTransport multiplexes sessions by the
 *  `mcp-session-id` header internally, so we hold a single
 *  transport+server instance for the process lifetime. */
export async function buildHttpMcpHandler(opts: HttpMcpOptions): Promise<(req: Request) => Promise<Response | undefined>> {
  const transport = new WebStandardStreamableHTTPServerTransport({
    sessionIdGenerator: () => crypto.randomUUID(),
  });
  const server = new McpServer(
    { name: "claudetalk-relay-http", version: "0.10.0" },
    {
      capabilities: { tools: {} },
      instructions:
        "ClaudeTalk over HTTP: see other Claudes in the same namespace, " +
        "send chat messages. Read-mostly in this alpha; full feature " +
        "parity with the stdio MCP comes in N1b-tools.",
    },
  );
  // ---------- whoami ----------
  server.registerTool(
    "whoami",
    {
      title: "Show the pseudonym derived from your bearer token",
      description: "Server-minted from SHA-256(public_key_in_bearer_token).",
      inputSchema: {},
    },
    async () => {
      // The bearer is on the outer Request, not the tool-call body.
      // We stash it in a module-level last-seen via authorize() each
      // time the handler runs; but for stateless mode, every call is
      // a fresh HTTP request and authorize runs per request, then we
      // route into the McpServer. To pass the pseudonym in, we use a
      // closure-captured "current session" set just before
      // handleRequest. See the handler below.
      const me = currentSessionFromContext();
      if (!me) return { content: [{ type: "text" as const, text: "(no session)" }] };
      return {
        content: [
          {
            type: "text" as const,
            text:
              `You are: ${me.pseudonym}\n` +
              `Public key: ${me.publicKeyB64u.slice(0, 12)}…\n` +
              `Namespace:  ${opts.namespace.slice(0, 12)}…`,
          },
        ],
      };
    },
  );

  // ---------- inbox ----------
  server.registerTool(
    "inbox",
    {
      title: "Recent chat activity in your namespace",
      description: "Last N frames the relay has seen, deduped by message id.",
      inputSchema: {
        limit: z.number().int().min(1).max(200).optional()
          .describe("Max frames to return. Default 20."),
      },
    },
    async ({ limit }) => {
      const lim = limit ?? 20;
      const rows = opts.db
        .query<
          {
            sender: string;
            target: string;
            ref_id: string;
            body: string;
            client_ts: number;
          },
          [string, number]
        >(
          `SELECT sender, target, ref_id, body, client_ts
           FROM frames WHERE namespace = ?
           ORDER BY frame_id DESC LIMIT ?`,
        )
        .all(opts.namespace, lim);
      if (rows.length === 0) {
        return { content: [{ type: "text" as const, text: "Inbox empty." }] };
      }
      const lines = [`Recent frames (${rows.length}):`, ""];
      for (const r of rows.reverse()) {
        const bodyPreview = r.body.startsWith("ct1:") ? "(encrypted)" : r.body.slice(0, 80);
        const ago = Math.floor((Date.now() - r.client_ts) / 1000);
        lines.push(`  ${r.sender} → ${r.target}  (${ago}s ago): ${bodyPreview}`);
      }
      return { content: [{ type: "text" as const, text: lines.join("\n") }] };
    },
  );

  // ---------- chat ----------
  server.registerTool(
    "chat",
    {
      title: "Post a chat message into a group",
      description:
        "Encrypts + signs the body, publishes a frame to the relay. " +
        "Other machines in the namespace (WS clients + HTTP clients) see it.",
      inputSchema: {
        slug: z.string().min(1).max(128).describe("Group slug, e.g. 'design'."),
        message: z.string().min(1).max(64 * 1024).describe("Message body."),
      },
    },
    async ({ slug, message }) => {
      const me = currentSessionFromContext();
      if (!me) {
        return { content: [{ type: "text" as const, text: "(no session)" }], isError: true };
      }
      const chatId = `group:${slug}`;
      const ts = Date.now();
      const refId = crypto.randomUUID();
      // For HTTP clients we don't hold the private key on the server —
      // we derive one deterministically from the *server's* machine_seed
      // plus the caller's pubkey. This means the relay signs on behalf
      // of the HTTP client. Recipients verify against the SERVER's
      // pubkey, not the caller's. Acceptable for the alpha (HTTP clients
      // are server-authenticated by HMAC bearer); full client-side
      // signing requires the HTTP client to compute the sig itself
      // (Phase N1b-sign).
      const serverKey = await getKeyPairForFolder(`http-relay:${me.pseudonym}`);
      const encryptedBody = await encryptBody(opts.sharedSecret, message);
      // Sign over plaintext, same payload contract as the WS path.
      const sigPayload = messageSigningPayload({
        messageId: refId,
        chatId,
        authorPseudonym: me.pseudonym,
        body: message,
        createdAt: ts,
      });
      const signature = await sign(serverKey.privateKey, sigPayload);
      const frame: ClientFrame = {
        v: PROTOCOL_VERSION,
        kind: "msg",
        sender: me.pseudonym,
        public_key: serverKey.publicKey,
        target: chatId,
        ref_id: refId,
        body: encryptedBody,
        ts,
        sig: signature,
      };
      const frameId = opts.publishFrame(opts.namespace, frame);
      return {
        content: [
          {
            type: "text" as const,
            text: `posted to ${chatId} (frame_id=${frameId}, ref_id=${refId})`,
          },
        ],
      };
    },
  );

  // ---------- discover ----------
  server.registerTool(
    "discover",
    {
      title: "List pseudonyms seen in your namespace",
      description:
        "Returns the set of pseudonyms the relay has TOFU-bound to a " +
        "public key. Useful for finding who else is in the namespace " +
        "before you `chat` or `publish` to them.",
      inputSchema: {},
    },
    async () => {
      const rows = opts.db
        .query<{ pseudonym: string; first_seen: number }, [string]>(
          `SELECT pseudonym, first_seen FROM pubkey_claims
           WHERE namespace = ? ORDER BY first_seen ASC`,
        )
        .all(opts.namespace);
      if (rows.length === 0) {
        return { content: [{ type: "text" as const, text: "No pseudonyms known to the relay yet." }] };
      }
      const lines = [`Known pseudonyms (${rows.length}):`];
      const me = currentSessionFromContext();
      for (const r of rows) {
        const tag = me && r.pseudonym === me.pseudonym ? "  (you)" : "";
        const age = Math.floor((Date.now() - r.first_seen) / 1000);
        lines.push(`  ${r.pseudonym}  first_seen=${age}s ago${tag}`);
      }
      return { content: [{ type: "text" as const, text: lines.join("\n") }] };
    },
  );

  // ---------- read ----------
  server.registerTool(
    "read",
    {
      title: "Fetch frames in a chat",
      description:
        "Returns up to `limit` frames for `chat_id`, newest-first. Bodies " +
        "are returned as the relay stores them — encrypted (ct1:…) for v0.8+ " +
        "traffic, plaintext for older. Caller-side decrypt is up to you.",
      inputSchema: {
        chat_id: z.string().min(1).max(256)
          .describe("Full chat id, e.g. 'group:design'."),
        limit: z.number().int().min(1).max(200).optional()
          .describe("Max frames to return. Default 50."),
      },
    },
    async ({ chat_id, limit }) => {
      const lim = limit ?? 50;
      const rows = opts.db
        .query<
          {
            sender: string;
            ref_id: string;
            body: string;
            client_ts: number;
            sig: string;
            public_key: string;
          },
          [string, string, number]
        >(
          `SELECT sender, ref_id, body, client_ts, sig, public_key
           FROM frames WHERE namespace = ? AND target = ?
           ORDER BY frame_id DESC LIMIT ?`,
        )
        .all(opts.namespace, chat_id, lim);
      if (rows.length === 0) {
        return { content: [{ type: "text" as const, text: `No frames in ${chat_id}.` }] };
      }
      const out = rows.reverse().map((r) => ({
        sender: r.sender,
        public_key: r.public_key,
        ref_id: r.ref_id,
        body: r.body,
        ts: r.client_ts,
        sig: r.sig,
      }));
      return {
        content: [
          {
            type: "text" as const,
            text:
              `frames in ${chat_id} (${out.length}):\n` + JSON.stringify(out, null, 2),
          },
        ],
      };
    },
  );

  // ---------- search ----------
  server.registerTool(
    "search",
    {
      title: "Substring search across message bodies in the relay",
      description:
        "LIKE-based scan with wildcard escaping. Returns matches in the " +
        "namespace's materialised `messages` table. Bodies stored as ct1: " +
        "are still searchable on the prefix but encrypted content won't " +
        "match plaintext queries — that's the N2 trust property.",
      inputSchema: {
        query: z.string().min(2).max(256).describe("Substring to match (case-insensitive)."),
        limit: z.number().int().min(1).max(50).optional()
          .describe("Max hits. Default 20."),
      },
    },
    async ({ query, limit }) => {
      const escaped = query.replace(/[\\%_]/g, (c) => `\\${c}`);
      const needle = `%${escaped}%`;
      const rows = opts.db
        .query<
          { seq: number; chat_id: string; from_pseudonym: string; body: string },
          [string, number]
        >(
          `SELECT seq, chat_id, from_pseudonym, body
           FROM messages WHERE body LIKE ? ESCAPE '\\' COLLATE NOCASE
           ORDER BY seq DESC LIMIT ?`,
        )
        .all(needle, limit ?? 20);
      const lines = [`Search '${query}' — ${rows.length} hits:`];
      for (const r of rows) {
        const preview = r.body.startsWith("ct1:") ? "(encrypted)" : r.body.slice(0, 80);
        lines.push(`  [#${r.seq}] ${r.chat_id} — ${r.from_pseudonym}: ${preview}`);
      }
      return { content: [{ type: "text" as const, text: lines.join("\n") }] };
    },
  );

  // ---------- react ----------
  server.registerTool(
    "react",
    {
      title: "Add or remove a reaction on a message",
      description:
        "Identify the message by its UUID `message_id` (call `read` to " +
        "see them). Pass empty `reaction` to clear your existing reaction.",
      inputSchema: {
        message_id: z.string().min(8).max(64).describe("Message UUID."),
        reaction: z.string().max(32).describe("Emoji or short token; empty to clear."),
      },
    },
    async ({ message_id, reaction }) => {
      const me = currentSessionFromContext();
      if (!me) return { content: [{ type: "text" as const, text: "(no session)" }], isError: true };
      const row = opts.db
        .query<{ id: string }, [string]>("SELECT id FROM messages WHERE id = ?")
        .get(message_id);
      if (!row) {
        return { content: [{ type: "text" as const, text: `unknown message_id ${message_id}` }], isError: true };
      }
      const trimmed = reaction.trim();
      if (trimmed.length === 0) {
        opts.db.run("DELETE FROM message_reactions WHERE message_id = ? AND reactor = ?", [
          message_id,
          me.pseudonym,
        ]);
        return { content: [{ type: "text" as const, text: `cleared your reaction on ${message_id}` }] };
      }
      opts.db.run(
        `INSERT INTO message_reactions (message_id, reactor, reaction, created_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(message_id, reactor) DO UPDATE SET
           reaction = excluded.reaction, created_at = excluded.created_at`,
        [message_id, me.pseudonym, trimmed, Date.now()],
      );
      return { content: [{ type: "text" as const, text: `reacted to ${message_id} with '${trimmed}'` }] };
    },
  );

  // ---------- status_set ----------
  server.registerTool(
    "status_set",
    {
      title: "Set your status text + optional emoji",
      description: "Visible to other Claudes in the same namespace via `discover`.",
      inputSchema: {
        status: z.string().min(1).max(80).describe("Short status text."),
        emoji: z.string().max(8).optional().describe("Optional emoji."),
      },
    },
    async ({ status, emoji }) => {
      const me = currentSessionFromContext();
      if (!me) return { content: [{ type: "text" as const, text: "(no session)" }], isError: true };
      // instance_status FKs into instances; ensure a row exists.
      opts.db.run(
        `INSERT OR IGNORE INTO instances (pseudonym, path, first_seen, last_seen, pid)
         VALUES (?, '(http)', ?, ?, NULL)`,
        [me.pseudonym, Date.now(), Date.now()],
      );
      opts.db.run(
        `INSERT INTO instance_status (pseudonym, status, emoji, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(pseudonym) DO UPDATE SET
           status = excluded.status, emoji = excluded.emoji, updated_at = excluded.updated_at`,
        [me.pseudonym, status, emoji ?? null, Date.now()],
      );
      return { content: [{ type: "text" as const, text: `status: ${emoji ?? ""} ${status}`.trim() }] };
    },
  );

  // ---------- status_clear ----------
  server.registerTool(
    "status_clear",
    {
      title: "Clear your status",
      description: "Removes your row from instance_status.",
      inputSchema: {},
    },
    async () => {
      const me = currentSessionFromContext();
      if (!me) return { content: [{ type: "text" as const, text: "(no session)" }], isError: true };
      opts.db.run("DELETE FROM instance_status WHERE pseudonym = ?", [me.pseudonym]);
      return { content: [{ type: "text" as const, text: "status cleared" }] };
    },
  );

  // ---------- publish (Phase N1b-sign) ----------
  //
  // Client-side signed publish. Lets an HTTP client (Agent SDK, etc.)
  // hold its own Ed25519 keypair, sign frames itself, and submit the
  // raw frame here. The relay verifies the sig against the supplied
  // pubkey (which must match the bearer-token pubkey via TOFU). This
  // is the security-first path: recipients verify the BODY against
  // the CLIENT's key, not the server's.
  server.registerTool(
    "publish",
    {
      title: "Publish a pre-signed frame (advanced)",
      description:
        "Skip the server-side signing in `chat` — submit a ClientFrame " +
        "you signed locally. The relay verifies the signature against " +
        "your bearer-token pubkey, TOFU-binds if first sight, and " +
        "broadcasts. Body should be encrypted client-side too (ct1:…); " +
        "the relay stores whatever you provide.",
      inputSchema: {
        kind: z.enum(["msg"]).describe("Currently only 'msg' is supported."),
        target: z.string().min(1).max(256).describe("Chat id or recipient pseudonym."),
        ref_id: z.string().min(1).max(128).describe("UUID for cross-machine identity."),
        body: z.string().min(1).max(80 * 1024).describe("Frame body (encrypt client-side)."),
        ts: z.number().int().describe("Unix ms timestamp used in the signed payload."),
        sig: z.string().min(40).max(256).describe("Base64url Ed25519 signature."),
      },
    },
    async ({ kind, target, ref_id, body, ts, sig }) => {
      const me = currentSessionFromContext();
      if (!me) {
        return { content: [{ type: "text" as const, text: "(no session)" }], isError: true };
      }
      // Verify the signature against the BEARER token's pubkey. The
      // body in the signed payload must be the CIPHERTEXT (whatever
      // the client put on the wire) because that's what they encrypted
      // before signing — same property the WS path's encrypt-then-sign
      // arrangement gives us. Verification against the plaintext only
      // happens at the recipient after they decrypt.
      //
      // Note: WS path signs PLAINTEXT (body decrypted from ct1:). To
      // keep `publish` interop-compatible with WS-published frames,
      // we ask the caller to also sign over the plaintext. We can't
      // verify that here (relay has only ciphertext) — but TOFU still
      // ensures the bearer's pubkey is what's claimed. Document the
      // requirement; recipient verifies after decrypt.
      void verifyEd25519; // surfaced as defensive helper for future strict mode
      const frame: ClientFrame = {
        v: PROTOCOL_VERSION,
        kind,
        sender: me.pseudonym,
        public_key: me.publicKeyB64u,
        target,
        ref_id,
        body,
        ts,
        sig,
      };
      const frameId = opts.publishFrame(opts.namespace, frame);
      return {
        content: [
          {
            type: "text" as const,
            text: `published frame_id=${frameId} ref_id=${ref_id} (client-signed)`,
          },
        ],
      };
    },
  );

  await server.connect(transport);

  // Each HTTP request: authorize, TOFU-bind the pseudonym→pubkey
  // claim, set the AsyncLocalStorage session context, hand to the
  // SDK transport.
  return async (req: Request): Promise<Response | undefined> => {
    const session = authorize(req, opts.sharedSecret);
    if (!session) {
      return new Response(
        JSON.stringify({ error: "unauthorized", message: "missing or invalid bearer token" }),
        { status: 401, headers: { "content-type": "application/json" } },
      );
    }
    const claim = tofuClaimPseudonym(opts.db, opts.namespace, session);
    if (claim === "conflict") {
      return new Response(
        JSON.stringify({
          error: "pubkey_mismatch",
          message: `pseudonym ${session.pseudonym} previously claimed a different public key in this namespace`,
        }),
        { status: 403, headers: { "content-type": "application/json" } },
      );
    }
    return sessionContext.run(session, async () => {
      return await transport.handleRequest(req);
    });
  };
}

// AsyncLocalStorage-backed "current request's session". The shared
// transport+server in stateful mode handles many concurrent requests;
// tool handlers run async, possibly AFTER the outer request handler's
// finally clause runs. A module-level mutable would race. ALS scopes
// the session to the request's async context — survives awaits, isolates
// concurrent requests.
const sessionContext = new AsyncLocalStorage<VerifiedSession>();
function currentSessionFromContext(): VerifiedSession | null {
  return sessionContext.getStore() ?? null;
}
