/**
 * Phase N1b alpha: MCP-over-HTTP-SSE endpoint on the relay. Spawns the
 * relay binary, mints a bearer token using the same HMAC the WS path
 * uses, hits POST /mcp with initialize + tools/list + tools/call, and
 * verifies the responses.
 *
 * Limited to the alpha-tool surface (whoami / inbox / chat). Full
 * feature parity with the stdio MCP is N1b-tools follow-up work.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { mintToken } from "../../src/relay-auth.ts";
import { getKeyPairForFolder } from "../../src/keys.ts";

const RELAY = resolve(import.meta.dir, "..", "..", "relay", "src", "index.ts");

function randomPort(): number {
  return 19000 + Math.floor(Math.random() * 1000);
}

function randomBase64u(bytes = 32): string {
  const u = new Uint8Array(bytes);
  crypto.getRandomValues(u);
  return Buffer.from(u).toString("base64url");
}

let SECRET: string;
let PORT: number;
let RELAY_PROC: Subprocess<"ignore", "inherit", "inherit"> | null = null;
let RELAY_DIR: string;
let CLIENT_HOME: string;
let TOKEN: string;
let MY_PUBKEY: string;

beforeAll(async () => {
  SECRET = randomBase64u(32);
  PORT = randomPort();
  RELAY_DIR = mkdtempSync(join(tmpdir(), "claudetalk-httpmcp-relay-"));
  CLIENT_HOME = mkdtempSync(join(tmpdir(), "claudetalk-httpmcp-client-"));
  RELAY_PROC = spawn({
    cmd: ["bun", "run", RELAY],
    stdin: "ignore",
    stdout: "inherit",
    stderr: "inherit",
    env: {
      ...process.env,
      RELAY_PORT: String(PORT),
      RELAY_HOST: "127.0.0.1",
      RELAY_SHARED_SECRET: SECRET,
      RELAY_DB_PATH: join(RELAY_DIR, "relay.db"),
    },
  });
  await Bun.sleep(700);
  // Generate a keypair the way a real HTTP client would (we use
  // CLAUDETALK_HOME=client-tmp so we don't pollute ~/.claudetalk).
  process.env.CLAUDETALK_HOME = CLIENT_HOME;
  const kp = await getKeyPairForFolder("/tmp/httpmcp-test-client");
  delete process.env.CLAUDETALK_HOME;
  MY_PUBKEY = kp.publicKey;
  TOKEN = mintToken({
    pseudonym: "(http-test-client)",
    publicKeyB64u: kp.publicKey,
    sharedSecret: SECRET,
  });
});

afterAll(() => {
  try {
    RELAY_PROC?.kill();
  } catch {}
  try {
    rmSync(RELAY_DIR, { recursive: true, force: true });
  } catch {}
  try {
    rmSync(CLIENT_HOME, { recursive: true, force: true });
  } catch {}
});

let SESSION_ID = "";
let RPC_ID = 0;

async function rpcCall(method: string, params?: unknown): Promise<any> {
  RPC_ID += 1;
  const body = JSON.stringify({ jsonrpc: "2.0", id: RPC_ID, method, params });
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    authorization: `Bearer ${TOKEN}`,
  };
  if (SESSION_ID) headers["mcp-session-id"] = SESSION_ID;
  const res = await fetch(`http://127.0.0.1:${PORT}/mcp`, {
    method: "POST",
    headers,
    body,
  });
  // Capture the session id from the initialize response.
  const sid = res.headers.get("mcp-session-id");
  if (sid && !SESSION_ID) SESSION_ID = sid;
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`HTTP ${res.status}: ${text}`);
  }
  const ct = res.headers.get("content-type") ?? "";
  if (ct.includes("text/event-stream")) {
    const text = await res.text();
    const match = /^data: (.+)$/m.exec(text);
    if (!match) throw new Error(`SSE response had no data line: ${text}`);
    return JSON.parse(match[1]!);
  }
  return await res.json();
}

async function rpcNotify(method: string, params?: unknown): Promise<void> {
  const body = JSON.stringify({ jsonrpc: "2.0", method, params });
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    authorization: `Bearer ${TOKEN}`,
  };
  if (SESSION_ID) headers["mcp-session-id"] = SESSION_ID;
  await fetch(`http://127.0.0.1:${PORT}/mcp`, { method: "POST", headers, body });
}

test("HTTP MCP: missing bearer returns 401", async () => {
  const res = await fetch(`http://127.0.0.1:${PORT}/mcp`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 99, method: "initialize" }),
  });
  expect(res.status).toBe(401);
});

test(
  "HTTP MCP: full lifecycle — initialize → tools/list → tools/call(whoami|chat|inbox)",
  async () => {
    // 1. Initialize (the response carries the mcp-session-id header
    //    which rpcCall stashes in SESSION_ID for subsequent calls).
    const init = await rpcCall("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "http-test", version: "0" },
    });
    expect(init.result?.serverInfo?.name).toBe("claudetalk-relay-http");
    expect(SESSION_ID.length).toBeGreaterThan(8);
    // 2. notifications/initialized (no response expected).
    await rpcNotify("notifications/initialized");

    // 3. tools/list.
    const list = await rpcCall("tools/list");
    const tools = (list.result?.tools ?? []).map((t: any) => t.name);
    for (const expected of ["whoami", "inbox", "chat", "discover", "read", "publish"]) {
      expect(tools).toContain(expected);
    }

    // 4. tools/call whoami.
    const who = await rpcCall("tools/call", { name: "whoami", arguments: {} });
    const whoText = who.result?.content?.[0]?.text ?? "";
    expect(whoText).toContain("You are:");
    expect(whoText).toMatch(/You are: [A-Z][a-z]+[A-Z][a-z]+-[0-9a-f]{3}/);
    void MY_PUBKEY;

    // 5. tools/call chat.
    const post = await rpcCall("tools/call", {
      name: "chat",
      arguments: { slug: "httpmcp-smoke", message: "hello from HTTP MCP" },
    });
    const postText = post.result?.content?.[0]?.text ?? "";
    expect(postText).toContain("posted to group:httpmcp-smoke");
    expect(postText).toContain("frame_id=");

    // 6. tools/call inbox — should show the frame we just posted.
    //    Body is encrypted at rest → preview is "(encrypted)" not plaintext.
    const inb = await rpcCall("tools/call", { name: "inbox", arguments: { limit: 20 } });
    const inbText = inb.result?.content?.[0]?.text ?? "";
    expect(inbText).toContain("group:httpmcp-smoke");
    expect(inbText).toContain("(encrypted)");

    // 7. tools/call discover — the relay TOFU-bound our pseudonym on
    //    the first publish; we should appear in discover.
    const disc = await rpcCall("tools/call", { name: "discover", arguments: {} });
    const discText = disc.result?.content?.[0]?.text ?? "";
    expect(discText).toMatch(/Known pseudonyms \(\d+\):/);
    // Our pseudonym should be flagged "(you)".
    expect(discText).toContain("(you)");

    // 8. tools/call read — fetch frames for the chat we posted to.
    const rd = await rpcCall("tools/call", {
      name: "read",
      arguments: { chat_id: "group:httpmcp-smoke", limit: 5 },
    });
    const rdText = rd.result?.content?.[0]?.text ?? "";
    expect(rdText).toContain("group:httpmcp-smoke");
    const parsedRead = JSON.parse(rdText.slice(rdText.indexOf("[")));
    expect(Array.isArray(parsedRead)).toBe(true);
    expect(parsedRead.length).toBeGreaterThan(0);
    expect(parsedRead[0]!.body).toContain("ct1:"); // encrypted
  },
  20_000,
);

test(
  "HTTP MCP: publish accepts a client-signed frame",
  async () => {
    // Reuse SESSION_ID from the prior lifecycle test (Bun test files
    // share module state). If the prior test was skipped, re-init.
    if (!SESSION_ID) {
      await rpcCall("initialize", {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "http-test-publish", version: "0" },
      });
      await rpcNotify("notifications/initialized");
    }

    // Build a real client-signed frame: sign over plaintext (matches
    // the WS-path convention) so a recipient can decrypt+verify.
    const { getKeyPairForFolder, messageSigningPayload, sign } = await import(
      "../../src/keys.ts"
    );
    process.env.CLAUDETALK_HOME = CLIENT_HOME;
    const kp = await getKeyPairForFolder("/tmp/httpmcp-test-client");
    delete process.env.CLAUDETALK_HOME;

    const refId = crypto.randomUUID();
    const ts = Date.now();
    const target = "group:publish-test";
    const plaintext = "client-signed message";
    // Pseudonym MUST match what whoami would return (= bearer pubkey).
    const { pseudonymForKey } = await import("../../src/pseudonym.ts");
    const me = pseudonymForKey(kp.publicKey, "(http)");
    const sigBytes = await sign(
      kp.privateKey,
      messageSigningPayload({
        messageId: refId,
        chatId: target,
        authorPseudonym: me.pseudonym,
        body: plaintext,
        createdAt: ts,
      }),
    );
    // Encrypt the body client-side as the protocol expects.
    const { encryptBody } = await import("../../src/relay-crypto.ts");
    const encryptedBody = await encryptBody(SECRET, plaintext);

    const resp = await rpcCall("tools/call", {
      name: "publish",
      arguments: {
        kind: "msg",
        target,
        ref_id: refId,
        body: encryptedBody,
        ts,
        sig: sigBytes,
      },
    });
    const t = resp.result?.content?.[0]?.text ?? "";
    expect(t).toContain("published frame_id=");
    expect(t).toContain("client-signed");

    // Verify via read.
    const rd = await rpcCall("tools/call", {
      name: "read",
      arguments: { chat_id: target, limit: 5 },
    });
    const rdText = rd.result?.content?.[0]?.text ?? "";
    expect(rdText).toContain(target);
    const parsed = JSON.parse(rdText.slice(rdText.indexOf("[")));
    expect(parsed.length).toBeGreaterThan(0);
    expect(parsed[0]!.ref_id).toBe(refId);
    expect(parsed[0]!.public_key).toBe(kp.publicKey);
    expect(parsed[0]!.sig).toBe(sigBytes);
  },
  20_000,
);
