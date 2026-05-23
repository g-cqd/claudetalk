/**
 * v0.5.3 (perf audit M1) introduced a 10k-row hard cap on the in-memory
 * audit log queue. Under sustained DB-busy contention the 200ms batched
 * flusher could fall behind unboundedly; now it drops the oldest row
 * with a single warning instead.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import { resetDb } from "../../src/db.ts";
import {
  _enqueueForTests,
  _queueLengthForTests,
  _resetAuditLogForTests,
} from "../../src/audit-log.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };
const CAP = 10_000;

beforeEach(() => {
  home = isolatedHome();
  resetDb();
  _resetAuditLogForTests();
});

afterEach(() => {
  resetDb();
  _resetAuditLogForTests();
  home.cleanup();
});

function row(i: number) {
  return {
    pseudonym: "P",
    tool: "noop",
    args_json: `{"i":${i}}`,
    result_summary: "ok",
    is_error: false,
    error: null,
    started_at: Date.now(),
    duration_ms: 1,
  };
}

test("queue grows to but never exceeds the hard cap", () => {
  for (let i = 0; i < CAP; i++) _enqueueForTests(row(i));
  expect(_queueLengthForTests()).toBe(CAP);
  // Push past the cap; queue stays at CAP, oldest dropped.
  for (let i = 0; i < 500; i++) _enqueueForTests(row(CAP + i));
  expect(_queueLengthForTests()).toBe(CAP);
});

test("overflow warning emits once per overflow event (not per drop)", () => {
  const seen: string[] = [];
  const orig = console.error;
  console.error = (...args: unknown[]) => {
    seen.push(args.join(" "));
  };
  try {
    for (let i = 0; i < CAP + 100; i++) _enqueueForTests(row(i));
  } finally {
    console.error = orig;
  }
  const overflowLines = seen.filter((s) => s.includes("audit log queue hit"));
  expect(overflowLines.length).toBe(1);
});
