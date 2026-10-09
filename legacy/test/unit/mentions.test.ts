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
  advanceMentionCursor,
  getMentionCursor,
  mentionsForTargetSince,
  parseMentions,
  recordMessageMentions,
} from "../../src/mentions.ts";
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

describe("parseMentions", () => {
  test("returns empty for body with no mentions", () => {
    expect(parseMentions("hello world")).toEqual([]);
  });

  test("captures one pseudonym-shaped @mention", () => {
    expect(parseMentions("hey @SwiftFox-a3f how's it going")).toEqual(["SwiftFox-a3f"]);
  });

  test("captures multiple mentions, deduplicated", () => {
    expect(
      parseMentions("@AmberCrow-5ad @SwiftFox-a3f see also @AmberCrow-5ad"),
    ).toEqual(["AmberCrow-5ad", "SwiftFox-a3f"]);
  });

  test("ignores non-pseudonym tokens like @bob or @123", () => {
    expect(parseMentions("@bob @123 @swiftfox-a3f @SwiftFox-XYZ")).toEqual([]);
  });
});

describe("recordMessageMentions", () => {
  function seedTwoInstances() {
    upsertInstance("Alice", "/a", 1);
    upsertInstance("SwiftFox-a3f", "/sf", 2);
    const chatId = groupChatId("g");
    ensureChat(chatId, "group", null);
    addChatMember(chatId, "Alice");
    addChatMember(chatId, "SwiftFox-a3f");
    return chatId;
  }

  test("records a row for each known mentioned pseudonym", () => {
    const cid = seedTwoInstances();
    const m = insertMessage(cid, "Alice", "@SwiftFox-a3f take a look");
    recordMessageMentions(m.id, m.body, "Alice");
    const rows = mentionsForTargetSince("SwiftFox-a3f", 0);
    expect(rows.length).toBe(1);
    expect(rows[0]!.message_id).toBe(m.id);
    expect(rows[0]!.from_pseudonym).toBe("Alice");
  });

  test("silently drops mentions of unknown pseudonyms", () => {
    const cid = seedTwoInstances();
    const m = insertMessage(cid, "Alice", "@GhostFox-zzz are you there?");
    recordMessageMentions(m.id, m.body, "Alice");
    expect(mentionsForTargetSince("GhostFox-zzz", 0)).toEqual([]);
  });

  test("self-mention is dropped (you can't @ yourself)", () => {
    const cid = seedTwoInstances();
    upsertInstance("LonePhoenix-553", "/p", 9);
    const m = insertMessage(cid, "LonePhoenix-553", "noted by @LonePhoenix-553");
    recordMessageMentions(m.id, m.body, "LonePhoenix-553");
    expect(mentionsForTargetSince("LonePhoenix-553", 0)).toEqual([]);
  });
});

describe("mention cursor", () => {
  test("getMentionCursor defaults to 0 for fresh instance", () => {
    upsertInstance("X", "/x", 1);
    expect(getMentionCursor("X")).toBe(0);
  });

  test("advanceMentionCursor monotonically; never moves backwards", () => {
    upsertInstance("X", "/x", 1);
    advanceMentionCursor("X", 5);
    expect(getMentionCursor("X")).toBe(5);
    advanceMentionCursor("X", 3);
    expect(getMentionCursor("X")).toBe(5);
    advanceMentionCursor("X", 12);
    expect(getMentionCursor("X")).toBe(12);
  });

  test("mentionsForTargetSince respects the cursor", () => {
    upsertInstance("Alice", "/a", 1);
    upsertInstance("SwiftFox-a3f", "/sf", 2);
    const cid = groupChatId("g");
    ensureChat(cid, "group", null);
    addChatMember(cid, "Alice");
    addChatMember(cid, "SwiftFox-a3f");
    const m1 = insertMessage(cid, "Alice", "@SwiftFox-a3f first");
    recordMessageMentions(m1.id, m1.body, "Alice");
    const m2 = insertMessage(cid, "Alice", "@SwiftFox-a3f second");
    recordMessageMentions(m2.id, m2.body, "Alice");

    expect(mentionsForTargetSince("SwiftFox-a3f", 0).length).toBe(2);
    // Cursor is seq-based now (UUIDs aren't orderable).
    expect(mentionsForTargetSince("SwiftFox-a3f", m1.seq).length).toBe(1);
    expect(mentionsForTargetSince("SwiftFox-a3f", m2.seq).length).toBe(0);
  });
});
