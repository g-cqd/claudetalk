/**
 * formatReplayReport — pure rendering of a ReplayReport. The actual
 * runReplay() spawns a subprocess MCP server; that path is covered
 * by integration tests indirectly via the multi-instance suite.
 */
import { expect, test } from "bun:test";
import { formatReplayReport, type ReplayReport } from "../../src/replay.ts";

function mkReport(diffs: ReplayReport["diffs"], overrides: Partial<ReplayReport> = {}): ReplayReport {
  return {
    pseudonym: "TestOtter-001",
    homeDir: "/tmp/claudetalk-replay-xyz",
    startedAt: 0,
    durationMs: 12,
    rowsConsidered: diffs.length,
    rowsReplayed: diffs.length,
    matches: diffs.filter((d) => d.match).length,
    mismatches: diffs.filter((d) => !d.match && !d.error).length,
    errors: diffs.filter((d) => d.error !== null).length,
    diffs,
    ...overrides,
  };
}

test("formatReplayReport: all-match summary hides per-row detail by default", () => {
  const r = mkReport([
    { id: 1, tool: "whoami", originalSummary: "x", replayedSummary: "x", match: true, error: null },
    { id: 2, tool: "inbox", originalSummary: "y", replayedSummary: "y", match: true, error: null },
  ]);
  const out = formatReplayReport(r, false);
  expect(out).toContain("Replay of 'TestOtter-001'");
  expect(out).toContain("match=2  mismatch=0  error=0");
  expect(out).not.toContain("#1 whoami");
});

test("formatReplayReport: verbose shows every row", () => {
  const r = mkReport([
    { id: 1, tool: "whoami", originalSummary: "x", replayedSummary: "x", match: true, error: null },
  ]);
  const out = formatReplayReport(r, true);
  expect(out).toContain("[OK  ] #1 whoami");
});

test("formatReplayReport: mismatches surface side-by-side, even non-verbose", () => {
  const r = mkReport([
    { id: 5, tool: "discover", originalSummary: "old", replayedSummary: "new", match: false, error: null },
  ]);
  const out = formatReplayReport(r, false);
  expect(out).toContain("[DIFF] #5 discover");
  expect(out).toContain("original: old");
  expect(out).toContain("replayed: new");
});

test("formatReplayReport: errors include the error message", () => {
  const r = mkReport([
    { id: 7, tool: "ask", originalSummary: "ok", replayedSummary: null, match: false, error: "boom" },
  ]);
  const out = formatReplayReport(r, false);
  expect(out).toContain("[ERR ] #7 ask");
  expect(out).toContain("error: boom");
});

test("formatReplayReport: truncates long bodies", () => {
  const long = "x".repeat(500);
  const r = mkReport([
    { id: 1, tool: "chat", originalSummary: long, replayedSummary: "y", match: false, error: null },
  ]);
  const out = formatReplayReport(r, false);
  expect(out).toContain("…");
});
