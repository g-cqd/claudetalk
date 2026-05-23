/**
 * Phase K0+K1: Ed25519 keypair derivation, sign/verify round trip,
 * deterministic per (machine_seed, folder_path).
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  getKeyPairForFolder,
  messageSigningPayload,
  sign,
  verify,
} from "../../src/keys.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
});

afterEach(() => {
  home.cleanup();
});

test("keypair generation is deterministic for the same folder", async () => {
  const k1 = await getKeyPairForFolder("/Users/alice/projects/foo");
  const k2 = await getKeyPairForFolder("/Users/alice/projects/foo");
  expect(k1.publicKey).toBe(k2.publicKey);
  expect(k1.publicKey.length).toBeGreaterThan(30); // base64url ~43 chars
});

test("different folders give different keypairs", async () => {
  const k1 = await getKeyPairForFolder("/Users/alice/projects/foo");
  const k2 = await getKeyPairForFolder("/Users/alice/projects/bar");
  expect(k1.publicKey).not.toBe(k2.publicKey);
});

test("sign + verify round-trip", async () => {
  const k = await getKeyPairForFolder("/Users/alice/projects/foo");
  const payload = messageSigningPayload({
    messageId: "uuid-1",
    chatId: "group:alpha",
    authorPseudonym: "SwiftFox-a3f",
    body: "hello",
    createdAt: 1_700_000_000_000,
  });
  const sig = await sign(k.privateKey, payload);
  expect(typeof sig).toBe("string");
  expect(await verify(k.publicKey, payload, sig)).toBe(true);
});

test("verify fails on tampered body", async () => {
  const k = await getKeyPairForFolder("/Users/alice/projects/foo");
  const original = messageSigningPayload({
    messageId: "uuid-1",
    chatId: "group:alpha",
    authorPseudonym: "SwiftFox-a3f",
    body: "hello",
    createdAt: 1_700_000_000_000,
  });
  const tampered = messageSigningPayload({
    messageId: "uuid-1",
    chatId: "group:alpha",
    authorPseudonym: "SwiftFox-a3f",
    body: "GOODBYE", // body changed
    createdAt: 1_700_000_000_000,
  });
  const sig = await sign(k.privateKey, original);
  expect(await verify(k.publicKey, tampered, sig)).toBe(false);
});

test("verify fails with the wrong public key", async () => {
  const a = await getKeyPairForFolder("/Users/alice/projects/foo");
  const b = await getKeyPairForFolder("/Users/alice/projects/bar");
  const payload = messageSigningPayload({
    messageId: "uuid-1",
    chatId: "group:alpha",
    authorPseudonym: "SwiftFox-a3f",
    body: "hello",
    createdAt: 1_700_000_000_000,
  });
  const sig = await sign(a.privateKey, payload);
  expect(await verify(b.publicKey, payload, sig)).toBe(false);
});

test("machine_seed is persisted to machine.json on backfill", async () => {
  const { readFileSync, writeFileSync } = await import("node:fs");
  const { join } = await import("node:path");
  // Seed a v0.5.x-style machine.json with NO machine_seed.
  writeFileSync(
    join(home.home, "machine.json"),
    JSON.stringify(
      { machine_id: "abc-123", hostname: "test", created_at: "2026-01-01T00:00:00Z" },
      null,
      2,
    ) + "\n",
  );
  await getKeyPairForFolder("/Users/alice/projects/foo");
  const after = JSON.parse(readFileSync(join(home.home, "machine.json"), "utf8"));
  expect(typeof after.machine_seed).toBe("string");
  expect(after.machine_seed.length).toBeGreaterThan(40);
});
