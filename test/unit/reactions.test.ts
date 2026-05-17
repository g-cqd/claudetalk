import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  ensureChat,
  groupChatId,
  insertMessage,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
import {
  clearReaction,
  listReactionsFor,
  setReaction,
  summariseReactions,
} from "../../src/reactions.ts";
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

function seedMessage(): { msgId: string; chatId: string } {
  upsertInstance("Alice", "/a", 1);
  upsertInstance("Bob", "/b", 2);
  const chatId = groupChatId("g");
  ensureChat(chatId, "group", null);
  addChatMember(chatId, "Alice");
  addChatMember(chatId, "Bob");
  const m = insertMessage(chatId, "Alice", "hi");
  return { msgId: m.id, chatId };
}

describe("reactions store", () => {
  test("setReaction inserts; re-react replaces", () => {
    const { msgId } = seedMessage();
    setReaction(msgId, "Bob", "👍");
    let rows = listReactionsFor(msgId);
    expect(rows.length).toBe(1);
    expect(rows[0]!.reaction).toBe("👍");

    setReaction(msgId, "Bob", "✓");
    rows = listReactionsFor(msgId);
    expect(rows.length).toBe(1);
    expect(rows[0]!.reaction).toBe("✓");
  });

  test("multiple reactors round-trip", () => {
    const { msgId } = seedMessage();
    upsertInstance("Carol", "/c", 3);
    setReaction(msgId, "Bob", "👍");
    setReaction(msgId, "Carol", "👍");
    expect(listReactionsFor(msgId).length).toBe(2);
  });

  test("clearReaction removes a row and reports the change", () => {
    const { msgId } = seedMessage();
    setReaction(msgId, "Bob", "👍");
    expect(clearReaction(msgId, "Bob")).toBe(true);
    expect(listReactionsFor(msgId)).toEqual([]);
    expect(clearReaction(msgId, "Bob")).toBe(false); // idempotent on second clear
  });
});

describe("summariseReactions", () => {
  test("empty when no reactions exist", () => {
    const { msgId } = seedMessage();
    expect(summariseReactions(msgId)).toBe("");
  });

  test("groups reactors per reaction with the separator", () => {
    const { msgId } = seedMessage();
    upsertInstance("Carol", "/c", 3);
    setReaction(msgId, "Bob", "👍");
    setReaction(msgId, "Carol", "👍");
    const out = summariseReactions(msgId);
    expect(out).toContain("👍 from Bob,Carol");
    expect(out.startsWith("  · ")).toBe(true);
  });

  test("multiple distinct reactions render side by side", () => {
    const { msgId } = seedMessage();
    upsertInstance("Carol", "/c", 3);
    setReaction(msgId, "Bob", "👍");
    setReaction(msgId, "Carol", "✓");
    const out = summariseReactions(msgId);
    expect(out).toContain("👍 from Bob");
    expect(out).toContain("✓ from Carol");
  });
});
