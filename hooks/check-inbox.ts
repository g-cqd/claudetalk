#!/usr/bin/env bun
/**
 * Claude Code hook entrypoint. Triggered on:
 *   - SessionStart     (no matcher)                      → top-level `systemMessage`
 *   - UserPromptSubmit (no matcher)                      → hookSpecificOutput.additionalContext
 *   - PostToolUse      (matcher: mcp__claudetalk__.*)    → hookSpecificOutput.additionalContext
 *   - PostToolBatch    (no matcher)                      → hookSpecificOutput.additionalContext
 *   - SubagentStop     (no matcher)                      → top-level `systemMessage`
 *   - Stop             (no matcher)                      → top-level `systemMessage`
 *
 * The hook emits a HEADER-ONLY summary ("N new — X DMs from A, Y in #Z. Call
 * mcp__claudetalk__inbox for bodies.") and uses per-(viewer, chat) +
 * per-viewer cursors to suppress duplicate notifications. Without the cursor,
 * every PostToolUse re-injected the full body of the latest message, costing
 * thousands of tokens per session (reported by OnyxKraken-7ba 2026-05-17).
 *
 * Failure mode: never block the hook. Any error → exit 0 with no output.
 */
import { resolve } from "node:path";
import { pseudonymFor } from "../src/pseudonym.ts";
import {
  advanceNotificationCursors,
  notificationDeltaFor,
  type NotificationDelta,
} from "../src/db.ts";

async function readStdin(): Promise<string> {
  if (process.stdin.isTTY) return "";
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks).toString("utf8");
}

const ADDITIONAL_CONTEXT_EVENTS = new Set([
  "PostToolUse",
  "PostToolBatch",
  "UserPromptSubmit",
]);

function emit(eventName: string, context: string): void {
  const out: Record<string, unknown> = ADDITIONAL_CONTEXT_EVENTS.has(eventName)
    ? {
        hookSpecificOutput: {
          hookEventName: eventName,
          additionalContext: context,
        },
      }
    : { systemMessage: context };
  process.stdout.write(JSON.stringify(out));
}

/** Header-only summary: counts + senders, no message bodies. */
function summarise(pseudonym: string, delta: NotificationDelta): string {
  const parts: string[] = [];
  if (delta.newAsks.length > 0) {
    const fromCounts = new Map<string, number>();
    for (const a of delta.newAsks) fromCounts.set(a.from_pseudonym, (fromCounts.get(a.from_pseudonym) ?? 0) + 1);
    const askers = [...fromCounts.entries()]
      .map(([who, n]) => (n > 1 ? `${who} (${n})` : who))
      .join(", ");
    parts.push(`${delta.newAsks.length} pending ask(s) from ${askers}`);
  }
  if (delta.newChats.length > 0) {
    const fragments = delta.newChats.map((c) => {
      const label = c.chat.kind === "direct"
        ? `DM from ${c.latest.from_pseudonym}`
        : `#${c.chat.title ?? c.chat.id.replace(/^group:/, "")}`;
      return c.new_count === 1 ? label : `${label} ×${c.new_count}`;
    });
    parts.push(`${delta.newChats.length} chat(s): ${fragments.join(", ")}`);
  }
  return (
    `ClaudeTalk (${pseudonym}): ${parts.join(" • ")}. ` +
    "Call mcp__claudetalk__inbox to read; mcp__claudetalk__answer for asks; " +
    "mcp__claudetalk__chat / mcp__claudetalk__groupchat to reply."
  );
}

async function main(): Promise<void> {
  let event: any = {};
  try {
    const raw = await readStdin();
    if (raw.trim().length > 0) event = JSON.parse(raw);
  } catch {
    /* malformed stdin → fall through */
  }
  const eventName: string =
    event.hook_event_name ?? event.hookEventName ?? "PostToolUse";

  const projectDir = resolve(
    event.cwd ?? process.env.CLAUDE_PROJECT_DIR ?? process.cwd(),
  );
  const me = pseudonymFor(projectDir);

  let delta: NotificationDelta;
  try {
    delta = notificationDeltaFor(me.pseudonym);
  } catch {
    return; // DB unavailable / contended → silent
  }

  const hasNew = delta.newAsks.length > 0 || delta.newChats.length > 0;

  // SessionStart always greets so Claude learns its identity, even with
  // empty inbox.
  if (eventName === "SessionStart" && !hasNew) {
    emit(
      eventName,
      `ClaudeTalk: you are ${me.pseudonym} (folder ${me.path}). ` +
        "Inbox empty. Call mcp__claudetalk__discover to see who else is online.",
    );
    return;
  }

  if (!hasNew) return; // nothing strictly new since last notification → silent

  emit(eventName, summarise(me.pseudonym, delta));

  // Advance cursors so subsequent hook fires won't re-emit this same delta.
  // Failure here is non-fatal — worst case we re-notify once.
  try {
    advanceNotificationCursors(me.pseudonym, delta);
  } catch {}
}

main().catch(() => {
  /* never block the hook on errors */
});
