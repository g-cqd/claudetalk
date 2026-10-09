/**
 * Phase N2: client-side AES-GCM encryption of message bodies before they
 * leave for the relay. Key is HKDF-derived from the shared secret so
 * every machine in the namespace can decrypt; the relay sees only
 * ciphertext + a random 12-byte IV.
 *
 * Wire format for an encrypted body (string carried in ClientFrame.body):
 *   "ct1:<base64url(iv || ciphertext_plus_tag)>"
 *
 * Where:
 *   * "ct1:" is the version prefix — bump if the cipher / KDF / payload
 *     framing ever changes.
 *   * iv is 12 random bytes (96 bits, AES-GCM nonce).
 *   * ciphertext_plus_tag is AES-GCM(key, iv, plaintext) — Web Crypto
 *     returns the auth tag appended to the ciphertext.
 *
 * Plaintext bodies (legacy unsigned rows + pre-N2 traffic) are passed
 * through unchanged so receivers can detect them by the missing
 * "ct1:" prefix.
 */
import type { KeyPair } from "./keys.ts";

const ENC_PREFIX = "ct1:";

function b64u(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64url");
}

function fromB64u(s: string): Uint8Array {
  return new Uint8Array(Buffer.from(s, "base64url"));
}

/** Lazy-cached AES-GCM key derived once per process from the shared
 *  secret. Different secrets in different namespaces would derive
 *  different keys; the relay never sees either. */
let cachedKey: { secret: string; key: CryptoKey } | null = null;

async function getEncryptionKey(sharedSecretB64u: string): Promise<CryptoKey> {
  if (cachedKey?.secret === sharedSecretB64u) return cachedKey.key;
  const secretBytes = fromB64u(sharedSecretB64u);
  const baseKey = await crypto.subtle.importKey(
    "raw",
    secretBytes as unknown as ArrayBuffer,
    "HKDF",
    false,
    ["deriveKey"],
  );
  const key = await crypto.subtle.deriveKey(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: new TextEncoder().encode("claudetalk:body-encryption:v1"),
      info: new Uint8Array(0),
    },
    baseKey,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
  cachedKey = { secret: sharedSecretB64u, key };
  return key;
}

/** Encrypt a plaintext body. Returns the ciphertext string ready to
 *  drop into ClientFrame.body. */
export async function encryptBody(
  sharedSecretB64u: string,
  plaintext: string,
): Promise<string> {
  const key = await getEncryptionKey(sharedSecretB64u);
  const iv = new Uint8Array(12);
  crypto.getRandomValues(iv);
  const ctBuf = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: iv as unknown as ArrayBuffer },
    key,
    new TextEncoder().encode(plaintext) as unknown as ArrayBuffer,
  );
  const ct = new Uint8Array(ctBuf);
  const combined = new Uint8Array(iv.length + ct.length);
  combined.set(iv, 0);
  combined.set(ct, iv.length);
  return ENC_PREFIX + b64u(combined);
}

/** Detect whether a body is encrypted (has the version prefix). */
export function isEncrypted(body: string): boolean {
  return body.startsWith(ENC_PREFIX);
}

/** Decrypt a "ct1:"-prefixed body. Returns the original plaintext.
 *  Throws on auth-tag mismatch, malformed input, or wrong key. Caller
 *  treats throw as "drop frame; we cannot trust this body". */
export async function decryptBody(
  sharedSecretB64u: string,
  encryptedBody: string,
): Promise<string> {
  if (!isEncrypted(encryptedBody)) {
    throw new Error("decryptBody: not a ct1:-prefixed body");
  }
  const combined = fromB64u(encryptedBody.slice(ENC_PREFIX.length));
  if (combined.length <= 12) throw new Error("decryptBody: payload too short");
  const iv = combined.slice(0, 12);
  const ct = combined.slice(12);
  const key = await getEncryptionKey(sharedSecretB64u);
  const ptBuf = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: iv as unknown as ArrayBuffer },
    key,
    ct as unknown as ArrayBuffer,
  );
  return new TextDecoder().decode(ptBuf);
}

/** Reset the cached key. Tests use this between cases when the
 *  shared-secret-under-test changes. */
export function _resetKeyCacheForTests(): void {
  cachedKey = null;
}

// keyPair import kept for the eventual per-session key wrapping path
// (Phase N2b — out-of-namespace guest invites).
type _Unused = KeyPair;
void (null as unknown as _Unused);
