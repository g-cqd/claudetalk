/**
 * Three simulated Claude instances (different CLAUDE_PROJECT_DIRs, isolated
 * CLAUDETALK_HOME), driven through initialize → discover → ask/answer →
 * chat → groupchat → nicknames over real stdio. Exercises the full
 * MCP server, SQLite store and tool surface end-to-end.
 *
 * Runs with `bun test test/integration` or as part of `bun test`.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

const SERVER = resolve(import.meta.dir, "..", "..", "src", "server.ts");

interface Client {
  name: string;
  proc: Subprocess<"pipe", "pipe", "inherit">;
  reader: ReadableStreamDefaultReader<string>;
  buffer: string;
  nextId: number;
}

let TEST_HOME: string;
const clients: Client[] = [];

beforeAll(() => {
  TEST_HOME = mkdtempSync(join(tmpdir(), "claudetalk-integ-"));
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
    env: {
      ...process.env,
      CLAUDETALK_HOME: TEST_HOME,
      CLAUDE_PROJECT_DIR: projectDir,
    },
  });
  const reader = proc.stdout.pipeThrough(new TextDecoderStream()).getReader();
  const c: Client = { name: label, proc, reader, buffer: "", nextId: 1 };
  clients.push(c);
  return c;
}

async function send(c: Client, method: string, params?: unknown): Promise<any> {
  const id = c.nextId++;
  const msg = JSON.stringify({ jsonrpc: "2.0", id, method, params });
  c.proc.stdin.write(`${msg}\n`);
  await c.proc.stdin.flush?.();
  while (true) {
    const newlineAt = c.buffer.indexOf("\n");
    if (newlineAt < 0) {
      const { value, done } = await c.reader.read();
      if (done) throw new Error(`${c.name} closed stdout`);
      c.buffer += value;
      continue;
    }
    const line = c.buffer.slice(0, newlineAt).trim();
    c.buffer = c.buffer.slice(newlineAt + 1);
    if (!line) continue;
    const parsed = JSON.parse(line);
    if (parsed.id === id) return parsed;
    // Otherwise it's a notification; keep reading.
  }
}

async function notify(c: Client, method: string, params?: unknown): Promise<void> {
  c.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
  await c.proc.stdin.flush?.();
}

async function initialize(c: Client): Promise<void> {
  await send(c, "initialize", {
    protocolVersion: "2024-11-05",
    capabilities: { roots: { listChanged: false } },
    clientInfo: { name: "integration-test", version: "0" },
  });
  await notify(c, "notifications/initialized");
}

async function callTool(c: Client, name: string, args: unknown): Promise<any> {
  return send(c, "tools/call", { name, arguments: args });
}

function txt(r: any): string {
  const arr = r?.result?.content ?? [];
  return arr.map((b: any) => (b.type === "text" ? b.text : `<${b.type}>`)).join("\n");
}

test(
  "three simulated Claudes can discover, ask/answer, chat and groupchat over MCP",
  async () => {
    const alice = await spawnClient("alice", "/tmp/integ-alice");
    const bob = await spawnClient("bob", "/tmp/integ-bob");
    const carol = await spawnClient("carol", "/tmp/integ-carol");

    await initialize(alice);
    await initialize(bob);
    await initialize(carol);

    // tools/list
    const list = await send(alice, "tools/list");
    const tools = (list.result?.tools ?? []).map((t: any) => t.name);
    for (const expected of [
      "whoami",
      "discover",
      "ask",
      "answer",
      "chat",
      "groupchat",
      "read",
      "inbox",
    ]) {
      expect(tools).toContain(expected);
    }
    // wait_for_messages was intentionally removed (it blocked the JSON-RPC
    // channel and made Claude appear stuck); hooks handle live polling now.
    expect(tools).not.toContain("wait_for_messages");

    // identities
    const aliceWho = txt(await callTool(alice, "whoami", {}));
    const bobWho = txt(await callTool(bob, "whoami", {}));
    const carolWho = txt(await callTool(carol, "whoami", {}));
    const aliceName = aliceWho.match(/You are: (\S+)/)![1]!;
    const bobName = bobWho.match(/You are: (\S+)/)![1]!;
    const carolName = carolWho.match(/You are: (\S+)/)![1]!;
    expect(new Set([aliceName, bobName, carolName]).size).toBe(3);

    // discover sees everyone
    const discover = txt(await callTool(alice, "discover", {}));
    expect(discover).toContain(bobName);
    expect(discover).toContain(carolName);

    // ask / inbox / answer round-trip
    const askRes = txt(await callTool(alice, "ask", { to: bobName, question: "ping?" }));
    const askId = Number(askRes.match(/ask_id=(\d+)/)![1]);
    const bobInbox = txt(await callTool(bob, "inbox", {}));
    expect(bobInbox).toContain(`ask_id=${askId}`);
    expect(bobInbox).toContain("ping?");
    await callTool(bob, "answer", { ask_id: askId, answer: "pong!" });
    const aliceInbox = txt(await callTool(alice, "inbox", {}));
    expect(aliceInbox).toContain("pong!");

    // direct chat persists both ways
    const chat1 = txt(await callTool(alice, "chat", { with: bobName, message: "hi bob" }));
    expect(chat1).toMatch(/chat_id=direct:/);
    const chat2 = txt(await callTool(bob, "chat", { with: aliceName, message: "hi alice" }));
    expect(chat2).toContain("hi bob");
    const chat3 = txt(await callTool(alice, "chat", { with: bobName }));
    expect(chat3).toContain("hi alice");

    // group chat with three members
    await callTool(alice, "groupchat", {
      slug: "design",
      title: "Design Review",
      message: "starting",
    });
    await callTool(bob, "groupchat", { slug: "design", message: "joining" });
    const carolGroup = txt(
      await callTool(carol, "groupchat", { slug: "design", message: "me too" }),
    );
    expect(carolGroup).toContain("starting");
    expect(carolGroup).toContain("joining");

    // ---------- personal nickname ----------
    // Alice unilaterally nicknames Bob. From now on, her discover/inbox/etc.
    // show 'bobby' alongside or in place of the pseudonym; nothing changes
    // on Bob's side.
    const setRes = txt(
      await callTool(alice, "nickname_set", { target: bobName, nickname: "bobby" }),
    );
    expect(setRes).toMatch(/personal nickname/i);

    // Alice's discover should now contain 'bobby' next to Bob's pseudonym.
    const aliceDiscover = txt(await callTool(alice, "discover", {}));
    expect(aliceDiscover).toContain("bobby");
    expect(aliceDiscover).toContain(bobName);

    // Bob's discover is unaffected.
    const bobDiscover = txt(await callTool(bob, "discover", {}));
    expect(bobDiscover).not.toContain("bobby");

    // ---------- group nickname (votes by 2 including target) ----------
    // Alice proposes 'carrot' for Carol in #design.
    const v1 = txt(
      await callTool(alice, "nickname_in_chat", {
        chat_id: "group:design",
        target: carolName,
        nickname: "carrot",
      }),
    );
    expect(v1).toMatch(/[Pp]ending/);

    // Bob also votes 'carrot' — still pending because target Carol hasn't.
    const v2 = txt(
      await callTool(bob, "nickname_in_chat", {
        chat_id: "group:design",
        target: carolName,
        nickname: "carrot",
      }),
    );
    expect(v2).toMatch(/[Pp]ending/);

    // Carol ratifies — now ACTIVE.
    const v3 = txt(
      await callTool(carol, "nickname_in_chat", {
        chat_id: "group:design",
        target: carolName,
        nickname: "carrot",
      }),
    );
    expect(v3).toMatch(/ACTIVE/);

    // In #design, future chat messages render Carol as 'carrot' from Alice's
    // perspective (and everyone else's too).
    await callTool(carol, "groupchat", { slug: "design", message: "I am the carrot" });
    const aliceSeesChat = txt(
      await callTool(alice, "groupchat", { slug: "design" }),
    );
    expect(aliceSeesChat).toContain("carrot");
    expect(aliceSeesChat).toContain("I am the carrot");

    // ---------- folder search ----------
    const folderSearch = txt(
      await callTool(alice, "discover", { folder_contains: "integ-bob" }),
    );
    expect(folderSearch).toContain(bobName);
    expect(folderSearch).not.toContain(carolName);

    // ---------- nicknames_list ----------
    const nickList = txt(await callTool(alice, "nicknames_list", {}));
    expect(nickList).toContain("Personal");
    expect(nickList).toContain("bobby");
    expect(nickList).toContain("Group");
    expect(nickList).toContain("carrot");

  },
  60_000, // generous overall timeout
);
