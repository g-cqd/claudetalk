/**
 * Phase N1 — HMAC bearer token format used between RelayClient and the
 * relay binary. Mint → verify round trip, tamper detection, timestamp
 * drift enforcement.
 */
import { expect, test } from "bun:test";
import { mintToken, namespaceForSecret, verifyToken } from "../../src/relay-auth.ts";

// 32 random bytes, base64url
const SECRET = "n5cXa3T5K6xKZ4WGn3xFlYyXp4QC4n9b7vT5W2m4u5w";
const PUBKEY = "X63UpSmMYd5lG2spNB33RyYSHj2yZIHEcPHGMogbTd4"; // 43 chars

test("mint → verify round trip", () => {
  const tok = mintToken({
    pseudonym: "SwiftFox-a3f",
    publicKeyB64u: PUBKEY,
    sharedSecret: SECRET,
  });
  const v = verifyToken(tok, SECRET);
  expect(v).not.toBeNull();
  expect(v!.publicKeyB64u).toBe(PUBKEY);
});

test("wrong shared secret fails verify", () => {
  const tok = mintToken({
    pseudonym: "SwiftFox-a3f",
    publicKeyB64u: PUBKEY,
    sharedSecret: SECRET,
  });
  const other = "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";
  expect(verifyToken(tok, other)).toBeNull();
});

test("expired token (outside ±30s window) fails", () => {
  const tok = mintToken({
    pseudonym: "SwiftFox-a3f",
    publicKeyB64u: PUBKEY,
    sharedSecret: SECRET,
    nowSec: 1_000_000,
  });
  // 60s later → outside ±30s window
  expect(verifyToken(tok, SECRET, 1_000_060)).toBeNull();
  // within window
  expect(verifyToken(tok, SECRET, 1_000_020)).not.toBeNull();
});

test("tampered MAC fails", () => {
  const tok = mintToken({
    pseudonym: "SwiftFox-a3f",
    publicKeyB64u: PUBKEY,
    sharedSecret: SECRET,
  });
  // Decode → flip a byte in the MAC region (last 32 bytes) → re-encode.
  // Mutating a base64url char can be a no-op if it maps to the same
  // byte; mutating the raw byte is reliable.
  const bytes = new Uint8Array(Buffer.from(tok, "base64url"));
  bytes[bytes.length - 1] = bytes[bytes.length - 1]! ^ 0xff;
  const tampered = Buffer.from(bytes).toString("base64url");
  expect(verifyToken(tampered, SECRET)).toBeNull();
});

test("namespaceForSecret is deterministic", () => {
  const a = namespaceForSecret(SECRET);
  const b = namespaceForSecret(SECRET);
  expect(a).toBe(b);
  expect(a).not.toBe(namespaceForSecret("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
});
