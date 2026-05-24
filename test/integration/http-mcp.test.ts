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

    // 3. tools/list — N1b-tools-5 exposes the full stdio surface
    //    (registerTools registers ~18 tools) + the relay-specific
    //    `publish`. Confirm representative names from each register*.
    const list = await rpcCall("tools/list");
    const tools = (list.result?.tools ?? []).map((t: any) => t.name);
    for (const expected of [
      // src/tools.ts
      "whoami",
      "discover",
      "ask",
      "answer",
      "inbox",
      "notifications_reset",
      "wait_for_messages",
      // src/chat-tools.ts
      "chat",
      "groupchat",
      "read",
      // src/reactions.ts
      "react",
      // src/status.ts
      "status_set",
      "status_clear",
      // src/search.ts
      "search",
      // src/mute.ts
      "mute",
      // src/nickname.ts
      "nickname_set",
      "nickname_clear",
      "nickname_in_chat",
      "nicknames_list",
      // relay-specific
      "publish",
    ]) {
      expect(tools).toContain(expected);
    }

    // 4. tools/call whoami — stdio MCP format: "You are: <pseudonym>"
    //    followed by "Folder: <path>". Path is "(http)" for HTTP
    //    clients.
    const who = await rpcCall("tools/call", { name: "whoami", arguments: {} });
    const whoText = who.result?.content?.[0]?.text ?? "";
    expect(whoText).toMatch(/You are: [A-Z][a-z]+[A-Z][a-z]+-[0-9a-f]{3}/);
    expect(whoText).toContain("(http)"); // path for HTTP clients
    void MY_PUBKEY;

    // 5. tools/call groupchat — the correct way to post to a slug-
    //    addressed room. (`chat` is for direct 1:1 by pseudonym).
    const post = await rpcCall("tools/call", {
      name: "groupchat",
      arguments: { slug: "httpmcp-smoke", message: "hello from HTTP MCP" },
    });
    const postText = post.result?.content?.[0]?.text ?? "";
    expect(postText).toContain("chat_id=group:httpmcp-smoke");
    expect(postText).toContain("Sent your message");

    // 6. tools/call inbox — stdio MCP format: "Inbox for <pseudonym>:"
    //    + chat lines. We just posted into httpmcp-smoke; it should
    //    appear in our chats listing.
    const inb = await rpcCall("tools/call", { name: "inbox", arguments: {} });
    const inbText = inb.result?.content?.[0]?.text ?? "";
    expect(inbText).toContain("Inbox for ");
    expect(inbText).toContain("group:httpmcp-smoke");

    // 7. tools/call discover — stdio MCP format: "Active ClaudeTalk
    //    instances (N):" + per-instance lines. Our pseudonym should
    //    appear (and the trailing "(You are ...)" footer too).
    const disc = await rpcCall("tools/call", { name: "discover", arguments: {} });
    const discText = disc.result?.content?.[0]?.text ?? "";
    expect(discText).toMatch(/Active ClaudeTalk instances \(\d+\):/);
    expect(discText).toContain("(You are ");

    // 8. tools/call read — stdio MCP format: "chat_id=X (N messages
    //    since 0)" + [seq] lines. With the N1b-tools-5 architecture
    //    chat-tools' insertMessage writes the PLAINTEXT body into the
    //    relay's `messages` table (the loopback Publisher only
    //    encrypts for the WS broadcast envelope + frames log). HTTP-
    //    sent content via chat/groupchat is therefore relay-readable;
    //    callers who want N2 confidentiality at the relay must use
    //    `publish` instead with a pre-encrypted body.
    const rd = await rpcCall("tools/call", {
      name: "read",
      arguments: { chat_id: "group:httpmcp-smoke", limit: 5 },
    });
    const rdText = rd.result?.content?.[0]?.text ?? "";
    expect(rdText).toContain("chat_id=group:httpmcp-smoke");
    expect(rdText).toContain("hello from HTTP MCP");
  },
  20_000,
);

