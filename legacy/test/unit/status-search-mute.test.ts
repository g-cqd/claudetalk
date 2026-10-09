/**
 * Phase 2 helpers — status / search / mute. Direct tests against the
 * storage layer; the registered MCP tools are exercised end-to-end via
 * the multi-instance integration test.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  addChatMember,
  ensureChat,
  groupChatId,
  insertAsk,
  insertMessage,
  resetDb,
  upsertInstance,
  answerAsk,
} from "../../src/db.ts";
import { clearStatus, fmtStatus, getStatus, setStatus } from "../../src/status.ts";
import { isChatMutedFor, listMutedChatsFor, setChatMuted } from "../../src/mute.ts";
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

describe("status", () => {
  test("setStatus/getStatus round-trips and re-set replaces", () => {
    upsertInstance("X", "/x", 1);
    setStatus("X", "busy", "🟠");
    let row = getStatus("X");
    expect(row?.status).toBe("busy");
    expect(row?.emoji).toBe("🟠");
    setStatus("X", "available", null);
    row = getStatus("X");
    expect(row?.status).toBe("available");
    expect(row?.emoji).toBeNull();
  });

  test("clearStatus removes the row", () => {
    upsertInstance("X", "/x", 1);
    setStatus("X", "busy", null);
    expect(clearStatus("X")).toBe(true);
    expect(getStatus("X")).toBeNull();
    expect(clearStatus("X")).toBe(false);
  });

  test("fmtStatus renders emoji + text, or text alone, or empty for null", () => {
    expect(fmtStatus(null)).toBe("");
    expect(fmtStatus({ pseudonym: "X", status: "busy", emoji: "🟠", updated_at: 0 })).toBe("🟠 busy");
    expect(fmtStatus({ pseudonym: "X", status: "available", emoji: null, updated_at: 0 })).toBe(
      "available",
    );
  });
});

describe("mute", () => {
  test("isChatMutedFor defaults false; setChatMuted toggles persistently", () => {
    upsertInstance("X", "/x", 1);
    const cid = groupChatId("g");
    ensureChat(cid, "group", null);
    expect(isChatMutedFor("X", cid)).toBe(false);
    setChatMuted("X", cid, true);
    expect(isChatMutedFor("X", cid)).toBe(true);
    setChatMuted("X", cid, false);
    expect(isChatMutedFor("X", cid)).toBe(false);
  });

  test("listMutedChatsFor returns only muted chats for the viewer", () => {
    upsertInstance("X", "/x", 1);
    upsertInstance("Y", "/y", 2);
    const a = groupChatId("a");
    const b = groupChatId("b");
    ensureChat(a, "group", null);
    ensureChat(b, "group", null);
    setChatMuted("X", a, true);
    setChatMuted("Y", b, true);
    expect(listMutedChatsFor("X")).toEqual([a]);
    expect(listMutedChatsFor("Y")).toEqual([b]);
  });
});

describe("search (via dynamic import to keep server-side tools out of unit scope)", () => {
  // We test the underlying SQL semantics by inserting fixture data and
  // querying through plain LIKE — mirrors what the registered tool does.
  test("LIKE-based substring match across messages + asks", async () => {
    upsertInstance("Alice", "/a", 1);
    upsertInstance("Bob", "/b", 2);
    const cid = groupChatId("dev");
    ensureChat(cid, "group", null);
    addChatMember(cid, "Alice");
    addChatMember(cid, "Bob");
    insertMessage(cid, "Alice", "I love SwiftUI");
    insertMessage(cid, "Bob", "Swift is great");
    insertMessage(cid, "Alice", "unrelated");
    const ask = insertAsk("Alice", "Bob", "How do you handle SwiftData migrations?");
    answerAsk(ask.id, "Bob", "Carefully and with NSPersistentContainer.");

    // Replicates registerSearchTool's queries — easier than spinning up MCP.
    const { db } = await import("../../src/db.ts");
    const needle = "%swift%";
    const chatHits = db()
      .query<{ id: number; body: string }, [string]>(
        "SELECT id, body FROM messages WHERE body LIKE ? COLLATE NOCASE",
      )
      .all(needle);
    expect(chatHits.length).toBe(2);
    const askHits = db()
      .query<{ id: number; body: string }, [string, string]>(
        "SELECT id, body FROM asks WHERE body LIKE ? COLLATE NOCASE OR answer_body LIKE ? COLLATE NOCASE",
      )
      .all(needle, needle);
    expect(askHits.length).toBe(1);
  });
});
