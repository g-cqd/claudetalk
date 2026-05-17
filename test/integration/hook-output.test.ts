/**
 * Drives hooks/check-inbox.ts as a subprocess for each Claude Code hook event
 * and asserts the output matches the event's required schema. Catches the
 * regression we hit on 2026-05: emitting `hookSpecificOutput.additionalContext`
 * for a `Stop` event, which Claude Code's validator rejects.
 *
 * Schema rules per Claude Code docs:
 *   - PostToolUse / PostToolBatch / UserPromptSubmit → hookSpecificOutput.additionalContext
 *   - Stop / SessionStart / SubagentStop / (anything else) → top-level `systemMessage`
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { spawn } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { insertAsk, resetDb, upsertInstance } from "../../src/db.ts";

const HOOK = resolve(import.meta.dir, "..", "..", "hooks", "check-inbox.ts");
const PROJECT_DIR = "/tmp/claudetalk-hook-test-folder";

let HOME: string;

beforeAll(() => {
  HOME = mkdtempSync(join(tmpdir(), "claudetalk-hook-"));
  process.env.CLAUDETALK_HOME = HOME;
  resetDb();
  upsertInstance("ME", PROJECT_DIR, 1);
  upsertInstance("PEER", "/tmp/peer", 2);
  insertAsk("PEER", "ME", "hi me");
});

afterAll(() => {
  resetDb();
  delete process.env.CLAUDETALK_HOME;
  try {
    rmSync(HOME, { recursive: true, force: true });
  } catch {}
});

async function runHook(eventName: string, cwd: string): Promise<unknown | null> {
  const proc = spawn({
    cmd: ["bun", "run", HOOK],
    stdin: "pipe",
    stdout: "pipe",
    stderr: "ignore",
    env: { ...process.env, CLAUDETALK_HOME: HOME, CLAUDE_PROJECT_DIR: cwd },
  });
  proc.stdin.write(JSON.stringify({ hook_event_name: eventName, cwd }));
  await proc.stdin.end();
  const out = await new Response(proc.stdout).text();
  await proc.exited;
  const trimmed = out.trim();
  if (trimmed.length === 0) return null;
  return JSON.parse(trimmed);
}

test("Stop hook emits top-level systemMessage (NOT hookSpecificOutput)", async () => {
  // The hook needs an MCP-discoverable peer too; the asker is the cwd so we
  // simulate "another" peer asking us, then drive the hook from cwd=PROJECT_DIR.
  // Pseudonym for our PROJECT_DIR isn't ME; we use the helper's path resolution
  // which maps the path to a hashed pseudonym. The test passes if we get a
  // *systemMessage*-shaped object back.
  // Insert an ask addressed to whoever the hook computes as "me".
  const { pseudonymFor } = await import("../../src/pseudonym.ts");
  const me = pseudonymFor(PROJECT_DIR);
  insertAsk("PEER", me.pseudonym, "stop-event sample question");

  const out = await runHook("Stop", PROJECT_DIR);
  expect(out).not.toBeNull();
  // Required: top-level systemMessage; forbidden: hookSpecificOutput
  expect(out).toHaveProperty("systemMessage");
  expect((out as any).systemMessage).toContain("ClaudeTalk");
  expect(out).not.toHaveProperty("hookSpecificOutput");
});

test("PostToolUse hook emits hookSpecificOutput.additionalContext", async () => {
  const { pseudonymFor } = await import("../../src/pseudonym.ts");
  const me = pseudonymFor(PROJECT_DIR);
  insertAsk("PEER", me.pseudonym, "posttool sample question");

  const out = await runHook("PostToolUse", PROJECT_DIR);
  expect(out).not.toBeNull();
  expect(out).toHaveProperty("hookSpecificOutput");
  const spec = (out as any).hookSpecificOutput;
  expect(spec.hookEventName).toBe("PostToolUse");
  expect(typeof spec.additionalContext).toBe("string");
  expect(spec.additionalContext).toContain("ClaudeTalk");
  expect(out).not.toHaveProperty("systemMessage");
});

test("SessionStart hook (empty inbox) still greets via top-level systemMessage", async () => {
  // Use a fresh project dir whose pseudonym has no inbox entries.
  const out = await runHook("SessionStart", "/tmp/claudetalk-fresh-project-for-greeting");
  expect(out).not.toBeNull();
  expect(out).toHaveProperty("systemMessage");
  expect((out as any).systemMessage).toMatch(/ClaudeTalk: you are /);
  expect(out).not.toHaveProperty("hookSpecificOutput");
});

test("Stop hook (empty inbox) emits NO output", async () => {
  const out = await runHook("Stop", "/tmp/claudetalk-another-fresh-dir");
  expect(out).toBeNull();
});

test("PostToolUse hook (empty inbox) emits NO output", async () => {
  const out = await runHook("PostToolUse", "/tmp/yet-another-fresh-dir");
  expect(out).toBeNull();
});