test("Relay materialises inbound frames into the ClaudeTalk schema (messages/chats)", async () => {
  // Open the relay's SQLite directly (read-only via a separate
  // connection) and confirm messages/chats rows exist after the
  // earlier publish.
  const { Database } = await import("bun:sqlite");
  const { join } = await import("node:path");
  const relayDb = new Database(join(RELAY_DIR, "relay.db"));
  try {
    const chats = relayDb
      .query<{ id: string; kind: string }, []>(
        "SELECT id, kind FROM chats WHERE id = 'group:httpmcp-smoke'",
      )
      .all();
    expect(chats.length).toBe(1);
    expect(chats[0]!.kind).toBe("group");

    const msgs = relayDb
      .query<{ id: string; seq: number; body: string; signature: string | null }, []>(
        "SELECT id, seq, body, signature FROM messages WHERE chat_id = 'group:httpmcp-smoke'",
      )
      .all();
    expect(msgs.length).toBeGreaterThan(0);
    // N1b-tools-5 trade-off: chat-tools writes plaintext into messages
    // (via insertMessage) BEFORE the loopback Publisher encrypts for
    // the WS broadcast frame. The `frames` table still holds the
    // ct1:-encrypted body. HTTP clients who need N2-strength
    // confidentiality at the relay must use `publish` with a pre-
    // encrypted body instead of `chat`/`groupchat`. See CHANGELOG
    // v0.10.5 for the rationale.
    expect(msgs[0]!.body).toBe("hello from HTTP MCP");
    expect(msgs[0]!.signature).not.toBeNull();
    // Frames table still has the encrypted form:
    const frames = relayDb
      .query<{ body: string }, []>(
        "SELECT body FROM frames WHERE target = 'group:httpmcp-smoke'",
      )
      .all();
    expect(frames.length).toBeGreaterThan(0);
    expect(frames[0]!.body).toMatch(/^ct1:/);

    const members = relayDb
      .query<{ pseudonym: string }, []>(
        "SELECT pseudonym FROM chat_members WHERE chat_id = 'group:httpmcp-smoke'",
      )
      .all();
    expect(members.length).toBeGreaterThan(0);
  } finally {
    relayDb.close();
  }
});

test("HTTP MCP: status_set + status_clear round-trip via discover", async () => {
  // discover doesn't yet surface status, so we check the relay DB directly.
  const { Database } = await import("bun:sqlite");
  const { join } = await import("node:path");
  if (!SESSION_ID) {
    await rpcCall("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "http-test-status", version: "0" },
    });
    await rpcNotify("notifications/initialized");
  }
  await rpcCall("tools/call", {
    name: "status_set",
    arguments: { status: "available for review", emoji: "👀" },
  });
  const relayDb = new Database(join(RELAY_DIR, "relay.db"));
  try {
    const row = relayDb
      .query<{ status: string; emoji: string | null }, []>(
        "SELECT status, emoji FROM instance_status WHERE pseudonym != ''",
      )
      .get();
    expect(row?.status).toBe("available for review");
    expect(row?.emoji).toBe("👀");
  } finally {
    relayDb.close();
  }
  await rpcCall("tools/call", { name: "status_clear", arguments: {} });
  const relayDb2 = new Database(join(RELAY_DIR, "relay.db"));
  try {
    const row = relayDb2
      .query<{ n: number }, []>("SELECT COUNT(*) AS n FROM instance_status")
      .get();
    expect(row?.n ?? 0).toBe(0);
  } finally {
    relayDb2.close();
  }
});

test("HTTP MCP: search hits a posted message via the materialised schema", async () => {
  if (!SESSION_ID) {
    await rpcCall("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "http-test-search", version: "0" },
    });
    await rpcNotify("notifications/initialized");
  }
  // Seed regardless of session state — the lifecycle test posted to
  // a different slug so we need to ensure something with our needle
  // exists in messages.body.
  await rpcCall("tools/call", {
    name: "groupchat",
    arguments: { slug: "search-seed", message: "needle to find" },
  });
  // chat-tools writes plaintext to messages (see N1b-tools-5 trade-off
  // in CHANGELOG). Search hits the plaintext content directly.
  const r = await rpcCall("tools/call", {
    name: "search",
    arguments: { query: "needle", limit: 10 },
  });
  const text = r.result?.content?.[0]?.text ?? "";
  expect(text).toContain("Search 'needle'");
  expect(text).toContain("Chat hits");
  // The chat we seeded was "needle to find" in chat_id=group:search-seed.
  expect(text).toMatch(/\[#\d+\] group:search-seed/);
});

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

    // Verify via read — stdio MCP format: "chat_id=X (N messages
    // since 0)\n[seq] sender (Xs ago): <body>". Body is encrypted at
    // rest. The frame was materialised into the messages table via
    // publishFrameAndBroadcast → insertFrame → materialiseMessageFrame.
    const rd = await rpcCall("tools/call", {
      name: "read",
      arguments: { chat_id: target, limit: 5 },
    });
    const rdText = rd.result?.content?.[0]?.text ?? "";
    expect(rdText).toContain(`chat_id=${target}`);
    expect(rdText).toContain(me.pseudonym);
    expect(rdText).toMatch(/\[\d+\]/); // [seq] prefix
    expect(rdText).toContain("ct1:"); // body is encrypted at rest

    // Also confirm via direct DB query that signature + public_key
    // round-tripped without server-side rewrites (publish bypasses the
    // server-side keypair derivation that `chat` does).
    const { Database } = await import("bun:sqlite");
    const { join } = await import("node:path");
    const relayDb = new Database(join(RELAY_DIR, "relay.db"));
    try {
      const row = relayDb
        .query<{ sig: string; public_key: string }, [string]>(
          "SELECT sig, public_key FROM frames WHERE ref_id = ?",
        )
        .get(refId);
      expect(row?.sig).toBe(sigBytes);
      expect(row?.public_key).toBe(kp.publicKey);
    } finally {
      relayDb.close();
    }
  },
  20_000,
);
