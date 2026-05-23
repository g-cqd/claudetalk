/**
 * Outbound WebSocket client to the ClaudeTalk relay. Held by the MCP
 * server process; bridges local SQLite writes to the relay so other
 * machines see them in real time, and ingests inbound frames into local
 * SQLite so local hooks/tools render them like any other message.
 *
 * Lifecycle:
 *   * Constructed eagerly in src/server.ts main() iff network.json is
 *     present. Constructor returns immediately; the WS connect is async.
 *   * Reconnect with exponential back-off (1s → 30s, jitter).
 *   * Outbound queue: writes accumulate locally while disconnected;
 *     drained on reconnect.
 *   * On (re)connect: GET /pull?since=<last_frame_id> to catch up
 *     missed frames before resuming live broadcast.
 *
 * Design tradeoff: this client is best-effort — local SQLite remains
 * the source of truth for tools/hooks. If the relay never comes back,
 * the user keeps using their local data; cross-machine sync simply
 * stalls until the relay returns.
 */
import type { Identity } from "./pseudonym.ts";
import {
  type ClientFrame,
  type AnyServerMessage,
  PROTOCOL_VERSION,
  isRelayControl,
  isRelayFrame,
  type PullResponse,
} from "./relay-protocol.ts";
import { mintToken, namespaceForSecret } from "./relay-auth.ts";
import { messageSigningPayload, verify } from "./keys.ts";
import {
  addChatMember,
  db,
  ensureChat,
  getInstance,
  insertMessage,
  upsertInstance,
} from "./db.ts";
import { recordMessageMentions } from "./mentions.ts";
import { getOrCreateMachineId } from "./machine-id.ts";

interface ConfigBundle {
  relayUrl: string;
  sharedSecret: string;
}

interface OutboundEntry {
  frame: ClientFrame;
  /** Resolves with the relay's assigned frame_id once ack'd, or rejects
   *  on permanent failure (bad token, bad sig). Used by callers that
   *  want to know when a publish was durable on the relay. */
  done: { resolve: (id: number) => void; reject: (e: Error) => void };
}

/** Outbound queue cap. Beyond this we drop oldest with a single warn
 *  (mirrors the audit-log queue cap from v0.5.3). */
const OUTBOUND_CAP = 5_000;
const RECONNECT_MIN_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;

export class RelayClient {
  private ws: WebSocket | null = null;
  private outbound: OutboundEntry[] = [];
  private reconnectDelay = RECONNECT_MIN_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  private lastFrameId = 0;
  private overflowWarned = false;
  private readonly namespace: string;
  /** Map ack-pending frame UUIDs (ref_id) to their done promise so we
   *  can resolve when the relay sends back the assigned frame_id. */
  private readonly pendingAcks = new Map<string, OutboundEntry["done"]>();

  constructor(
    private readonly me: Identity,
    private readonly config: ConfigBundle,
  ) {
    if (!me.keyPair) {
      throw new Error("RelayClient requires an Identity with a keyPair attached");
    }
    this.namespace = namespaceForSecret(config.sharedSecret);
    void this.connect();
  }

  /** Publish a chat message frame. Resolves with the relay-assigned
   *  frame_id, or null if the client is permanently closed. */
  async publishMessage(args: {
    messageId: string;
    chatId: string;
    body: string;
    createdAt: number;
    signature: string;
  }): Promise<number | null> {
    if (this.closed) return null;
    const frame: ClientFrame = {
      v: PROTOCOL_VERSION,
      kind: "msg",
      sender: this.me.pseudonym,
      public_key: this.me.keyPair!.publicKey,
      target: args.chatId,
      ref_id: args.messageId,
      body: args.body,
      ts: args.createdAt,
      sig: args.signature,
    };
    return new Promise<number | null>((resolve, reject) => {
      this.enqueue({
        frame,
        done: {
          resolve: (id) => resolve(id),
          reject: (e) => reject(e),
        },
      });
    });
  }

  /** Gracefully shut down: stop reconnecting, flush nothing (queue
   *  persists in memory only — caller can re-instantiate to retry). */
  close(): void {
    this.closed = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    try {
      this.ws?.close();
    } catch {}
    this.ws = null;
    // Reject any in-flight publishes so callers don't await forever.
    for (const [, done] of this.pendingAcks) {
      done.reject(new Error("RelayClient closed"));
    }
    this.pendingAcks.clear();
  }

  // ---------------- internals ----------------

  private enqueue(entry: OutboundEntry): void {
    if (this.outbound.length >= OUTBOUND_CAP) {
      const dropped = this.outbound.shift();
      dropped?.done.reject(new Error("outbound queue overflow"));
      if (!this.overflowWarned) {
        this.overflowWarned = true;
        console.error(
          `[claudetalk.relay-client] outbound queue hit ${OUTBOUND_CAP}; dropping oldest. Relay likely down.`,
        );
      }
    } else if (this.overflowWarned && this.outbound.length < OUTBOUND_CAP / 2) {
      this.overflowWarned = false;
    }
    this.outbound.push(entry);
    if (this.ws?.readyState === WebSocket.OPEN) this.flush();
  }

  private flush(): void {
    while (this.outbound.length > 0 && this.ws?.readyState === WebSocket.OPEN) {
      const entry = this.outbound.shift()!;
      this.pendingAcks.set(entry.frame.ref_id, entry.done);
      try {
        this.ws.send(JSON.stringify(entry.frame));
      } catch {
        // Put it back; we'll retry on next OPEN.
        this.pendingAcks.delete(entry.frame.ref_id);
        this.outbound.unshift(entry);
        return;
      }
    }
  }

