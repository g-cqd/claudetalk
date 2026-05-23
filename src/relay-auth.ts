/**
 * HMAC-SHA256 bearer token format shared between RelayClient (client)
 * and the relay binary (server). All four fields (pseudonym, ts, pubkey,
 * mac) are packed into a single base64url string passed in the
 * `Authorization: Bearer <token>` header. ±30 s timestamp window for
 * anti-replay. (Phase N1.)
 *
 * Token layout (in base64url decoding order):
 *   - sha256(pseudonym) (32 bytes)
 *   - timestamp seconds (8 bytes, big-endian)
 *   - public key (32 bytes, raw Ed25519)
 *   - HMAC-SHA256(shared_secret, first 72 bytes) (32 bytes)
 *
 * Total: 104 bytes → 140 base64url chars.
 */
import { createHash, createHmac } from "node:crypto";

const TS_DRIFT_SECONDS = 30;

export interface TokenInput {
  pseudonym: string;
  publicKeyB64u: string; // base64url, 43 chars
  sharedSecret: string; // base64url
  /** Override timestamp for tests; defaults to now. */
  nowSec?: number;
}

export interface VerifiedToken {
  pseudonym: string;
  pseudonymHash: Uint8Array;
  timestamp: number;
  publicKeyB64u: string;
  publicKeyBytes: Uint8Array;
}

function b64u(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64url");
}

function fromB64u(s: string): Uint8Array {
  return new Uint8Array(Buffer.from(s, "base64url"));
}

function uint64BE(n: number): Uint8Array {
  const buf = new Uint8Array(8);
  const view = new DataView(buf.buffer);
  view.setBigUint64(0, BigInt(n), false);
  return buf;
}

function readUint64BE(buf: Uint8Array, offset: number): number {
  const view = new DataView(buf.buffer, buf.byteOffset + offset, 8);
  return Number(view.getBigUint64(0, false));
}

/** Build a token from a (pseudonym, pubkey, shared-secret) tuple. */
export function mintToken(input: TokenInput): string {
  const pseudonymHash = createHash("sha256").update(input.pseudonym).digest();
  const ts = input.nowSec ?? Math.floor(Date.now() / 1000);
  const tsBytes = uint64BE(ts);
  const pkBytes = fromB64u(input.publicKeyB64u);
  if (pkBytes.length !== 32) {
    throw new Error(`mintToken: expected 32-byte public key, got ${pkBytes.length}`);
  }
  const signed = new Uint8Array(32 + 8 + 32);
  signed.set(pseudonymHash, 0);
  signed.set(tsBytes, 32);
  signed.set(pkBytes, 40);
  const secretBytes = fromB64u(input.sharedSecret);
  const mac = createHmac("sha256", secretBytes).update(signed).digest();
  const token = new Uint8Array(signed.length + 32);
  token.set(signed, 0);
  token.set(mac, signed.length);
  return b64u(token);
}

/** Verify a token. Returns the unpacked claim on success, or null on
 *  any failure (bad MAC, expired, malformed). */
export function verifyToken(
  tokenB64u: string,
  sharedSecret: string,
  nowSec: number = Math.floor(Date.now() / 1000),
): VerifiedToken | null {
  let token: Uint8Array;
  try {
    token = fromB64u(tokenB64u);
  } catch {
    return null;
  }
  if (token.length !== 104) return null;
  const signed = token.slice(0, 72);
  const presentedMac = token.slice(72);
  const secretBytes = fromB64u(sharedSecret);
  const expectedMac = new Uint8Array(
    createHmac("sha256", secretBytes).update(signed).digest(),
  );
  if (!constantTimeEqual(presentedMac, expectedMac)) return null;
  const pseudonymHash = signed.slice(0, 32);
  const ts = readUint64BE(signed, 32);
  if (Math.abs(nowSec - ts) > TS_DRIFT_SECONDS) return null;
  const publicKeyBytes = signed.slice(40);
  return {
    pseudonym: "", // caller knows the pseudonym from the connection context
    pseudonymHash,
    timestamp: ts,
    publicKeyB64u: b64u(publicKeyBytes),
    publicKeyBytes,
  };
}

function constantTimeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/** Derive the namespace hash from a shared secret. The relay uses this
 *  to partition connections — two machines with the same shared secret
 *  share a namespace; different secrets don't see each other's frames. */
export function namespaceForSecret(sharedSecret: string): string {
  const secretBytes = fromB64u(sharedSecret);
  return createHash("sha256").update(secretBytes).digest("base64url");
}
