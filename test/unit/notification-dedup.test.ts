/**
 * Hook-dedup regression tests. Locks in the contract that a Claude session
 * is notified ONCE per new message; subsequent hook fires with no new
 * content produce an empty delta.
 *
 * The bug this guards against (2026-05-17, reported by OnyxKraken-7ba): the
 * hook re-injected the same message body on every PostToolUse / PostToolBatch
 * because there was no per-(viewer, chat) notification cursor.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  advanceNotificationCursors,
  discoverableGroupsFor,
  ensureChat,
  groupChatId,
  insertAsk,
  insertMessage,
  notificationDeltaFor,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
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

describe("notificationDeltaFor", () => {
  test("first call returns every message; cursor advance then yields empty", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("PEER", "/peer", 2);
    const cid = groupChatId("g");
    ensureChat(cid, "group", null);
    addChatMember(cid, "ME");
    addChatMember(cid, "PEER");
    insertMessage(cid, "PEER", "hi");
    insertMessage(cid, "PEER", "there");

    const first = notificationDeltaFor("ME");
    expect(first.newChats.length).toBe(1);
    expect(first.newChats[0]!.new_count).toBe(2);

    advanceNotificationCursors("ME", first);

    const second = notificationDeltaFor("ME");
    expect(second.newChats).toEqual([]);
    expect(second.newAsks).toEqual([]);
  });

  test("a fresh peer message after cursor advance is surfaced again", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("PEER", "/peer", 2);
    const cid = groupChatId("g2");
    ensureChat(cid, "group", null);
    addChatMember(cid, "ME");
    addChatMember(cid, "PEER");
    insertMessage(cid, "PEER", "first");

    advanceNotificationCursors("ME", notificationDeltaFor("ME"));
    expect(notificationDeltaFor("ME").newChats).toEqual([]);

    insertMessage(cid, "PEER", "fresh");
    const delta = notificationDeltaFor("ME");
    expect(delta.newChats.length).toBe(1);
    expect(delta.newChats[0]!.new_count).toBe(1);
    expect(delta.newChats[0]!.latest.body).toBe("fresh");
  });

  test("own messages do not trigger notifications", () => {
    upsertInstance("ME", "/me", 1);
    const cid = groupChatId("g3");
    ensureChat(cid, "group", null);
    addChatMember(cid, "ME");
    insertMessage(cid, "ME", "self");
    insertMessage(cid, "ME", "talk");
    const delta = notificationDeltaFor("ME");
    expect(delta.newChats).toEqual([]);
  });

  test("asks deduplicate per-recipient cursor", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("PEER", "/peer", 2);
    insertAsk("PEER", "ME", "q1");
    insertAsk("PEER", "ME", "q2");
    const first = notificationDeltaFor("ME");
    expect(first.newAsks.length).toBe(2);
    advanceNotificationCursors("ME", first);
    const second = notificationDeltaFor("ME");
    expect(second.newAsks).toEqual([]);
    insertAsk("PEER", "ME", "q3");
    const third = notificationDeltaFor("ME");
    expect(third.newAsks.length).toBe(1);
    expect(third.newAsks[0]!.body).toBe("q3");
  });

  test("advanceNotificationCursors never moves a cursor backwards", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("PEER", "/peer", 2);
    const cid = groupChatId("g4");
    ensureChat(cid, "group", null);
    addChatMember(cid, "ME");
    insertMessage(cid, "PEER", "a"); // id 1
    insertMessage(cid, "PEER", "b"); // id 2

    const fullDelta = notificationDeltaFor("ME");
    advanceNotificationCursors("ME", fullDelta);

    // Synthetic stale delta with lower max_message_id should be a no-op.
    const staleDelta = {
      ...fullDelta,
      newChats: [{ ...fullDelta.newChats[0]!, max_message_id: 1 }],
    };
    advanceNotificationCursors("ME", staleDelta);
    const post = notificationDeltaFor("ME");
    expect(post.newChats).toEqual([]); // still up-to-date
  });
});

describe("discoverableGroupsFor", () => {
  test("returns group chats with recent activity that pseudonym is not in", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("ALICE", "/a", 2);
    const cidIn = groupChatId("joined");
    const cidOut = groupChatId("not-joined");
    ensureChat(cidIn, "group", "Joined");
    ensureChat(cidOut, "group", "NotJoined");
    addChatMember(cidIn, "ME");
    addChatMember(cidIn, "ALICE");
    addChatMember(cidOut, "ALICE");
    insertMessage(cidIn, "ALICE", "hi");
    insertMessage(cidOut, "ALICE", "stranger");

    const found = discoverableGroupsFor("ME", 60_000);
    expect(found.length).toBe(1);
    expect(found[0]!.chat.id).toBe(cidOut);
    expect(found[0]!.member_count).toBe(1);
    expect(found[0]!.latest_from).toBe("ALICE");
  });

  test("ignores groups with no recent activity past the activeWithinMs window", () => {
    upsertInstance("ME", "/me", 1);
    upsertInstance("ALICE", "/a", 2);
    const cid = groupChatId("quiet");
    ensureChat(cid, "group", null);
    addChatMember(cid, "ALICE");
    insertMessage(cid, "ALICE", "old");
    Bun.sleepSync(10);
    const found = discoverableGroupsFor("ME", 5);
    expect(found).toEqual([]);
  });
});