  private async connect(): Promise<void> {
    if (this.closed) return;
    const token = mintToken({
      pseudonym: this.me.pseudonym,
      publicKeyB64u: this.me.keyPair!.publicKey,
      sharedSecret: this.config.sharedSecret,
    });
    // Pull catch-up via HTTP before opening the WS so any frames sent
    // while we were offline get into local SQLite before we start
    // accepting live ones (which would otherwise interleave out of order).
    try {
      await this.pullCatchup(token);
    } catch (e) {
      // Catch-up failed; we'll still try the WS but warn.
      console.error("[claudetalk.relay-client] catch-up pull failed:", e);
    }

    const wsUrl = this.config.relayUrl.replace(/\/+$/, "") + "/ws";
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl, {
        headers: { Authorization: `Bearer ${token}` },
      } as any); // Bun WebSocket supports init { headers }
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;

    ws.addEventListener("open", () => {
      this.reconnectDelay = RECONNECT_MIN_MS;
      this.flush();
    });
    ws.addEventListener("message", (ev) => {
      try {
        const msg = JSON.parse(String(ev.data)) as AnyServerMessage;
        this.handleServerMessage(msg);
      } catch {
        // ignore malformed
      }
    });
    ws.addEventListener("close", () => {
      this.ws = null;
      this.scheduleReconnect();
    });
    ws.addEventListener("error", () => {
      try {
        ws.close();
      } catch {}
    });
  }

  private scheduleReconnect(): void {
    if (this.closed) return;
    if (this.reconnectTimer !== null) return;
    const jitter = Math.random() * this.reconnectDelay * 0.25;
    const delay = Math.min(RECONNECT_MAX_MS, this.reconnectDelay + jitter);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.reconnectDelay = Math.min(RECONNECT_MAX_MS, this.reconnectDelay * 2);
      void this.connect();
    }, delay);
    if (typeof this.reconnectTimer === "object" && "unref" in this.reconnectTimer) {
      (this.reconnectTimer as { unref: () => void }).unref();
    }
  }

  private handleServerMessage(msg: AnyServerMessage): void {
    if (isRelayControl(msg)) {
      if (msg.control === "ack" && msg.frame_id !== undefined) {
        // Find the corresponding pending publish. We use ref_id but the
        // control message doesn't carry it explicitly; the relay sends
        // acks in order, so resolve the oldest pending.
        const next = this.pendingAcks.entries().next().value;
        if (next) {
          const [refId, done] = next;
          this.pendingAcks.delete(refId);
          done.resolve(msg.frame_id);
        }
        this.lastFrameId = Math.max(this.lastFrameId, msg.frame_id);
      } else if (msg.control === "error") {
        console.error(
          `[claudetalk.relay-client] relay error: ${msg.code} ${msg.message ?? ""}`,
        );
      }
      return;
    }
    if (isRelayFrame(msg)) {
      this.lastFrameId = Math.max(this.lastFrameId, msg.frame_id);
      void this.ingestFrame(msg.frame).catch(() => {});
    }
  }

  /** Catch up missed frames via HTTP before the WS opens. */
  private async pullCatchup(token: string): Promise<void> {
    const url = `${this.config.relayUrl.replace(/\/+$/, "")}/pull?since=${this.lastFrameId}`;
    const res = await fetch(url, {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (!res.ok) throw new Error(`pull HTTP ${res.status}`);
    const body = (await res.json()) as PullResponse;
    for (const rf of body.frames) {
      this.lastFrameId = Math.max(this.lastFrameId, rf.frame_id);
      try {
        await this.ingestFrame(rf.frame);
      } catch {
        // best-effort
      }
    }
  }

  /** Verify a received frame's signature and persist it locally. */
  private async ingestFrame(frame: ClientFrame): Promise<void> {
    // Verify signature against the sender's pubkey (TOFU: pubkey is
    // carried in the frame; relay enforces consistency across frames).
    if (frame.kind !== "msg") return; // others not implemented in v0.7.0
    const payload = messageSigningPayload({
      messageId: frame.ref_id,
      chatId: frame.target,
      authorPseudonym: frame.sender,
      body: frame.body,
      createdAt: frame.ts,
    });
    const ok = await verify(frame.public_key, payload, frame.sig);
    if (!ok) {
      console.error(
        `[claudetalk.relay-client] dropping frame ${frame.ref_id}: bad signature from ${frame.sender}`,
      );
      return;
    }
    // Stamp the author into instances so listInstances / discover sees them.
    upsertInstance(frame.sender, "(remote)", 0, null, frame.public_key);
    // Ensure the chat exists locally and the author is a member.
    ensureChat(frame.target, frame.target.startsWith("group:") ? "group" : "direct", null);
    addChatMember(frame.target, frame.sender);
    if (frame.target.startsWith("direct:")) {
      // Direct chats include both pseudonyms in the id; add the other one too.
      const ids = frame.target.replace(/^direct:/, "").split("|");
      for (const id of ids) addChatMember(frame.target, id);
    }
    // Idempotent insert: if the same frame.ref_id already exists, skip
    // (the relay's ack to our own publish + the broadcast we receive of
    // OUR own frame both flow through ingestFrame).
    const existing = db()
      .query<{ id: string }, [string]>("SELECT id FROM messages WHERE id = ?")
      .get(frame.ref_id);
    if (existing) return;
    insertMessage(
      frame.target,
      frame.sender,
      frame.body,
      null, // parent_id — Phase N2 will add threading propagation
      frame.sig,
      frame.ref_id,
      frame.ts,
    );
    recordMessageMentions(frame.ref_id, frame.body, frame.sender);
  }
}

// Suppress unused warning on getInstance / getOrCreateMachineId until N1b uses them.
void getInstance;
void getOrCreateMachineId;
