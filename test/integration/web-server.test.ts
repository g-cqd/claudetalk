/**
 * Spins up the dashboard server against an isolated SQLite DB and exercises
 * the HTTP routes + the SSE stream.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  addChatMember,
  ensureChat,
  groupChatId,
  insertMessage,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
import { serveDashboard, type DashboardServer } from "../../src/web/server.ts";

let home: string;
let dash: DashboardServer;

beforeAll(() => {
  home = mkdtempSync(join(tmpdir(), "claudetalk-web-"));
  process.env.CLAUDETALK_HOME = home;
  resetDb();
  upsertInstance("Alice", "/tmp/alice", 1);
  const cid = groupChatId("integ");
  ensureChat(cid, "group", "Integ");
  addChatMember(cid, "Alice");
  insertMessage(cid, "Alice", "hello world");
  // port 0 → ephemeral
  dash = serveDashboard({ port: 0, pollMs: 100 });
});

afterAll(async () => {
  await dash.stop();
  resetDb();
  delete process.env.CLAUDETALK_HOME;
  try {
    rmSync(home, { recursive: true, force: true });
  } catch {}
});

test("GET / returns the dashboard HTML", async () => {
  const r = await fetch(dash.url);
  expect(r.status).toBe(200);
  const html = await r.text();
  expect(html).toContain("ClaudeTalk");
  expect(html).toContain('<script src="/client.js">');
});

test("GET /style.css and /client.js return assets", async () => {
  for (const [p, sniff] of [
    ["/style.css", "--bg:"],
    ["/client.js", "EventSource"],
  ] as const) {
    const r = await fetch(dash.url + p.slice(1));
    expect(r.status).toBe(200);
    const body = await r.text();
    expect(body).toContain(sniff);
  }
});

test("GET /api/snapshot returns the current state", async () => {
  const r = await fetch(`${dash.url}api/snapshot`);
  expect(r.status).toBe(200);
  const j = await r.json();
  expect(j.instances.map((i: any) => i.pseudonym)).toContain("Alice");
  expect(j.chats[0].chat.title).toBe("Integ");
  expect(j.chats[0].recent_messages[0].body).toBe("hello world");
});

test("GET /api/stream is text/event-stream and emits a snapshot event quickly", async () => {
  const ctrl = new AbortController();
  const r = await fetch(`${dash.url}api/stream`, { signal: ctrl.signal });
  expect(r.status).toBe(200);
  expect(r.headers.get("content-type")).toContain("text/event-stream");
  const reader = r.body!.getReader();
  const dec = new TextDecoder();
  let buf = "";
  const deadline = Date.now() + 4000;
  let sawSnapshot = false;
  while (Date.now() < deadline && !sawSnapshot) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    if (buf.includes("event: snapshot")) sawSnapshot = true;
  }
  ctrl.abort();
  expect(sawSnapshot).toBe(true);
});

test("GET /healthz returns { ok: true }", async () => {
  const r = await fetch(`${dash.url}healthz`);
  expect(r.status).toBe(200);
  expect(await r.json()).toEqual({ ok: true });
});

test("GET /missing returns 404", async () => {
  const r = await fetch(`${dash.url}does/not/exist`);
  expect(r.status).toBe(404);
});
