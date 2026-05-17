import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { _resetRateLimitsForTests, acquire } from "../../src/rate-limit.ts";

beforeEach(() => _resetRateLimitsForTests());
afterEach(() => _resetRateLimitsForTests());

describe("rate limit token bucket", () => {
  test("allows the first 30 calls instantly, denies the 31st", () => {
    let okCount = 0;
    let denyCount = 0;
    for (let i = 0; i < 31; i++) {
      const d = acquire("X", "tool_a");
      if (d.ok) okCount++; else denyCount++;
    }
    expect(okCount).toBe(30);
    expect(denyCount).toBe(1);
  });

  test("retry_after_seconds is positive when denied", () => {
    for (let i = 0; i < 30; i++) acquire("X", "tool_a");
    const d = acquire("X", "tool_a");
    expect(d.ok).toBe(false);
    expect(d.retry_after_seconds).toBeGreaterThan(0);
  });

  test("buckets are independent per (pseudonym, tool)", () => {
    for (let i = 0; i < 30; i++) acquire("A", "tool_a");
    expect(acquire("A", "tool_a").ok).toBe(false);
    expect(acquire("B", "tool_a").ok).toBe(true);
    expect(acquire("A", "tool_b").ok).toBe(true);
  });

  test("tokens refill over time", async () => {
    for (let i = 0; i < 30; i++) acquire("X", "tool_a");
    expect(acquire("X", "tool_a").ok).toBe(false);
    await Bun.sleep(400); // ≥ 1 token's worth of refill (refill ≈ 3/sec)
    expect(acquire("X", "tool_a").ok).toBe(true);
  });
});
