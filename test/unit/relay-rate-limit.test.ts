/**
 * Phase N3: per-namespace rate limiter at the relay. Window-bucket
 * counter; should admit up to N frames per window per namespace, reject
 * the rest until the window rolls over, and never cross-contaminate
 * across namespaces.
 */
import { expect, test } from "bun:test";
import { NamespaceRateLimiter } from "../../relay/src/rate-limit.ts";

test("allows up to framesPerWindow then rejects", () => {
  const r = new NamespaceRateLimiter({ framesPerWindow: 3, windowMs: 10_000 });
  const now = 1_000_000;
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now + 1)).toBe(true);
  expect(r.allow("ns-a", now + 2)).toBe(true);
  expect(r.allow("ns-a", now + 3)).toBe(false);
  expect(r.allow("ns-a", now + 4)).toBe(false);
});

test("window rollover refills the budget", () => {
  const r = new NamespaceRateLimiter({ framesPerWindow: 2, windowMs: 1_000 });
  const now = 1_000_000;
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now)).toBe(false);
  // Window rolls over at +1000ms.
  expect(r.allow("ns-a", now + 1_001)).toBe(true);
  expect(r.allow("ns-a", now + 1_001)).toBe(true);
  expect(r.allow("ns-a", now + 1_001)).toBe(false);
});

test("namespaces don't share budgets", () => {
  const r = new NamespaceRateLimiter({ framesPerWindow: 2, windowMs: 10_000 });
  const now = 1_000_000;
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now)).toBe(false);
  // Other namespace gets its own full budget.
  expect(r.allow("ns-b", now)).toBe(true);
  expect(r.allow("ns-b", now)).toBe(true);
  expect(r.allow("ns-b", now)).toBe(false);
});

test("_resetForTests clears all buckets", () => {
  const r = new NamespaceRateLimiter({ framesPerWindow: 1, windowMs: 10_000 });
  const now = 1_000_000;
  expect(r.allow("ns-a", now)).toBe(true);
  expect(r.allow("ns-a", now)).toBe(false);
  r._resetForTests();
  expect(r.allow("ns-a", now)).toBe(true);
});
