/**
 * Per-machine, per-folder Ed25519 keypair. Deterministic from
 * `HKDF(machine_seed, "claudetalk:v1:" + folder_path)` so the same folder
 * re-opened later regenerates the same key without persisting it. The
 * machine_seed lives in ~/.claudetalk/machine.json (also stores
 * machine_id) and is what makes the keypair stable across runs.
 *
 * Why Ed25519, not GPG: native to Bun's Web Crypto, 64-byte signatures,
 * <1ms verify, no external `gpg` binary, no key-server / web-of-trust UX.
 *
 * Phase K0 (v0.6.0): generate + persist pubkey alongside the pseudonym.
 * Phase K1: sign messages on insert.
 * Phase K3 (later): pseudonym derives from pubkey (the "key IS identity"
 * choice). For now pseudonym stays path-derived; pubkey is an additional
 * identity claim the relay (Phase N1+K4) will verify.
 *
 * Security audit M6 + M10 close once the relay verifies the pubkey
 * claim — at that point, pseudonym becomes a routing label that's
 * cryptographically bound to a key the holder controls.
 */
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { hostname } from "node:os";
import { join } from "node:path";
import { readJsonBounded } from "./safe-json.ts";

interface MachineFile {
  machine_id: string;
  hostname: string;
  created_at: string;
  /** Base64-encoded 32-byte machine secret. Used as HKDF input for
   *  per-folder keypair derivation. Added in v0.6.0; older machine.json
   *  files (v0.5.x) won't have it and will get one written on first key
   *  request. */
  machine_seed?: string;
}

function rootDir(): string {
  return process.env.CLAUDETALK_HOME ?? `${process.env.HOME}/.claudetalk`;
}

function machineFilePath(): string {
  return join(rootDir(), "machine.json");
}

function b64u(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64url");
}

function fromB64u(s: string): Uint8Array {
  return new Uint8Array(Buffer.from(s, "base64url"));
}

/** Ensure the machine.json has a `machine_seed` field. Creates the file
 *  with both a machine_id and a machine_seed if absent (matching the shape
 *  src/machine-id.ts would produce). Backfills the seed into an existing
 *  v0.5.x file that lacks it. Always persists so subsequent calls return
 *  the same value. */
function getOrCreateMachineSeed(): Uint8Array {
  const path = machineFilePath();
  let file: MachineFile | null = existsSync(path) ? readJsonBounded<MachineFile>(path) : null;
  if (file === null) {
    // First-ever boot under v0.6.0 — write a full machine.json shape.
    // machine-id.ts will read this and not overwrite (idempotent).
    mkdirSync(rootDir(), { recursive: true });
    const seed = new Uint8Array(32);
    crypto.getRandomValues(seed);
    file = {
      machine_id: crypto.randomUUID(),
      hostname: hostname(),
      created_at: new Date().toISOString(),
      machine_seed: b64u(seed),
    };
    writeFileSync(path, JSON.stringify(file, null, 2) + "\n", { mode: 0o600 });
    return seed;
  }
  if (typeof file.machine_seed === "string" && file.machine_seed.length > 0) {
    return fromB64u(file.machine_seed);
  }
  // Backfill: write a new seed into the existing file.
  const seed = new Uint8Array(32);
  crypto.getRandomValues(seed);
  const updated: MachineFile = { ...file, machine_seed: b64u(seed) };
  writeFileSync(path, JSON.stringify(updated, null, 2) + "\n", { mode: 0o600 });
  return seed;
}

/** Derive a 32-byte Ed25519 seed from the machine seed + folder path
 *  via HKDF-SHA256. */
async function deriveSeed(folderPath: string): Promise<Uint8Array> {
  const machineSeed = getOrCreateMachineSeed();
  const baseKey = await crypto.subtle.importKey(
    "raw",
    machineSeed as unknown as ArrayBuffer,
    "HKDF",
    false,
    ["deriveBits"],
  );
  const bits = await crypto.subtle.deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: new TextEncoder().encode("claudetalk:keys:v1"),
      info: new TextEncoder().encode(folderPath),
    },
    baseKey,
    256,
  );
  return new Uint8Array(bits);
}

