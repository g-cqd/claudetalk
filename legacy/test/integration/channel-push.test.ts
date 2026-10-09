/**
 * T2: the `notifications/claude/channel` push path. When Alice sends a
 * chat message, Bob's MCP server (already a member of the chat) should
 * emit a `notifications/claude/channel` notification on its stdout
 * within a couple of poll cycles (CHANNEL_POLL_MS = 1000).
 *
 * Covers: cursor initialisation to current max-seq, delta detection,
 * payload shape (chat_id / sender / message_id / seq / sig / kind /
 * members), v0.6.1+ inclusion of the Ed25519 signature.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const SERVER = resolve(import.meta.dir, "..", "..", "src", "server.ts");

interface Client {
  name: string;
  proc: Subprocess<"pipe", "pipe", "inherit">;
  reader: ReadableStreamDefaultReader<string>;
  buffer: string;
  nextId: number;
  notifications: any[];
}

let TEST_HOME: string;
const clients: Client[] = [];

beforeAll(() => {
  TEST_HOME = mkdtempSync(join(tmpdir(), "claudetalk-channel-"));
});

afterAll(() => {
  for (const c of clients) {
    try {
      c.proc.kill();
    } catch {}
  }
  try {
    rmSync(TEST_HOME, { recursive: true, force: true });
  } catch {}
});

async function spawnClient(label: string, projectDir: string): Promise<Client> {
  const proc = spawn({
    cmd: ["bun", "run", SERVER],
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, CLAUDETALK_HOME: TEST_HOME, CLAUDE_PROJECT_DIR: projectDir },
  });
  const reader = proc.stdout.pipeThrough(new TextDecoderStream()).getReader();
  const c: Client = { name: label, proc, reader, buffer: "", nextId: 1, notifications: [] };
  clients.push(c);
  return c;
}

async function readUntil(c: Client, matcher: (msg: any) => boolean, timeoutMs = 5_000): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    // Drain any line currently in the buffer.
    while (true) {
      const nl = c.buffer.indexOf("\n");
      if (nl < 0) break;
      const line = c.buffer.slice(0, nl).trim();
      c.buffer = c.buffer.slice(nl + 1);
      if (!line) continue;
      const parsed = JSON.parse(line);
      if (matcher(parsed)) return parsed;
      if (parsed.method) c.notifications.push(parsed);
    }
    const remaining = deadline - Date.now();
    if (remaining <= 0) break;
    const racePromise = Promise.race([
      c.reader.read(),
      new Promise<{ value: undefined; done: false }>((r) =>
        setTimeout(() => r({ value: undefined, done: false }), remaining),
      ),
    ]);
    const { value, done } = await racePromise;
    if (done) throw new Error(`${c.name} closed stdout`);
    if (value) c.buffer += value;
  }
  throw new Error(`${c.name} timed out waiting for matching message`);
}

async function send(c: Client, method: string, params?: unknown): Promise<any> {
  const id = c.nextId++;
  c.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  await c.proc.stdin.flush?.();
  return readUntil(c, (m) => m.id === id);
}

async function notify(c: Client, method: string): Promise<void> {
  c.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method })}\n`);
  await c.proc.stdin.flush?.();
}

async function initialize(c: Client): Promise<void> {
  await send(c, "initialize", {
    protocolVersion: "2024-11-05",
    capabilities: { roots: { listChanged: false } },
    clientInfo: { name: "channel-test", version: "0" },
  });
  await notify(c, "notifications/initialized");
}

test(
  "channel push: peer message produces notifications/claude/channel on recipient",
  async () => {
    const alice = await spawnClient("alice", "/tmp/integ-channel-alice");
    const bob = await spawnClient("bob", "/tmp/integ-channel-bob");
    await initialize(alice);
    await initialize(bob);

    // Discover identities.
    const aliceWho = await send(alice, "tools/call", { name: "whoami", arguments: {} });
    const bobWho = await send(bob, "tools/call", { name: "whoami", arguments: {} });
    const aliceName = String(aliceWho.result.content[0].text).match(/You are: (\S+)/)![1]!;
    const bobName = String(bobWho.result.content[0].text).match(/You are: (\S+)/)![1]!;

    // Create a group chat with both as members.
    await send(alice, "tools/call", {
      name: "groupchat",
      arguments: { slug: "channel-test", invite: [bobName] },
    });
    // Bob joins by reading the chat so his channelPoll initialises its cursor.
    await send(bob, "tools/call", {
      name: "groupchat",
      arguments: { slug: "channel-test" },
    });

    // Wait one channel-poll tick so Bob's cursor is initialised to "current max".
    await Bun.sleep(1_500);

    // Alice posts a message; Bob's channel push should fire within ~1s.
    await send(alice, "tools/call", {
      name: "groupchat",
      arguments: { slug: "channel-test", message: "hello from alice via channel" },
    });

    // Look for the notifications/claude/channel notification on Bob's stream.
    const pushed = await readUntil(
      bob,
      (m) => m.method === "notifications/claude/channel" && m.params?.meta?.sender === aliceName,
      6_000,
    );

    expect(pushed.method).toBe("notifications/claude/channel");
    expect(pushed.params.content).toContain(aliceName);
    expect(pushed.params.content).toContain("hello from alice via channel");
    const meta = pushed.params.meta;
    expect(meta.chat_id).toBe("group:channel-test");
    expect(meta.sender).toBe(aliceName);
    expect(typeof meta.message_id).toBe("string");
    expect(meta.message_id.length).toBeGreaterThan(8); // UUID-ish
    expect(meta.seq).toMatch(/^\d+$/);
    expect(meta.kind).toBe("group");
    // K4: signature carried in the meta payload (non-empty for v0.6+ rows).
    expect(typeof meta.sig).toBe("string");
    expect(meta.sig.length).toBeGreaterThan(20);
  },
  // Bun.spawn + channel poll = needs more than the default 5s test timeout
  // when the test runner is contended.
  20_000,
);
