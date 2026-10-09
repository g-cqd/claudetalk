/**
 * Phase N2 — AES-GCM body encryption keyed via HKDF from the shared
 * secret. Same secret on every machine in the namespace; the relay
 * holds only ciphertext.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  _resetKeyCacheForTests,
  decryptBody,
  encryptBody,
  isEncrypted,
} from "../../src/relay-crypto.ts";

const SECRET_A = "n5cXa3T5K6xKZ4WGn3xFlYyXp4QC4n9b7vT5W2m4u5w";
const SECRET_B = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

beforeEach(() => _resetKeyCacheForTests());
afterEach(() => _resetKeyCacheForTests());

test("encrypt → decrypt round trip", async () => {
  const ct = await encryptBody(SECRET_A, "hello, claudetalk");
  expect(isEncrypted(ct)).toBe(true);
  expect(ct).toContain("ct1:");
  expect(ct).not.toContain("hello");
  const pt = await decryptBody(SECRET_A, ct);
  expect(pt).toBe("hello, claudetalk");
});

test("encryptions of the same plaintext produce different ciphertexts (random IV)", async () => {
  const a = await encryptBody(SECRET_A, "same input");
  const b = await encryptBody(SECRET_A, "same input");
  expect(a).not.toBe(b);
});

test("decrypt with the wrong shared secret fails", async () => {
  const ct = await encryptBody(SECRET_A, "secret message");
  _resetKeyCacheForTests();
  await expect(decryptBody(SECRET_B, ct)).rejects.toThrow();
});

test("isEncrypted detects the ct1: prefix", () => {
  expect(isEncrypted("ct1:somethingsomething")).toBe(true);
  expect(isEncrypted("plaintext message")).toBe(false);
  expect(isEncrypted("")).toBe(false);
});

test("decryptBody throws on missing prefix", async () => {
  await expect(decryptBody(SECRET_A, "plain")).rejects.toThrow();
});

test("decryptBody throws on tampered ciphertext (auth tag mismatch)", async () => {
  const ct = await encryptBody(SECRET_A, "do not tamper");
  // Decode → flip a byte in the ciphertext region → re-encode.
  const body = ct.slice("ct1:".length);
  const bytes = new Uint8Array(Buffer.from(body, "base64url"));
  // Flip somewhere past the 12-byte IV so we're hitting ciphertext.
  bytes[20] = bytes[20]! ^ 0xff;
  const tampered = "ct1:" + Buffer.from(bytes).toString("base64url");
  await expect(decryptBody(SECRET_A, tampered)).rejects.toThrow();
});
