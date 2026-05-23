/**
 * Cross-machine end-to-end smoke test for Phase N1+N2.
 *
 * Spawns:
 *   * one relay binary (relay/src/index.ts) on an ephemeral port,
 *     with an isolated SQLite path + a random shared secret
 *   * two MCP servers (src/server.ts), each in its own CLAUDETALK_HOME
 *     (so they have independent local DBs), each with a
 *     network.json pointing at the relay + the same shared secret
 *
 * Then has Alice send a chat message; verifies it shows up in Bob's
 * local DB (via the inbox tool) within a few seconds. End-to-end:
 * sign → encrypt → publish → relay broadcast → ingest → decrypt →
 * verify sig → insert local.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const SERVER = resolve(import.meta.dir, "..", "..", "src", "server.ts");
const RELAY = resolve(import.meta.dir, "..", "..", "relay", "src", "index.ts");

interface Client {
  name: string;
  proc: Subprocess<"pipe", "pipe", "inherit">;
  reader: ReadableStreamDefaultReader<string>;
  buffer: string;
  nextId: number;
}

let SHARED_SECRET: string;
let RELAY_PORT: number;
let RELAY_HOME_DIR: string;
let RELAY_PROC: Subprocess<"ignore", "inherit", "inherit"> | null = null;
const CLIENT_HOMES: string[] = [];
const clients: Client[] = [];

function randomPort(): number {
  // Avoid the 4242 dashboard default + other common ports.
  return 18000 + Math.floor(Math.random() * 2000);
}

function randomBase64u(bytes = 32): string {
  const u = new Uint8Array(bytes);
  crypto.getRandomValues(u);
  return Buffer.from(u).toString("base64url");
}

beforeAll(async () => {
  SHARED_SECRET = randomBase64u(32);
  RELAY_PORT = randomPort();
  RELAY_HOME_DIR = mkdtempSync(join(tmpdir(), "claudetalk-relay-int-"));
  RELAY_PROC = spawn({
    cmd: ["bun", "run", RELAY],
    stdin: "ignore",
    stdout: "inherit",
    stderr: "inherit",
    env: {
      ...process.env,
      RELAY_PORT: String(RELAY_PORT),
      RELAY_HOST: "127.0.0.1",
      RELAY_SHARED_SECRET: SHARED_SECRET,
      RELAY_DB_PATH: join(RELAY_HOME_DIR, "relay.db"),
    },
  });
  // Give the relay ~600 ms to bind + open its SQLite DB.
  await Bun.sleep(600);
});

afterAll(async () => {
  for (const c of clients) {
    try {
      c.proc.kill();
    } catch {}
  }
  try {
    RELAY_PROC?.kill();
  } catch {}
  for (const h of CLIENT_HOMES) {
    try {
      rmSync(h, { recursive: true, force: true });
    } catch {}
  }
  try {
    rmSync(RELAY_HOME_DIR, { recursive: true, force: true });
  } catch {}
});

async function spawnClient(label: string, projectDir: string): Promise<Client> {
  const home = mkdtempSync(join(tmpdir(), `claudetalk-${label}-`));
  CLIENT_HOMES.push(home);
  writeFileSync(
    join(home, "network.json"),
    JSON.stringify({
      relay_url: `ws://127.0.0.1:${RELAY_PORT}`,
      shared_secret: SHARED_SECRET,
    }),
    { mode: 0o600 },
  );
  const proc = spawn({
    cmd: ["bun", "run", SERVER],
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, CLAUDETALK_HOME: home, CLAUDE_PROJECT_DIR: projectDir },
  });
  const reader = proc.stdout.pipeThrough(new TextDecoderStream()).getReader();
  const c: Client = { name: label, proc, reader, buffer: "", nextId: 1 };
  clients.push(c);
  return c;
}

async function send(c: Client, method: string, params?: unknown): Promise<any> {
  const id = c.nextId++;
  c.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  await c.proc.stdin.flush?.();
  const deadline = Date.now() + 8_000;
  while (Date.now() < deadline) {
    const nl = c.buffer.indexOf("\n");
    if (nl < 0) {
      const { value, done } = await c.reader.read();
      if (done) throw new Error(`${c.name} closed stdout`);
      if (value) c.buffer += value;
      continue;
    }
    const line = c.buffer.slice(0, nl).trim();
    c.buffer = c.buffer.slice(nl + 1);
    if (!line) continue;
    const parsed = JSON.parse(line);
    if (parsed.id === id) return parsed;
  }
  throw new Error(`${c.name} timed out on ${method}`);
}

async function notify(c: Client, method: string): Promise<void> {
  c.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method })}\n`);
  await c.proc.stdin.flush?.();
}

async function initialize(c: Client): Promise<void> {
  await send(c, "initialize", {
    protocolVersion: "2024-11-05",
    capabilities: { roots: { listChanged: false } },
    clientInfo: { name: "relay-e2e-test", version: "0" },
  });
  await notify(c, "notifications/initialized");
}

function txt(r: any): string {
  return (r?.result?.content ?? [])
    .map((b: any) => (b.type === "text" ? b.text : ""))
    .join("\n");
}

test(
  "alice → relay → bob: cross-home message is signed, encrypted, broadcast, and ingested",
  async () => {
    const alice = await spawnClient("alice", "/tmp/relay-e2e-alice");
    const bob = await spawnClient("bob", "/tmp/relay-e2e-bob");
    await initialize(alice);
    await initialize(bob);

    const aliceWho = txt(await send(alice, "tools/call", { name: "whoami", arguments: {} }));
    const bobWho = txt(await send(bob, "tools/call", { name: "whoami", arguments: {} }));
    const aliceName = aliceWho.match(/You are: (\S+)/)![1]!;
    const bobName = bobWho.match(/You are: (\S+)/)![1]!;
    expect(aliceName).not.toBe(bobName);

    // Both clients open the same group chat. Bob's first call adds him
    // as a member; the relay TOFU-binds his pubkey on his first
    // outbound frame.
    await send(alice, "tools/call", {
      name: "groupchat",
      arguments: { slug: "e2e", invite: [bobName] },
    });
    await send(bob, "tools/call", { name: "groupchat", arguments: { slug: "e2e" } });

    // Give the relay clients ~500ms to open their WebSockets.
    await Bun.sleep(500);

    // Alice posts. The local insert publishes to the relay; the relay
    // broadcasts to Bob; Bob's RelayClient ingests into his local DB.
    await send(alice, "tools/call", {
      name: "groupchat",
      arguments: { slug: "e2e", message: "hello from alice over the relay" },
    });

    // Poll Bob's inbox until the message arrives (or timeout).
    let bobSawIt = false;
    const deadline = Date.now() + 8_000;
    while (Date.now() < deadline && !bobSawIt) {
      const inbox = txt(await send(bob, "tools/call", { name: "inbox", arguments: {} }));
      const readResp = txt(
        await send(bob, "tools/call", {
          name: "read",
          arguments: { chat_id: "group:e2e", limit: 50 },
        }),
      );
      if (readResp.includes("hello from alice over the relay")) {
        bobSawIt = true;
        expect(readResp).toContain(aliceName);
        break;
      }
      void inbox;
      await Bun.sleep(400);
    }

    expect(bobSawIt).toBe(true);
  },
  30_000,
);
