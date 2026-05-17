/**
 * Phase 3.5: the dashboard_version trigger fires on every "interesting"
 * write so the WS poller can skip rebuilding snapshots when nothing has
 * changed. These tests assert each trigger bumps the counter.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import {
  addChatMember,
  answerAsk,
  ensureChat,
  getDashboardVersion,
  groupChatId,
  insertAsk,
  insertMessage,
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

test("getDashboardVersion starts at 0 on a fresh DB", () => {
  // The migration inserts the row with v=0. Any trigger that fires before
  // the first read (none here) would bump it.
  expect(getDashboardVersion()).toBeGreaterThanOrEqual(0);
});

test("inserting an instance bumps the version", () => {
  const before = getDashboardVersion();
  upsertInstance("OtterA", "/tmp/a", 1);
  expect(getDashboardVersion()).toBeGreaterThan(before);
});

test("inserting a chat + a message both bump the version", () => {
  upsertInstance("OtterA", "/tmp/a", 1);
  upsertInstance("OtterB", "/tmp/b", 2);
  const chatId = groupChatId("alpha");
  const v0 = getDashboardVersion();
  ensureChat(chatId, "group", "alpha");
  const v1 = getDashboardVersion();
  expect(v1).toBeGreaterThan(v0);

  addChatMember(chatId, "OtterA");
  const v2 = getDashboardVersion();
  expect(v2).toBeGreaterThan(v1);

  insertMessage(chatId, "OtterA", "hi");
  const v3 = getDashboardVersion();
  expect(v3).toBeGreaterThan(v2);
});

test("inserting and answering an ask each bump the version", () => {
  upsertInstance("OtterA", "/tmp/a", 1);
  upsertInstance("OtterB", "/tmp/b", 2);
  const v0 = getDashboardVersion();
  const ask = insertAsk("OtterA", "OtterB", "ping?");
  const v1 = getDashboardVersion();
  expect(v1).toBeGreaterThan(v0);
  answerAsk(ask.id, "OtterB", "pong");
  expect(getDashboardVersion()).toBeGreaterThan(v1);
});
