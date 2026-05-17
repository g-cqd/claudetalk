import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  answerAsk,
  ensureChat,
  groupChatId,
  insertAsk,
  insertMessage,
  resetDb,
  upsertInstance,
} from "../../src/db.ts";
import { snapshot } from "../../src/web/snapshot.ts";
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

describe("snapshot", () => {
  test("empty world: zero instances / chats / asks", () => {
    const s = snapshot();
    expect(s.instances).toEqual([]);
    expect(s.chats).toEqual([]);
    expect(s.asks).toEqual([]);
    expect(s.generated_at).toBeGreaterThan(0);
  });

  test("includes instances and counts unread per chat member", () => {
    upsertInstance("Alice", "/a", 1);
    upsertInstance("Bob", "/b", 2);
    const cid = groupChatId("design");
    ensureChat(cid, "group", "Design Review");
    addChatMember(cid, "Alice");
    addChatMember(cid, "Bob");
    insertMessage(cid, "Alice", "hi");
    insertMessage(cid, "Alice", "ping?");
    const s = snapshot();
    expect(s.instances.map((i) => i.pseudonym).sort()).toEqual(["Alice", "Bob"]);
    expect(s.chats.length).toBe(1);
    const c = s.chats[0]!;
    expect(c.chat.title).toBe("Design Review");
    expect(c.members.sort()).toEqual(["Alice", "Bob"]);
    expect(c.unread_per_member.Bob).toBe(2);
    expect(c.unread_per_member.Alice).toBe(0); // Alice is the author
    expect(c.recent_messages.map((m) => m.body)).toEqual(["hi", "ping?"]);
  });

  test("includes pending AND recently-answered asks; drops old answered ones", () => {
    insertAsk("Alice", "Bob", "q1");
    const a2 = insertAsk("Alice", "Bob", "q2");
    answerAsk(a2.id, "Bob", "a2");
    const s = snapshot({ askLookbackMs: 60_000 });
    expect(s.asks.length).toBe(2);
    expect(s.asks.find((a) => a.id === a2.id)?.answer_body).toBe("a2");

    // With a tiny lookback, answered ones older than 0ms get dropped (but
    // pending stays).
    Bun.sleepSync(5);
    const s2 = snapshot({ askLookbackMs: 1 });
    expect(s2.asks.find((a) => a.id === a2.id)).toBeUndefined();
    expect(s2.asks.filter((a) => a.answered_at === null).length).toBe(1);
  });

  test("recentMessages caps the per-chat history slice", () => {
    const cid = groupChatId("g");
    ensureChat(cid, "group", null);
    addChatMember(cid, "X");
    for (let i = 0; i < 30; i++) insertMessage(cid, "X", `m${i}`);
    const s = snapshot({ recentMessages: 5 });
    expect(s.chats[0]!.recent_messages.length).toBe(5);
    expect(s.chats[0]!.recent_messages[0]!.body).toBe("m25");
  });
});
