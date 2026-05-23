/**
 * Wire format shared between RelayClient (in the MCP server) and the
 * relay binary. Versioned so future fields can be added without breaking
 * old clients. Frames serialise as line-delimited JSON over WebSocket.
 *
 * KIND TAGS (forward-compatible):
 *   "msg"      — chat message
 *   "ask"      — one-shot ask (future)
 *   "answer"   — answer to an ask (future)
 *   "presence" — heartbeat / status update (future)
 *   "reaction" — reaction add/remove (future)
 *
 * Only "msg" is implemented in v0.7.0; the others are stubbed so the
 * protocol can grow without a version bump.
 */

export const PROTOCOL_VERSION = "1";

/** Outbound client → relay. Author MUST hold the private key matching
 *  `public_key`; the body is signed via messageSigningPayload(...). */
export interface ClientFrame {
  v: typeof PROTOCOL_VERSION;
  kind: "msg" | "ask" | "answer" | "presence" | "reaction";
  /** Author's pseudonym (key-derived since v0.6.1). */
  sender: string;
  /** Sender's published public key, base64url. Relay TOFU-binds first
   *  pubkey it sees for a (namespace, sender) pair; subsequent frames
   *  must match. */
  public_key: string;
  /** Chat id (for msg/reaction); for ask/answer, the recipient pseudonym. */
  target: string;
  /** Message UUID for "msg"; for "ask"/"answer" the relay-assigned global id. */
  ref_id: string;
  /** For "msg", body content (possibly ciphertext in Phase N2). */
  body: string;
  /** Client-side Unix ms timestamp. Relay echoes back with its own ts. */
  ts: number;
  /** Ed25519 signature over messageSigningPayload({ messageId: ref_id,
   *  chatId: target, authorPseudonym: sender, body, createdAt: ts }),
   *  base64url. Relay rejects frames whose sig doesn't verify. */
  sig: string;
}

/** Relay → client broadcast envelope. Adds the relay's monotonic
 *  frame_id (for /pull catch-up) and relay-side timestamp (for clock-skew
 *  diagnostics). The inner ClientFrame is verbatim what the publishing
 *  client sent — receivers can re-verify the signature against
 *  `frame.public_key`. */
export interface RelayFrame {
  v: typeof PROTOCOL_VERSION;
  frame_id: number;
  relay_ts: number;
  frame: ClientFrame;
}

/** Relay → client control messages (acks, errors). */
export interface RelayControl {
  v: typeof PROTOCOL_VERSION;
  control: "ack" | "error" | "hello";
  /** For "ack": the frame_id the relay assigned. */
  frame_id?: number;
  /** For "error": short machine-readable code. */
  code?:
    | "bad_token"
    | "bad_sig"
    | "pubkey_mismatch"
    | "rate_limited"
    | "malformed"
    | "unknown";
  /** For "error": human-readable detail. */
  message?: string;
}

export type AnyServerMessage = RelayFrame | RelayControl;

/** Pull response envelope for HTTP GET /pull?since=N. */
export interface PullResponse {
  v: typeof PROTOCOL_VERSION;
  frames: RelayFrame[];
  /** Highest frame_id in this response. Use as the next `since` cursor. */
  next_since: number;
}

export function isRelayFrame(msg: AnyServerMessage): msg is RelayFrame {
  return "frame" in msg && "frame_id" in msg;
}

export function isRelayControl(msg: AnyServerMessage): msg is RelayControl {
  return "control" in msg;
}
