#!/usr/bin/env bun
/**
 * Claude Code hook entrypoint. See bin/cli.ts ensureHooks() for the events
 * wired up. Emits a HEADER-ONLY summary (no message bodies) and uses
 * per-(viewer, chat) + per-viewer cursors to suppress duplicate notifications.
 *
 * Phase 1.2 (mentions): mentions break out of the regular dedup envelope —
 * if you're @-mentioned in a new message, the hook surfaces a `[!]` marker
 * even if your chat cursor has already advanced.
 *
 * Phase 1.4 (smart suggestion): when exactly one new item exists, the
 * footer suggests the EXACT follow-up tool to call rather than the
 * generic "call inbox / answer / chat" line.
 *
 * Failure mode: never block the hook. Any error → exit 0 with no output.
 */
import { resolve } from "node:path";
import { pseudonymFor } from "../src/pseudonym.ts";
import {
  advanceNotificationCursors,
  type NotificationDelta,
  notificationDeltaFor,
} from "../src/db.ts";
import {
  advanceMentionCursor,
  getMentionCursor,
  type MentionForTarget,
  mentionsForTargetSince,
} from "../src/mentions.ts";

/** Cap on bytes read from the hook's stdin. Claude Code's hook payload is
 *  bounded (a few KB max in practice). If something is piping multi-MB
 *  into our hook, that's a bug or an attack — refuse rather than allocate. */
const STDIN_MAX_BYTES = 256 * 1024;

async function readStdin(): Promise<string> {
  if (process.stdin.isTTY) return "";
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of process.stdin) {
    const buf = chunk as Buffer;
    total += buf.length;
    if (total > STDIN_MAX_BYTES) return ""; // silently abort; hook is best-effort
    chunks.push(buf);
  }
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

/** Header-only summary: counts + senders, no message bodies. Mentions are
 *  flagged with `[!]` and an explicit "mentioned by X" line. When only one
 *  item is new, the footer suggests the exact follow-up tool. */
function summarise(
  pseudonym: string,
  delta: NotificationDelta,
  mentions: MentionForTarget[],
): string {
  const parts: string[] = [];
  const prefix = mentions.length > 0 ? "[!] " : "";

  if (mentions.length > 0) {
    const senders = [...new Set(mentions.map((m) => m.from_pseudonym))];
    parts.push(
      `mentioned by ${senders.join(", ")} (${mentions.length} message${mentions.length === 1 ? "" : "s"})`,
    );
  }

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
      const replyMarker = c.latest.parent_id !== null && c.latest.parent_id !== undefined
        ? " (reply)"
        : "";
      return c.new_count === 1
        ? `${label}${replyMarker}`
        : `${label}${replyMarker} ×${c.new_count}`;
    });
    parts.push(`${delta.newChats.length} chat(s): ${fragments.join(", ")}`);
  }

  // Footer: exact-tool suggestion when only one item is pending, else generic.
  const totalAsks = delta.newAsks.length;
  const totalChats = delta.newChats.length;
  const totalMentions = mentions.length;
  let footer = "";
  if (totalAsks === 1 && totalChats === 0 && totalMentions === 0) {
    footer = `Call mcp__claudetalk__answer ask_id=${delta.newAsks[0]!.id} to reply.`;
  } else if (totalAsks === 0 && totalChats === 1 && totalMentions === 0) {
    const c = delta.newChats[0]!;
    if (c.chat.kind === "direct") {
      const peer = c.chat.id.replace(/^direct:/, "").split("|").find((p) => p !== pseudonym) ?? "?";
      footer = `Call mcp__claudetalk__chat with=${peer} to read+reply.`;
    } else {
      const slug = c.chat.id.replace(/^group:/, "");
      footer = `Call mcp__claudetalk__groupchat slug=${slug} to read+reply.`;
    }
  } else {
    footer = "Call mcp__claudetalk__inbox to read; mcp__claudetalk__answer for asks; mcp__claudetalk__chat / mcp__claudetalk__groupchat to reply.";
  }

  return `${prefix}ClaudeTalk (${pseudonym}): ${parts.join(" • ")}. ${footer}`;
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
  let mentions: MentionForTarget[];
  let mentionCursor: number;
  try {
    delta = notificationDeltaFor(me.pseudonym);
    mentionCursor = getMentionCursor(me.pseudonym);
    mentions = mentionsForTargetSince(me.pseudonym, mentionCursor);
  } catch {
    return; // DB unavailable / contended → silent
  }

  const hasNew =
    delta.newAsks.length > 0 || delta.newChats.length > 0 || mentions.length > 0;

  if (eventName === "SessionStart" && !hasNew) {
    emit(
      eventName,
      `ClaudeTalk: you are ${me.pseudonym} (folder ${me.path}). ` +
        `Inbox empty. Call mcp__claudetalk__discover to see who else is online.`,
    );
    return;
  }

  if (!hasNew) return;

  emit(eventName, summarise(me.pseudonym, delta, mentions));

  try {
    advanceNotificationCursors(me.pseudonym, delta);
    if (mentions.length > 0) {
      const maxMentionSeq = Math.max(...mentions.map((m) => m.message_seq));
      advanceMentionCursor(me.pseudonym, maxMentionSeq);
    }
  } catch {}
}

main().catch(() => {
  /* never block the hook on errors */
});