export interface KeyPair {
  /** Ed25519 public key, base64url-encoded (43 chars). */
  publicKey: string;
  /** Ed25519 private key handle (Web Crypto, non-extractable except via
   *  jwk export which is gated by `extractable: true` at import). */
  privateKey: CryptoKey;
  /** Raw 32-byte public key bytes (for direct use with crypto.subtle.verify). */
  publicKeyBytes: Uint8Array;
  /** Raw 32-byte private-key seed (the HKDF output we derived from). Used
   *  to re-derive the same keypair later. NEVER share this. */
  seed: Uint8Array;
}

/** Get the deterministic Ed25519 keypair for this folder, deriving from
 *  the machine seed. Same folder on same machine → same keypair, every
 *  invocation. */
export async function getKeyPairForFolder(folderPath: string): Promise<KeyPair> {
  const seed = await deriveSeed(folderPath);
  // Web Crypto's Ed25519 importKey accepts a 32-byte raw private key
  // (the seed) as PKCS#8 only — but Bun also accepts a "raw" 32-byte
  // private key for Ed25519 (deviation from spec but stable since
  // Bun 1.1). Use the standard form: construct a PKCS#8 wrapper.
  const privateKey = await crypto.subtle.importKey(
    "pkcs8",
    pkcs8WrapEd25519Seed(seed) as unknown as ArrayBuffer,
    { name: "Ed25519" },
    true,
    ["sign"],
  );
  // Derive the public key via jwk export (Web Crypto Ed25519 supports
  // exporting either half).
  const jwk = await crypto.subtle.exportKey("jwk", privateKey);
  const publicKeyBytes = fromB64u(jwk.x!);
  return {
    publicKey: b64u(publicKeyBytes),
    privateKey,
    publicKeyBytes,
    seed,
  };
}

/** Sign an arbitrary byte sequence. Returns a 64-byte signature,
 *  base64url-encoded. */
export async function sign(privateKey: CryptoKey, data: Uint8Array): Promise<string> {
  const sig = await crypto.subtle.sign("Ed25519", privateKey, data as unknown as ArrayBuffer);
  return b64u(new Uint8Array(sig));
}

/** Verify a signature against a public key. Returns true iff valid. */
export async function verify(
  publicKeyB64u: string,
  data: Uint8Array,
  signatureB64u: string,
): Promise<boolean> {
  try {
    const publicKey = await crypto.subtle.importKey(
      "raw",
      fromB64u(publicKeyB64u) as unknown as ArrayBuffer,
      { name: "Ed25519" },
      false,
      ["verify"],
    );
    return await crypto.subtle.verify(
      "Ed25519",
      publicKey,
      fromB64u(signatureB64u) as unknown as ArrayBuffer,
      data as unknown as ArrayBuffer,
    );
  } catch {
    return false;
  }
}

/** Canonical bytes-to-sign for a chat message. Wire format MUST stay
 *  stable across versions — any change requires a version-prefixed
 *  HKDF info string to avoid old/new signatures cross-contaminating. */
export function messageSigningPayload(args: {
  messageId: string;
  chatId: string;
  authorPseudonym: string;
  body: string;
  createdAt: number;
}): Uint8Array {
  const canon = [
    "claudetalk:msg:v1",
    args.messageId,
    args.chatId,
    args.authorPseudonym,
    String(args.createdAt),
    args.body,
  ].join("\n");
  return new TextEncoder().encode(canon);
}

/** Wrap a raw 32-byte Ed25519 seed in a PKCS#8 ASN.1 envelope so
 *  Web Crypto's `importKey("pkcs8", ...)` will accept it. The structure
 *  is fixed: 16-byte header + 32-byte private-key octet string.
 *  See RFC 8410 §7. */
function pkcs8WrapEd25519Seed(seed: Uint8Array): Uint8Array {
  if (seed.length !== 32) throw new Error("Ed25519 seed must be 32 bytes");
  const header = Uint8Array.from([
    0x30, 0x2e, // SEQUENCE (46 bytes)
    0x02, 0x01, 0x00, // INTEGER version 0
    0x30, 0x05, // SEQUENCE (5 bytes)
    0x06, 0x03, 0x2b, 0x65, 0x70, // OID 1.3.101.112 (Ed25519)
    0x04, 0x22, // OCTET STRING (34 bytes)
    0x04, 0x20, // OCTET STRING (32 bytes)
  ]);
  const out = new Uint8Array(header.length + seed.length);
  out.set(header, 0);
  out.set(seed, header.length);
  return out;
}
