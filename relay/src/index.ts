#!/usr/bin/env bun
/**
 * ClaudeTalk relay — bridge for cross-machine ClaudeTalk traffic.
 *
 * Self-hosted Bun WebSocket server. ~300 LOC. Accepts authenticated
 * connections from RelayClients (one per machine), broadcasts each
 * `msg`/`ask`/`answer` frame to all OTHER connections in the same
 * namespace, persists frames for catch-up via HTTP `/pull`.
 *
 * Auth: HMAC-SHA256 bearer token (src/relay-auth.ts), ±30 s timestamp
 * window. Namespaces are SHA-256 of the shared secret — two machines
 * with the same secret share a namespace and see each other; different
 * secrets give different namespaces.
 *
 * Signature: every inbound frame's Ed25519 signature is verified against
 * the sender's public_key, which is itself TOFU-bound on first sight
 * per (namespace, pseudonym). If a later frame from the same pseudonym
 * arrives with a different pubkey, it's rejected with `pubkey_mismatch`.
 *
 * Storage: relay/relay_db.sqlite. Schema: `frames` (append-only log),
 * `pubkey_claims` (TOFU table). Retention: 30 days by default; configurable.
 *
 * Deployment: any host that can run Bun and accept inbound TCP. Fly.io,
 * Hetzner, Tailscale-reachable VPS, etc. No Anthropic dependency.
 */
import { Database } from "bun:sqlite";
import {
  type ClientFrame,
  PROTOCOL_VERSION,
  type PullResponse,
  type RelayControl,
  type RelayFrame,
} from "../../src/relay-protocol.ts";
import { verifyToken } from "../../src/relay-auth.ts";
import { messageSigningPayload, verify } from "../../src/keys.ts";
import { migrate } from "../../src/migrations.ts";
import { buildHttpMcpHandler } from "./mcp-http.ts";

const PORT = Number(process.env.RELAY_PORT ?? 7878);
const HOST = process.env.RELAY_HOST ?? "0.0.0.0";
const SHARED_SECRET = process.env.RELAY_SHARED_SECRET ?? "";
const DB_PATH = process.env.RELAY_DB_PATH ?? "relay_db.sqlite";
const RETENTION_DAYS = Number(process.env.RELAY_RETENTION_DAYS ?? 30);
const PULL_MAX_FRAMES = 500;
// Per-namespace token bucket: defaults to 200 frames in any 10s window.
// Tuned for "many machines burst-publishing at once is fine, but a stuck
// loop on one machine doesn't drown the others." Configurable via env.
const RATE_FRAMES_PER_WINDOW = Number(process.env.RELAY_RATE_FRAMES ?? 200);
const RATE_WINDOW_MS = Number(process.env.RELAY_RATE_WINDOW_MS ?? 10_000);

if (!SHARED_SECRET) {
  console.error(
    "RELAY_SHARED_SECRET env var required (base64url-encoded 32 bytes). " +
      "Generate one with `openssl rand -base64 32 | tr '+/' '-_' | tr -d '='`.",
  );
  process.exit(2);
}

const log = (...args: unknown[]) => console.log("[relay]", ...args);

// ---------------- storage ----------------

const db = new Database(DB_PATH, { create: true });
db.exec("PRAGMA journal_mode = WAL;");
db.exec("PRAGMA busy_timeout = 1000;");
// Relay-specific tables (frames log, pubkey TOFU).
db.exec(`
  CREATE TABLE IF NOT EXISTS frames (
    frame_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace TEXT NOT NULL,
    sender    TEXT NOT NULL,
    public_key TEXT NOT NULL,
    kind      TEXT NOT NULL,
    target    TEXT NOT NULL,
    ref_id    TEXT NOT NULL,
    body      TEXT NOT NULL,
    sig       TEXT NOT NULL,
    client_ts INTEGER NOT NULL,
    relay_ts  INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_frames_ns_id ON frames(namespace, frame_id);

  CREATE TABLE IF NOT EXISTS pubkey_claims (
    namespace  TEXT NOT NULL,
    pseudonym  TEXT NOT NULL,
    public_key TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (namespace, pseudonym)
  );
`);
// Phase N1b-tools-2: also run the full ClaudeTalk schema so the relay
// can materialise inbound frames as proper message/chat rows. Lets
// HTTP MCP tools use the same primitives the stdio MCP does (asks,
// reactions, mute, nicknames, threading).
//
// NOTE: src/migrations.ts uses PRAGMA user_version + BEGIN IMMEDIATE
// to be safe under concurrency. It runs against any sqlite Database,
// not just the local MCP one. Our relay DB will end up with BOTH the
// relay-specific tables (above) AND the full ClaudeTalk schema. They
// don't conflict — no name overlap.
migrate(db);

