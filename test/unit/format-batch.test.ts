/**
 * v0.5.2: `fmtMessageList` must produce the same per-message rendering as
 * `fmtMessage` (called N times) but with batched SQL — 2 queries total
 * instead of 2N. This test pins the equivalence so refactors don't drift.
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
import { fmtMessage, fmtMessageList } from "../../src/format.ts";
import { setReaction } from "../../src/reactions.ts";
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

test("fmtMessageList result matches per-call fmtMessage on the same slice", () => {
  upsertInstance("Alice", "/a", 1);
  upsertInstance("Bob", "/b", 2);
  const chatId = groupChatId("eq-test");
  ensureChat(chatId, "group", null);
  addChatMember(chatId, "Alice");
  addChatMember(chatId, "Bob");
  const m1 = insertMessage(chatId, "Alice", "hello");
  const m2 = insertMessage(chatId, "Bob", "reply", m1.id);
  setReaction(m1.id, "Bob", "👍");
  setReaction(m2.id, "Alice", "✓");
  const messages = [m1, m2];

  const batched = fmtMessageList(messages, "Alice");
  const sequential = messages.map((m) => fmtMessage(m, "Alice"));

  expect(batched).toEqual(sequential);
});

test("fmtMessageList handles empty input", () => {
  expect(fmtMessageList([], "Alice")).toEqual([]);
});

test("fmtMessageList shows parent_seq for replies, even when many messages share a parent", () => {
  upsertInstance("Alice", "/a", 1);
  const chatId = groupChatId("threading");
  ensureChat(chatId, "group", null);
  addChatMember(chatId, "Alice");
  const parent = insertMessage(chatId, "Alice", "parent");
  const r1 = insertMessage(chatId, "Alice", "reply 1", parent.id);
  const r2 = insertMessage(chatId, "Alice", "reply 2", parent.id);
  const lines = fmtMessageList([parent, r1, r2]);
  expect(lines[0]).toContain(`[${parent.seq}]`);
  expect(lines[1]).toContain(`[${r1.seq} ↪ ${parent.seq}]`);
  expect(lines[2]).toContain(`[${r2.seq} ↪ ${parent.seq}]`);
});
