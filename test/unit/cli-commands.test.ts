/**
 * Coverage for `src/cli-commands.ts`. Originally empty — both `exportChat`
 * and `buildMetrics` quietly broke under the v0.5.0 UUID-PK migration
 * because no test exercised them. These tests pin both against the
 * post-v3 schema.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  addChatMember,
  ensureChat,
  groupChatId,
  insertMessage,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
import { buildMetrics, exportChat } from "../../src/cli-commands.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
  resetDb();
});

afterEach(() => {
  resetDb();
  home.cleanup();
});

test("exportChat (md) shows seq labels, not UUIDs, and orders chronologically", () => {
  upsertInstance("Alice", "/a", 1);
  upsertInstance("Bob", "/b", 2);
  const chatId = groupChatId("export-test");
  ensureChat(chatId, "group", "Export Test");
  addChatMember(chatId, "Alice");
  addChatMember(chatId, "Bob");
  const m1 = insertMessage(chatId, "Alice", "first");
  const m2 = insertMessage(chatId, "Bob", "second");
  const m3 = insertMessage(chatId, "Alice", "reply to first", m1.id);

  const r = exportChat(chatId, "md");
  expect(r.ok).toBe(true);
  // Heading uses seq (numeric), not UUID.
  expect(r.output).toContain(`## #${m1.seq} — Alice`);
  expect(r.output).toContain(`## #${m2.seq} — Bob`);
  // Reply marker resolves parent UUID → parent seq.
  expect(r.output).toContain(`_(replying to #${m1.seq})_`);
  // Order is chronological (seq ASC) — m1 appears before m3 appears before m2 if and only if
  // seqs follow insertion order, which they do.
  const i1 = r.output.indexOf(`## #${m1.seq}`);
  const i2 = r.output.indexOf(`## #${m2.seq}`);
  const i3 = r.output.indexOf(`## #${m3.seq}`);
  expect(i1).toBeLessThan(i2);
  expect(i2).toBeLessThan(i3);
  // No UUID leakage in the headings (UUIDs contain hyphens, length 36).
  expect(r.output).not.toContain(m1.id);
  expect(r.output).not.toContain(m2.id);
});

test("exportChat (json) carries both UUID id and numeric seq", () => {
  upsertInstance("Alice", "/a", 1);
  const chatId = groupChatId("export-json");
  ensureChat(chatId, "group", null);
  addChatMember(chatId, "Alice");
  insertMessage(chatId, "Alice", "hello");
  const r = exportChat(chatId, "json");
  expect(r.ok).toBe(true);
  const parsed = JSON.parse(r.output) as {
    messages: Array<{ id: string; seq: number; body: string }>;
  };
  expect(parsed.messages).toHaveLength(1);
  expect(typeof parsed.messages[0]!.id).toBe("string");
  expect(typeof parsed.messages[0]!.seq).toBe("number");
  expect(parsed.messages[0]!.body).toBe("hello");
});

test("exportChat returns ok=false on unknown chat", () => {
  const r = exportChat("group:nope", "md");
  expect(r.ok).toBe(false);
  expect(r.output).toContain("unknown chat_id");
});

test("buildMetrics runs against a v3 schema without column errors", () => {
  upsertInstance("Alice", "/a", 1);
  // Drive at least one row through chat_members so the hook-dedup query
  // touches the renamed column. (Pre-fix this threw `no such column:
  // last_notified_message_id`.)
  const chatId = groupChatId("metrics-test");
  ensureChat(chatId, "group", null);
  addChatMember(chatId, "Alice");
  insertMessage(chatId, "Alice", "m");
  const m = buildMetrics({ windowHours: 24 });
  expect(m.window_hours).toBe(24);
  expect(Array.isArray(m.per_tool)).toBe(true);
  expect(Array.isArray(m.per_pseudonym)).toBe(true);
  // The dedup heuristic touches chat_members.last_notified_message_seq;
  // success means the SELECT succeeded — exact values aren't important.
  expect(typeof m.hook_dedup_estimate.chat_messages_total).toBe("number");
});
