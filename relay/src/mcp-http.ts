/**
 * Phase N1b — MCP-over-HTTP-SSE endpoint on the relay. Lets claude.ai
 * Connectors, the Claude Agent SDK, and any other MCP HTTP client join
 * the same namespace as local Claude Code sessions already connected
 * via WebSocket.
 *
 * v0.10.5 (N1b-tools-5): the full stdio MCP tool surface (whoami,
 * discover, ask, answer, inbox, chat, groupchat, read, react, status_*,
 * search, mute, nicknames_*, notifications_reset — 18 tools) is now
 * exposed over HTTP via the same `registerTools` registration the
 * stdio server uses. The per-request identity is supplied via
 * AsyncLocalStorage (`identityContext.run`); tool handlers see the
 * caller's pseudonym + (server-derived) keypair through the
 * `dynamicIdentity` Proxy from `src/identity-context.ts`.
 *
 * Auth: HMAC bearer token (`src/relay-auth.ts`), 401 on missing/bad.
 *
 * Relay-specific extras (not in stdio MCP):
 *   * `publish` — submit a pre-signed ClientFrame, bypassing the
 *     server-side signing path. For clients who hold their own
 *     Ed25519 keypair (Agent SDK).
 *
 * Identity for HTTP clients:
 *   The bearer token carries the caller's pubkey. The pseudonym is
 *   `pseudonymForKey(pubkey)`. For tools that sign messages (chat,
 *   groupchat, ask, answer), we derive a server-side per-pseudonym
 *   keypair (`http-relay:<pseudonym>`) and attach it to the per-
 *   request Identity. Recipients verify against the SERVER's pubkey,
 *   not the caller's bearer pubkey. Full client-side signing of these
 *   messages uses the `publish` tool instead.
 */
import type { Database } from "bun:sqlite";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { z } from "zod";
import { getKeyPairForFolder } from "../../src/keys.ts";
import { pseudonymForKey, type Identity } from "../../src/pseudonym.ts";
import { verifyToken } from "../../src/relay-auth.ts";
import {
  type ClientFrame,
  PROTOCOL_VERSION,
} from "../../src/relay-protocol.ts";
import { identityContext } from "../../src/identity-context.ts";
import { registerTools } from "../../src/tools.ts";

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
 *  relay's pubkey_claims table. Returns "conflict" if a different
 *  pubkey was previously claimed for this pseudonym in this namespace
 *  (caller maps to 403). */
function tofuClaimPseudonym(
  db: Database,
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

/** Ensure the session's pseudonym is in `instances` so touchInstance,
 *  upsertInstance, etc. find a row. Idempotent. */
function ensureInstanceRow(db: Database, session: VerifiedSession): void {
  db.run(
    `INSERT INTO instances (pseudonym, path, first_seen, last_seen, pid, public_key)
     VALUES (?, '(http)', ?, ?, NULL, ?)
     ON CONFLICT(pseudonym) DO UPDATE SET
       last_seen = excluded.last_seen,
       public_key = COALESCE(excluded.public_key, instances.public_key)`,
    [session.pseudonym, Date.now(), Date.now(), session.publicKeyB64u],
  );
}

/** Per-pseudonym server-side keypair cache. Tools that sign (chat,
 *  groupchat, ask, answer) read me.keyPair from the Identity Proxy;
 *  we derive a deterministic key from the relay's machine_seed plus
 *  the caller's pseudonym so each pseudonym signs consistently. */
const keypairCache = new Map<string, Identity["keyPair"]>();

async function getServerSideKeyFor(pseudonym: string): Promise<NonNullable<Identity["keyPair"]>> {
  const cached = keypairCache.get(pseudonym);
  if (cached) return cached;
  const k = await getKeyPairForFolder(`http-relay:${pseudonym}`);
  keypairCache.set(pseudonym, k);
  return k;
}

/** Build the MCP HTTP handler. Uses stateful mode (sessionIdGenerator)
 *  for the full MCP lifecycle; one transport+server pair lives for the
 *  process lifetime and the SDK multiplexes per `mcp-session-id`. */
export async function buildHttpMcpHandler(opts: HttpMcpOptions): Promise<(req: Request) => Promise<Response | undefined>> {
  const transport = new WebStandardStreamableHTTPServerTransport({
    sessionIdGenerator: () => crypto.randomUUID(),
  });
  const server = new McpServer(
    { name: "claudetalk-relay-http", version: "0.10.5" },
    {
      capabilities: { tools: {} },
      instructions:
        "ClaudeTalk over HTTP — same tool surface as the stdio MCP. " +
        "Identity is derived from your bearer-token public key. Bodies " +
        "encrypted client-side; the relay holds ciphertext only.",
    },
  );

  // Bootstrap identity passed to registerTools at startup. The
  // `dynamicIdentity` Proxy inside each register* function transparently
  // routes every `me.*` access to the per-request Identity via
  // AsyncLocalStorage. So this bootstrap value is never actually read
  // at runtime — it's just shape-satisfying for the type signature.
  const bootstrapMe: Identity = {
    pseudonym: "(http-bootstrap)",
    path: "(http)",
    hash: "",
  };
  registerTools(server, bootstrapMe);

  // ---------- publish (relay-specific, not in stdio MCP) ----------
  server.registerTool(
    "publish",
    {
      title: "Publish a pre-signed frame (advanced — Agent SDK / custom clients)",
      description:
        "Skip the server-side signing in `chat` — submit a ClientFrame " +
        "you signed locally. The relay TOFU-binds your bearer pubkey on " +
        "first sight and broadcasts. Body should be encrypted client- " +
        "side too (ct1:…); the relay stores whatever you provide.",
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
      const me = identityContext.getStore();
      if (!me) {
        return {
          content: [{ type: "text" as const, text: "(no session)" }],
          isError: true,
        };
      }
      // `publish` is client-signed: forward the BEARER token's pubkey
      // verbatim. The server-side keypair (me.keyPair) is what chat/
      // groupchat/ask use; publish bypasses that path.
      const frame: ClientFrame = {
        v: PROTOCOL_VERSION,
        kind,
        sender: me.pseudonym,
        public_key: me.bearerPublicKey ?? "",
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

  // Each HTTP request:
  //   1. Authorize via HMAC bearer (401 if missing/bad)
  //   2. TOFU-bind pseudonym → pubkey (403 on mismatch)
  //   3. Ensure instances row exists (touchInstance et al. require it)
  //   4. Build per-request Identity with a server-side keypair
  //   5. Wrap handleRequest in identityContext.run so the Proxy in
  //      registerTools' me resolves to this Identity
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
    ensureInstanceRow(opts.db, session);
    const keyPair = await getServerSideKeyFor(session.pseudonym);
    const identity: Identity = {
      pseudonym: session.pseudonym,
      path: "(http)",
      hash: session.publicKeyB64u,
      keyPair,
      bearerPublicKey: session.publicKeyB64u,
    };
    return identityContext.run(identity, async () => {
      return await transport.handleRequest(req);
    });
  };
}
