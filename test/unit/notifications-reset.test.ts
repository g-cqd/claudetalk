import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  ensureChat,
  groupChatId,
  insertAsk,
  insertMessage,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
import {
  advanceNotificationCursors,
  notificationDeltaFor,
  resetNotificationCursors,
} from "../../src/notifications.ts";
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

function seedChatWithUnread(): string {
  upsertInstance("ME", "/me", 1);
  upsertInstance("PEER", "/peer", 2);
  const cid = groupChatId("g");
  ensureChat(cid, "group", null);
  addChatMember(cid, "ME");
  addChatMember(cid, "PEER");
  insertMessage(cid, "PEER", "first");
  insertMessage(cid, "PEER", "second");
  // Advance so the chat cursor is non-zero.
  advanceNotificationCursors("ME", notificationDeltaFor("ME"));
  return cid;
}

describe("resetNotificationCursors", () => {
  test("with chat_id: only that chat is reset, other cursors untouched", () => {
    const cidA = seedChatWithUnread();
    // Add a second chat, also with non-zero cursor
    upsertInstance("PEER2", "/p2", 3);
    const cidB = groupChatId("h");
    ensureChat(cidB, "group", null);
    addChatMember(cidB, "ME");
    addChatMember(cidB, "PEER2");
    insertMessage(cidB, "PEER2", "x");
    advanceNotificationCursors("ME", notificationDeltaFor("ME"));

    // First delta should be empty (cursors advanced).
    expect(notificationDeltaFor("ME").newChats).toEqual([]);

    const n = resetNotificationCursors("ME", cidA);
    expect(n).toBe(1);

    const after = notificationDeltaFor("ME");
    // Only chat A should re-surface; chat B's cursor is still advanced.
    const ids = after.newChats.map((c) => c.chat.id);
    expect(ids).toEqual([cidA]);
  });

  test("without chat_id: every chat + ask cursor resets", () => {
    seedChatWithUnread();
    insertAsk("PEER", "ME", "q");
    advanceNotificationCursors("ME", notificationDeltaFor("ME"));
    expect(notificationDeltaFor("ME").newChats).toEqual([]);
    expect(notificationDeltaFor("ME").newAsks).toEqual([]);

    const n = resetNotificationCursors("ME", null);
    expect(n).toBeGreaterThan(0);

    const after = notificationDeltaFor("ME");
    expect(after.newChats.length).toBeGreaterThan(0);
    expect(after.newAsks.length).toBeGreaterThan(0);
  });

  test("returns 0 when nothing was past 0", () => {
    upsertInstance("ME", "/me", 1);
    // No chats joined, no asks, nothing to reset.
    expect(resetNotificationCursors("ME", null)).toBe(0);
  });
});