interface FrameRow {
  frame_id: number;
  namespace: string;
  sender: string;
  public_key: string;
  kind: string;
  target: string;
  ref_id: string;
  body: string;
  sig: string;
  client_ts: number;
  relay_ts: number;
}

function insertFrame(ns: string, frame: ClientFrame, relayTs: number): number {
  const r = db.run(
    `INSERT INTO frames (namespace, sender, public_key, kind, target, ref_id, body, sig, client_ts, relay_ts)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    [
      ns,
      frame.sender,
      frame.public_key,
      frame.kind,
      frame.target,
      frame.ref_id,
      frame.body,
      frame.sig,
      frame.ts,
      relayTs,
    ],
  );
  // Phase N1b-tools-2: materialise the frame as a row in the
  // ClaudeTalk schema's `messages` table so future schema-aware HTTP
  // tools can read it as a proper chat row. Bodies are stored as the
  // relay holds them (ct1:-encrypted in v0.8+; the relay deliberately
  // never decrypts to preserve the N2 trust property).
  if (frame.kind === "msg") {
    try {
      materialiseMessageFrame(frame);
    } catch (e) {
      // Materialisation failures shouldn't kill the relay; log + move on.
      // The frame is still in `frames` for catch-up.
      log("materialise failed", (e as Error).message);
    }
  }
  return Number(r.lastInsertRowid);
}

/** Insert an inbound frame into the schema-shaped messages/chats/
 *  chat_members tables. Idempotent (re-running with the same ref_id
 *  is a no-op). Uses INSERT OR IGNORE so concurrent inserts of the
 *  same UUID can't collide. */
function materialiseMessageFrame(frame: ClientFrame): void {
  // Ensure the chat exists (group: or direct: prefix is enough).
  const kind = frame.target.startsWith("group:") ? "group" : "direct";
  db.run(
    `INSERT OR IGNORE INTO chats (id, kind, title, created_at) VALUES (?, ?, NULL, ?)`,
    [frame.target, kind, frame.ts],
  );
  // Ensure the sender is a member.
  db.run(
    `INSERT OR IGNORE INTO chat_members
       (chat_id, pseudonym, joined_at, last_read_message_seq, last_notified_message_seq)
     VALUES (?, ?, ?, 0, 0)`,
    [frame.target, frame.sender, frame.ts],
  );
  // For direct chats, also add the other peer (extracted from the id).
  if (kind === "direct") {
    const ids = frame.target.replace(/^direct:/, "").split("|");
    for (const id of ids) {
      if (id !== frame.sender) {
        db.run(
          `INSERT OR IGNORE INTO chat_members
             (chat_id, pseudonym, joined_at, last_read_message_seq, last_notified_message_seq)
           VALUES (?, ?, ?, 0, 0)`,
          [frame.target, id, frame.ts],
        );
      }
    }
  }
  // Allocate a seq via the message_seq counter (created by migration v3).
  // If the UUID already exists, skip the insert + don't bump the counter.
  const existing = db
    .query<{ id: string }, [string]>("SELECT id FROM messages WHERE id = ?")
    .get(frame.ref_id);
  if (existing) return;
  db.exec("UPDATE message_seq SET next = next + 1 WHERE id = 1");
  const seqRow = db
    .query<{ next: number }, []>("SELECT next - 1 AS next FROM message_seq WHERE id = 1")
    .get();
  const seq = seqRow?.next ?? 1;
  db.run(
    `INSERT INTO messages
       (id, seq, chat_id, from_pseudonym, body, created_at, parent_id, signature)
     VALUES (?, ?, ?, ?, ?, ?, NULL, ?)`,
    [frame.ref_id, seq, frame.target, frame.sender, frame.body, frame.ts, frame.sig],
  );
}

function pullSince(ns: string, since: number): RelayFrame[] {
  const rows = db
    .query<FrameRow, [string, number, number]>(
      `SELECT frame_id, namespace, sender, public_key, kind, target, ref_id, body, sig, client_ts, relay_ts
       FROM frames WHERE namespace = ? AND frame_id > ?
       ORDER BY frame_id ASC LIMIT ?`,
    )
    .all(ns, since, PULL_MAX_FRAMES);
  return rows.map((r) => ({
    v: PROTOCOL_VERSION,
    frame_id: r.frame_id,
    relay_ts: r.relay_ts,
    frame: {
      v: PROTOCOL_VERSION,
      kind: r.kind as ClientFrame["kind"],
      sender: r.sender,
      public_key: r.public_key,
      target: r.target,
      ref_id: r.ref_id,
      body: r.body,
      ts: r.client_ts,
      sig: r.sig,
    },
  }));
}

/** TOFU pubkey check. Returns true if the (ns, pseudonym) is either
 *  unseen (claim recorded) or matches the previously-recorded key. */
function tofuPubkey(ns: string, pseudonym: string, pubkey: string): boolean {
  const row = db
    .query<{ public_key: string }, [string, string]>(
      "SELECT public_key FROM pubkey_claims WHERE namespace = ? AND pseudonym = ?",
    )
    .get(ns, pseudonym);
  if (!row) {
    db.run(
      "INSERT INTO pubkey_claims (namespace, pseudonym, public_key, first_seen) VALUES (?, ?, ?, ?)",
      [ns, pseudonym, pubkey, Date.now()],
    );
    return true;
  }
  return row.public_key === pubkey;
}

/** Retention purge — runs at startup + every hour. */
function purgeOldFrames(): void {
  const cutoff = Date.now() - RETENTION_DAYS * 24 * 60 * 60_000;
  const r = db.run("DELETE FROM frames WHERE relay_ts < ?", [cutoff]);
  if (r.changes > 0) log(`purged ${r.changes} old frames (>${RETENTION_DAYS}d)`);
}
purgeOldFrames();
setInterval(purgeOldFrames, 60 * 60_000).unref?.();

// ---------------- live connections ----------------

interface WsData {
  namespace: string;
  pseudonym: string;
  publicKeyB64u: string;
}

// Per-namespace rate limiter — extracted to relay/src/rate-limit.ts so
// unit tests can exercise it without spawning the full relay.
import { NamespaceRateLimiter } from "./rate-limit.ts";
const rateLimiter = new NamespaceRateLimiter({
  framesPerWindow: RATE_FRAMES_PER_WINDOW,
  windowMs: RATE_WINDOW_MS,
});

const subscribersByNs = new Map<string, Set<unknown>>();

function addSub(ns: string, ws: unknown): void {
  const set = subscribersByNs.get(ns) ?? new Set<unknown>();
  set.add(ws);
  subscribersByNs.set(ns, set);
}

function removeSub(ns: string, ws: unknown): void {
  const set = subscribersByNs.get(ns);
  if (!set) return;
  set.delete(ws);
  if (set.size === 0) subscribersByNs.delete(ns);
}

function broadcast(ns: string, relayFrame: RelayFrame, except: unknown): void {
  const set = subscribersByNs.get(ns);
  if (!set) return;
  const payload = JSON.stringify(relayFrame);
  for (const ws of set) {
    if (ws === except) continue;
    try {
      (ws as { send: (s: string) => void }).send(payload);
    } catch {
      // socket dying; will close itself
    }
  }
}

function send(ws: unknown, msg: RelayControl | RelayFrame): void {
  try {
    (ws as { send: (s: string) => void }).send(JSON.stringify(msg));
  } catch {}
}

/** Persist a frame + broadcast to live WS subscribers in the namespace.
 *  Used by the WS handler (when a connected client publishes) AND by the
 *  HTTP MCP handler (when an HTTP-only client posts via tools/call). */
function publishFrameAndBroadcast(ns: string, frame: ClientFrame, except?: unknown): number {
  const relayTs = Date.now();
  const frameId = insertFrame(ns, frame, relayTs);
  const broadcastFrame: RelayFrame = {
    v: PROTOCOL_VERSION,
    frame_id: frameId,
    relay_ts: relayTs,
    frame,
  };
  broadcast(ns, broadcastFrame, except);
  return frameId;
}

// ---------------- bun.serve ----------------

function bearerOf(req: Request): string | null {
  const h = req.headers.get("authorization");
  if (!h) return null;
  const m = /^Bearer\s+(.+)$/.exec(h);
  return m ? m[1]! : null;
}

// HTTP MCP handler (Phase N1b alpha). Built lazily once at startup so
// every request can reuse the same McpServer + transport.
const httpMcpHandler = await buildHttpMcpHandler({
  db,
  sharedSecret: SHARED_SECRET,
  namespace: namespaceForToken(SHARED_SECRET),
  publishFrame: (ns, frame) => publishFrameAndBroadcast(ns, frame),
});

const server = Bun.serve<WsData>({
  port: PORT,
  hostname: HOST,
  async fetch(req, srv) {
    const url = new URL(req.url);
    const token = bearerOf(req);
    if (url.pathname === "/healthz") return new Response(JSON.stringify({ ok: true }));
    if (url.pathname === "/mcp") {
      // Phase N1b: MCP-over-HTTP-SSE endpoint for claude.ai Connectors,
      // Claude Agent SDK, and any other MCP HTTP client. Auth check is
      // inside the handler; it 401s if the bearer is missing/bad.
      const resp = await httpMcpHandler(req);
      return resp ?? new Response("handler returned nothing", { status: 500 });
    }
    if (url.pathname === "/metrics") {
      // Prometheus-exposition-format text. Anyone on the bind interface
      // can scrape this; no auth (it's metadata, not bodies). Move
      // behind auth or a separate bind if your threat model needs it.
      const frames = db
        .query<{ namespace: string; n: number }, []>(
          "SELECT namespace, COUNT(*) AS n FROM frames GROUP BY namespace",
        )
        .all();
      const clients = db
        .query<{ namespace: string; n: number }, []>(
          "SELECT namespace, COUNT(*) AS n FROM pubkey_claims GROUP BY namespace",
        )
        .all();
      const lines: string[] = [
        "# HELP claudetalk_relay_frames_total Total stored frames per namespace.",
        "# TYPE claudetalk_relay_frames_total counter",
      ];
      for (const r of frames) {
        lines.push(`claudetalk_relay_frames_total{namespace="${r.namespace}"} ${r.n}`);
      }
      lines.push(
        "# HELP claudetalk_relay_known_pseudonyms TOFU-bound pseudonyms per namespace.",
        "# TYPE claudetalk_relay_known_pseudonyms gauge",
      );
      for (const r of clients) {
        lines.push(`claudetalk_relay_known_pseudonyms{namespace="${r.namespace}"} ${r.n}`);
      }
      let connected = 0;
      for (const set of subscribersByNs.values()) connected += set.size;
      lines.push(
        "# HELP claudetalk_relay_connected_clients Live WebSocket connections.",
        "# TYPE claudetalk_relay_connected_clients gauge",
        `claudetalk_relay_connected_clients ${connected}`,
      );
      return new Response(lines.join("\n") + "\n", {
        headers: { "content-type": "text/plain; version=0.0.4" },
      });
    }
    if (url.pathname === "/ws") {
      if (!token) return new Response("missing bearer", { status: 401 });
      const verified = verifyToken(token, SHARED_SECRET);
      if (!verified) return new Response("bad token", { status: 401 });
      // Pseudonym is derived client-side from the pubkey (Phase K3);
      // we trust it as a label and re-verify it matches the same pubkey
      // for every subsequent frame from this connection.
      const data: WsData = {
        namespace: namespaceForToken(SHARED_SECRET),
        pseudonym: "", // filled on first frame (where sender is asserted)
        publicKeyB64u: verified.publicKeyB64u,
      };
      if (srv.upgrade(req, { data })) return undefined;
      return new Response("upgrade failed", { status: 426 });
    }
    if (url.pathname === "/pull") {
      if (!token) return new Response("missing bearer", { status: 401 });
      const verified = verifyToken(token, SHARED_SECRET);
      if (!verified) return new Response("bad token", { status: 401 });
      const since = Number(url.searchParams.get("since") ?? 0);
      const ns = namespaceForToken(SHARED_SECRET);
      const frames = pullSince(ns, since);
      const next = frames.length > 0 ? frames[frames.length - 1]!.frame_id : since;
      const body: PullResponse = { v: PROTOCOL_VERSION, frames, next_since: next };
      return new Response(JSON.stringify(body), {
        headers: { "content-type": "application/json" },
      });
    }
    return new Response("not found", { status: 404 });
  },
  websocket: {
    open(ws) {
      addSub(ws.data.namespace, ws);
      const hello: RelayControl = { v: PROTOCOL_VERSION, control: "hello" };
      send(ws, hello);
    },
    message(ws, raw) {
      let frame: ClientFrame;
      try {
        frame = JSON.parse(typeof raw === "string" ? raw : raw.toString());
      } catch {
        return send(ws, { v: PROTOCOL_VERSION, control: "error", code: "malformed" });
      }
      if (frame.v !== PROTOCOL_VERSION || frame.kind !== "msg") {
        return send(ws, {
          v: PROTOCOL_VERSION,
          control: "error",
          code: "malformed",
          message: "only kind=msg supported in v1",
        });
      }
      // Per-namespace rate limit before doing any work.
      if (!rateLimiter.allow(ws.data.namespace)) {
        return send(ws, { v: PROTOCOL_VERSION, control: "error", code: "rate_limited" });
      }
      // Pin the pseudonym on first frame; reject changes.
      if (ws.data.pseudonym === "") ws.data.pseudonym = frame.sender;
      if (ws.data.pseudonym !== frame.sender) {
        return send(ws, {
          v: PROTOCOL_VERSION,
          control: "error",
          code: "pubkey_mismatch",
          message: "sender changed mid-connection",
        });
      }
      if (frame.public_key !== ws.data.publicKeyB64u) {
        return send(ws, {
          v: PROTOCOL_VERSION,
          control: "error",
          code: "pubkey_mismatch",
          message: "frame public_key does not match connection token pubkey",
        });
      }
      // TOFU: bind first pubkey seen for this (ns, pseudonym).
      if (!tofuPubkey(ws.data.namespace, frame.sender, frame.public_key)) {
        return send(ws, {
          v: PROTOCOL_VERSION,
          control: "error",
          code: "pubkey_mismatch",
          message: "pseudonym claimed a different public_key previously",
        });
      }
      // Signature verification: when the body is encrypted ("ct1:"
      // prefix, since v0.8.0 / Phase N2) the relay holds only
      // ciphertext and cannot reconstruct the bytes the sender
      // signed — those are over the plaintext body. In that case
      // we skip the sig check at the relay; recipients verify after
      // decrypt. The HMAC bearer token (namespace membership) +
      // pubkey TOFU still authenticate the SENDER; only the BODY
      // is no longer relay-attestable. For plaintext bodies (v0.7.0
      // back-compat) we still verify.
      const bodyIsEncrypted = frame.body.startsWith("ct1:");
      void (async () => {
        if (!bodyIsEncrypted) {
          const payload = messageSigningPayload({
            messageId: frame.ref_id,
            chatId: frame.target,
            authorPseudonym: frame.sender,
            body: frame.body,
            createdAt: frame.ts,
          });
          const ok = await verify(frame.public_key, payload, frame.sig);
          if (!ok) {
            return send(ws, { v: PROTOCOL_VERSION, control: "error", code: "bad_sig" });
          }
        }
        const frameId = publishFrameAndBroadcast(ws.data.namespace, frame, ws);
        // Ack the sender.
        send(ws, { v: PROTOCOL_VERSION, control: "ack", frame_id: frameId });
      })();
    },
    close(ws) {
      removeSub(ws.data.namespace, ws);
    },
  },
});

function namespaceForToken(secret: string): string {
  // Mirror src/relay-auth.ts:namespaceForSecret — kept inline to avoid
  // a circular import surface (this file is the relay's own entrypoint).
  // SHA-256(secret_bytes), base64url.
  return require("node:crypto")
    .createHash("sha256")
    .update(Buffer.from(secret, "base64url"))
    .digest("base64url");
}

log(`listening on ws://${HOST}:${server.port}/ws  (db=${DB_PATH})`);
log(`retention: ${RETENTION_DAYS} days; pull max: ${PULL_MAX_FRAMES} frames`);
