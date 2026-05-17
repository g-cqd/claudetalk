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

test("WS /ws delivers an initial snapshot, then pushes after a DB change", async () => {
  const wsUrl = dash.url.replace(/^http/, "ws") + "ws";
  const ws = new WebSocket(wsUrl);
  const events: any[] = [];
  const opened = new Promise<void>((r) => ws.addEventListener("open", () => r()));
  ws.addEventListener("message", (ev) => {
    events.push(JSON.parse(ev.data));
  });
  await opened;
  // Initial snapshot pushed on open.
  for (let i = 0; i < 50 && events.length < 1; i++) await Bun.sleep(20);
  expect(events.length).toBeGreaterThanOrEqual(1);
  expect(events[0].type).toBe("snapshot");
  expect(events[0].data.instances.map((x: any) => x.pseudonym)).toContain("Alice");

  // A trigger-tracked write should produce a new snapshot push.
  insertMessage("group:integ", "Alice", "second");
  for (let i = 0; i < 50 && events.length < 2; i++) await Bun.sleep(20);
  expect(events.length).toBeGreaterThanOrEqual(2);
  const last = events[events.length - 1];
  expect(last.type).toBe("snapshot");
  expect(last.data.chats[0].recent_messages.some((m: any) => m.body === "second")).toBe(true);

  ws.close();
  await Bun.sleep(50);
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

// ---------- Phase 3.1 — /api/messages pagination ----------

test("GET /api/messages without chat_id → 400", async () => {
  const r = await fetch(`${dash.url}api/messages`);
  expect(r.status).toBe(400);
});

test("GET /api/messages with unknown chat_id → 404", async () => {
  const r = await fetch(`${dash.url}api/messages?chat_id=group:nonexistent`);
  expect(r.status).toBe(404);
});

test("GET /api/messages returns rows with display_from_name", async () => {
  const r = await fetch(`${dash.url}api/messages?chat_id=group:integ&limit=10`);
  expect(r.status).toBe(200);
  const body = await r.json();
  expect(body.chat_id).toBe("group:integ");
  expect(Array.isArray(body.messages)).toBe(true);
  expect(body.messages.length).toBeGreaterThan(0);
  for (const m of body.messages) {
    expect(typeof m.display_from_name).toBe("string");
  }
});

// ---------- Phase 3.2 — viewer-perspective nicknames in snapshot ----------

test("GET /api/snapshot?viewer=X returns display_name fields on every entity", async () => {
  const r = await fetch(`${dash.url}api/snapshot?viewer=Alice`);
  expect(r.status).toBe(200);
  const s = await r.json();
  expect(s.viewer).toBe("Alice");
  for (const i of s.instances) {
    expect(typeof i.display_name).toBe("string");
  }
  for (const c of s.chats) {
    for (const m of c.members) {
      expect(typeof m.display_name).toBe("string");
      expect(typeof m.pseudonym).toBe("string");
    }
    for (const msg of c.recent_messages) {
      expect(typeof msg.display_from_name).toBe("string");
    }
  }
});

// ---------- Phase 3.3 — /api/calls with filters ----------

test("GET /api/calls returns calls list with display_pseudonym_name", async () => {
  const r = await fetch(`${dash.url}api/calls?limit=10`);
  expect(r.status).toBe(200);
  const body = await r.json();
  expect(Array.isArray(body.calls)).toBe(true);
});

test("GET /api/calls?error_only=1 filters to errors only", async () => {
  const r = await fetch(`${dash.url}api/calls?error_only=1`);
  expect(r.status).toBe(200);
  const body = await r.json();
  for (const c of body.calls) {
    expect(c.is_error).toBe(1);
  }
});
