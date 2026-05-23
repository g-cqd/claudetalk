/**
 * Phase v0.5.4 (security audit M9): rate-limit bucket initialisation
 * pre-debits from the persisted audit log so a hot crash-loop can't
 * bypass the 30-call/10s ceiling by restarting the process between
 * every call.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import { db, resetDb, upsertInstance } from "../../src/db.ts";
import { _resetAuditLogForTests, flushNow } from "../../src/audit-log.ts";
import { _resetRateLimitsForTests, acquire } from "../../src/rate-limit.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
  resetDb();
  _resetRateLimitsForTests();
  _resetAuditLogForTests();
});

afterEach(() => {
  resetDb();
  _resetRateLimitsForTests();
  _resetAuditLogForTests();
  home.cleanup();
});

function seedRecentAuditRows(pseudonym: string, tool: string, n: number): void {
  upsertInstance(pseudonym, "/p", 1);
  const d = db();
  const now = Date.now();
  const stmt = d.prepare(
    `INSERT INTO tool_calls (pseudonym, tool, args_json, result_summary,
       is_error, error, started_at, duration_ms, kind, direction, jrpc_id)
     VALUES (?, ?, '{}', 'ok', 0, NULL, ?, 1, 'tool', 'in', NULL)`,
  );
  for (let i = 0; i < n; i++) {
    stmt.run(pseudonym, tool, now - i * 10); // all within the 10s window
  }
}

test("acquire returns ok=true with full budget on a clean DB", () => {
  upsertInstance("Alice", "/a", 1);
  const r = acquire("Alice", "chat");
  expect(r.ok).toBe(true);
  expect(r.remaining).toBeGreaterThanOrEqual(28);
});

test("acquire pre-debits from recent audit rows when bucket is fresh", () => {
  // Seed 25 recent calls for Bob/chat. A fresh bucket should start at
  // 30 - 25 = 5 tokens. After this acquire, 4 remaining.
  seedRecentAuditRows("Bob", "chat", 25);
  flushNow();
  const r = acquire("Bob", "chat");
  expect(r.ok).toBe(true);
  expect(r.remaining).toBeLessThanOrEqual(5);
});

test("acquire denies after audit backfill exceeds budget", () => {
  // 30+ recent calls → bucket starts depleted → first acquire denied.
  seedRecentAuditRows("Carol", "chat", 35);
  flushNow();
  const r = acquire("Carol", "chat");
  expect(r.ok).toBe(false);
  expect(r.retry_after_seconds).toBeGreaterThan(0);
});

test("audit backfill is per-(pseudonym, tool) — does not bleed across tools", () => {
  seedRecentAuditRows("Dave", "chat", 30);
  flushNow();
  // chat is depleted but groupchat has its own bucket.
  expect(acquire("Dave", "chat").ok).toBe(false);
  expect(acquire("Dave", "groupchat").ok).toBe(true);
});
