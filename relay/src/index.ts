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

const PORT = Number(process.env.RELAY_PORT ?? 7878);
const HOST = process.env.RELAY_HOST ?? "0.0.0.0";
const SHARED_SECRET = process.env.RELAY_SHARED_SECRET ?? "";
const DB_PATH = process.env.RELAY_DB_PATH ?? "relay_db.sqlite";
const RETENTION_DAYS = Number(process.env.RELAY_RETENTION_DAYS ?? 30);
const PULL_MAX_FRAMES = 500;

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
  return Number(r.lastInsertRowid);
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

// ---------------- bun.serve ----------------

function bearerOf(req: Request): string | null {
  const h = req.headers.get("authorization");
  if (!h) return null;
  const m = /^Bearer\s+(.+)$/.exec(h);
  return m ? m[1]! : null;
}

const server = Bun.serve<WsData>({
  port: PORT,
  hostname: HOST,
  fetch(req, srv) {
    const url = new URL(req.url);
    const token = bearerOf(req);
    if (url.pathname === "/healthz") return new Response(JSON.stringify({ ok: true }));
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
      // Verify the Ed25519 signature.
      const payload = messageSigningPayload({
        messageId: frame.ref_id,
        chatId: frame.target,
        authorPseudonym: frame.sender,
        body: frame.body,
        createdAt: frame.ts,
      });
      void (async () => {
        const ok = await verify(frame.public_key, payload, frame.sig);
        if (!ok) {
          return send(ws, { v: PROTOCOL_VERSION, control: "error", code: "bad_sig" });
        }
        const relayTs = Date.now();
        const frameId = insertFrame(ws.data.namespace, frame, relayTs);
        const broadcastFrame: RelayFrame = {
          v: PROTOCOL_VERSION,
          frame_id: frameId,
          relay_ts: relayTs,
          frame,
        };
        // Ack the sender + broadcast to everyone else in the namespace.
        send(ws, { v: PROTOCOL_VERSION, control: "ack", frame_id: frameId });
        broadcast(ws.data.namespace, broadcastFrame, ws);
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
