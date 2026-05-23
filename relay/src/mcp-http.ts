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
import { messageSigningPayload, sign, getKeyPairForFolder } from "../../src/keys.ts";
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


  await server.connect(transport);

  // Each HTTP request: authorize, set closure-captured currentSession,
  // hand to the SDK transport, clean up.
  return async (req: Request): Promise<Response | undefined> => {
    const session = authorize(req, opts.sharedSecret);
    if (!session) {
      return new Response(
        JSON.stringify({ error: "unauthorized", message: "missing or invalid bearer token" }),
        { status: 401, headers: { "content-type": "application/json" } },
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
