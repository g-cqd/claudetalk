import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  answerAsk,
  directChatId,
  ensureChat,
  getAsk,
  getChat,
  getInstance,
  groupChatId,
  insertAsk,
  insertMessage,
  listAnsweredAsksFrom,
  listChatMembers,
  listChatsFor,
  listInstances,
  listMessages,
  listPendingAsksFor,
  markChatRead,
  resetDb,
  touchInstance,
  unreadSummary,
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

describe("directChatId", () => {
  test("is order-independent", () => {
    expect(directChatId("A", "B")).toBe(directChatId("B", "A"));
  });
  test("namespaces with 'direct:' and uses '|' separator", () => {
    expect(directChatId("A", "B")).toMatch(/^direct:[^|]+\|[^|]+$/);
  });
});

test("groupChatId namespaces with 'group:'", () => {
  expect(groupChatId("design")).toBe("group:design");
});

describe("instances + presence", () => {
  test("upsertInstance + getInstance + touchInstance", () => {
    upsertInstance("A", "/path/a", 1234);
    let row = getInstance("A");
    expect(row).not.toBeNull();
    expect(row!.pseudonym).toBe("A");
    expect(row!.path).toBe("/path/a");
    expect(row!.pid).toBe(1234);
    const first = row!.last_seen;

    Bun.sleepSync(5);
    touchInstance("A");
    row = getInstance("A");
    expect(row!.last_seen).toBeGreaterThan(first);
  });

  test("listInstances filters by activity window", () => {
    upsertInstance("A", "/a", 1);
    upsertInstance("B", "/b", 2);
    const all = listInstances(60_000);
    expect(all.map((i) => i.pseudonym).sort()).toEqual(["A", "B"]);
    // After waiting past the window, both fall out.
    Bun.sleepSync(10);
    expect(listInstances(5)).toEqual([]);
  });
});

describe("asks (Q&A)", () => {
  test("insertAsk creates a pending ask; answerAsk resolves it", () => {
    upsertInstance("A", "/a", 1);
    upsertInstance("B", "/b", 2);
    const ask = insertAsk("A", "B", "ping?");
    expect(ask.id).toBeGreaterThan(0);
    expect(getAsk(ask.id)?.answered_at).toBeNull();

    const after = answerAsk(ask.id, "B", "pong!");
    expect(after?.answered_at).not.toBeNull();
    expect(after?.answer_body).toBe("pong!");
  });

  test("answerAsk refuses if the answerer is not the addressee", () => {
    upsertInstance("A", "/a", 1);
    const ask = insertAsk("A", "B", "ping?");
    const res = answerAsk(ask.id, "C", "no!");
    expect(res).toBeNull();
    expect(getAsk(ask.id)?.answered_at).toBeNull();
  });

  test("answerAsk is idempotent: a second answer does not overwrite", () => {
    const ask = insertAsk("A", "B", "ping?");
    answerAsk(ask.id, "B", "first");
    const second = answerAsk(ask.id, "B", "second");
    expect(second?.answer_body).toBe("first");
  });

  test("listPendingAsksFor / listAnsweredAsksFrom partition correctly", () => {
    const a1 = insertAsk("A", "B", "q1");
    const a2 = insertAsk("A", "B", "q2");
    insertAsk("X", "Y", "unrelated"); // separate pair

    expect(listPendingAsksFor("B").map((a) => a.id).sort()).toEqual([a1.id, a2.id]);
    expect(listAnsweredAsksFrom("A", 0)).toEqual([]);

    answerAsk(a1.id, "B", "ok");
    expect(listPendingAsksFor("B").map((a) => a.id)).toEqual([a2.id]);
    expect(listAnsweredAsksFrom("A", 0).map((a) => a.id)).toEqual([a1.id]);
  });
});

describe("chats + messages", () => {
  test("ensureChat + addChatMember + listChatMembers", () => {
    const cid = directChatId("Me", "You");
    ensureChat(cid, "direct", null);
    addChatMember(cid, "Me");
    addChatMember(cid, "You");
    addChatMember(cid, "Me"); // dedupes
    expect(listChatMembers(cid).map((m) => m.pseudonym).sort()).toEqual(["Me", "You"]);
    expect(getChat(cid)?.kind).toBe("direct");
  });

  test("insertMessage + listMessages with since_id cursor", () => {
    const cid = groupChatId("g1");
    ensureChat(cid, "group", null);
    addChatMember(cid, "A");
    insertMessage(cid, "A", "hi");
    insertMessage(cid, "A", "again");
    insertMessage(cid, "A", "third");
    const all = listMessages(cid, 0, 100);
    expect(all.length).toBe(3);
    const tail = listMessages(cid, all[0]!.id, 100);
    expect(tail.length).toBe(2);
  });

  test("markChatRead never moves the cursor backwards", () => {
    const cid = groupChatId("g2");
    ensureChat(cid, "group", null);
    addChatMember(cid, "A");
    insertMessage(cid, "A", "m1");
    insertMessage(cid, "A", "m2");
    markChatRead(cid, "A", 5);
    markChatRead(cid, "A", 3); // should not regress
    const chats = listChatsFor("A");
    expect(chats[0]!.member.last_read_message_id).toBe(5);
  });
});

describe("unreadSummary", () => {
  test("returns empty when nothing is pending and nothing unread", () => {
    upsertInstance("Me", "/me", 1);
    const u = unreadSummary("Me");
    expect(u.pendingAsks).toEqual([]);
    expect(u.unreadChats).toEqual([]);
  });

  test("counts only messages from OTHERS as unread", () => {
    upsertInstance("Me", "/me", 1);
    const cid = directChatId("Me", "You");
    ensureChat(cid, "direct", null);
    addChatMember(cid, "Me");
    addChatMember(cid, "You");
    insertMessage(cid, "Me", "self-talk"); // doesn't count
    insertMessage(cid, "You", "hi");
    insertMessage(cid, "You", "are you there?");
    const u = unreadSummary("Me");
    expect(u.unreadChats.length).toBe(1);
    expect(u.unreadChats[0]!.unreadCount).toBe(2);
  });

  test("includes pending asks addressed to me", () => {
    upsertInstance("Me", "/me", 1);
    insertAsk("Friend", "Me", "what's up?");
    const u = unreadSummary("Me");
    expect(u.pendingAsks.length).toBe(1);
    expect(u.pendingAsks[0]!.body).toBe("what's up?");
  });
});
