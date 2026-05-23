/**
 * Reactions on chat messages. Each (message_id, reactor) holds one
 * reaction; re-reacting replaces the previous value. Reactions are
 * intentionally lightweight — they do NOT bump the chat dedup cursor, so a
 * thumbs-up doesn't re-fire the hook for everyone in the chat.
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import { _now, db, getMessage, listChatMembers, touchInstance } from "./db.ts";
import { ErrorCode, toolError, toolText } from "./errors.ts";

const REACTION_MAX_LEN = 32;
/** Allowed reaction characters: Unicode letters/digits, underscore, dash,
 *  plus, and the full Unicode pictographic / emoji range. We rely on
 *  Unicode property classes — any visible non-whitespace token under the
 *  length cap qualifies. */
const REACTION_RE = /^[\p{L}\p{N}\p{Emoji}\p{Extended_Pictographic}_+\-]{1,32}$/u;

export interface ReactionRow {
  /** UUID of the reacted-to message (matches messages.id). */
  message_id: string;
  reactor: string;
  reaction: string;
  created_at: number;
}

export function setReaction(messageId: string, reactor: string, reaction: string): void {
  db().run(
    `INSERT INTO message_reactions (message_id, reactor, reaction, created_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(message_id, reactor) DO UPDATE SET
       reaction = excluded.reaction,
       created_at = excluded.created_at`,
    [messageId, reactor, reaction, _now()],
  );
}

export function clearReaction(messageId: string, reactor: string): boolean {
  const res = db().run(
    "DELETE FROM message_reactions WHERE message_id = ? AND reactor = ?",
    [messageId, reactor],
  );
  return res.changes > 0;
}

export function listReactionsFor(messageId: string): ReactionRow[] {
  return db()
    .query<ReactionRow, [string]>(
      `SELECT message_id, reactor, reaction, created_at
       FROM message_reactions WHERE message_id = ?
       ORDER BY created_at ASC`,
    )
    .all(messageId);
}

/** Render reactions as " · 👍 from A,B · ✓ from C", or empty string. */
export function summariseReactions(messageId: string): string {
  const rows = listReactionsFor(messageId);
  return summariseFromRows(rows);
}

function summariseFromRows(rows: ReactionRow[]): string {
  if (rows.length === 0) return "";
  const byReaction = new Map<string, string[]>();
  for (const r of rows) {
    const list = byReaction.get(r.reaction) ?? [];
    list.push(r.reactor);
    byReaction.set(r.reaction, list);
  }
  const parts: string[] = [];
  for (const [reaction, reactors] of byReaction) {
    parts.push(`${reaction} from ${reactors.join(",")}`);
  }
  return `  · ${parts.join(" · ")}`;
}

/** Phase v0.5.2 perf: one query for N message ids instead of N queries.
 *  Returns a map from message UUID → pre-rendered summary string (empty
 *  for ids with no reactions). Used by fmtMessageList to avoid the N+1
 *  pattern when rendering a chat slice. */
export function summariseReactionsBatch(messageIds: string[]): Map<string, string> {
  const out = new Map<string, string>();
  if (messageIds.length === 0) return out;
  const placeholders = messageIds.map(() => "?").join(",");
  const rows = db()
    .query<ReactionRow, string[]>(
      `SELECT message_id, reactor, reaction, created_at
       FROM message_reactions WHERE message_id IN (${placeholders})
       ORDER BY message_id ASC, created_at ASC`,
    )
    .all(...messageIds);
  // Group by message_id.
  const byMsg = new Map<string, ReactionRow[]>();
  for (const r of rows) {
    const list = byMsg.get(r.message_id) ?? [];
    list.push(r);
    byMsg.set(r.message_id, list);
  }
  for (const id of messageIds) {
    out.set(id, summariseFromRows(byMsg.get(id) ?? []));
  }
  return out;
}

// text/error helpers come from src/errors.ts (Phase 5.4 — codes).
const text = (s: string) => toolText(s);
const error = (s: string, code: ErrorCode = ErrorCode.UNSPECIFIED) => toolError(s, code);

export function registerReactionTools(server: McpServer, me: Identity): void {
  server.registerTool(
    "react",
    {
      title: "React to a chat message with a short reaction",
      description:
        "Lightweight acknowledgement: attach a single emoji or short token (≤32 chars, no whitespace) " +
        "to a message id. Re-reacting replaces your previous reaction on that message. Pass empty " +
        "reaction to remove. Reactions do NOT bump the chat dedup cursor — they're cheap, no hook fires.",
      inputSchema: {
        message_seq: z
          .number()
          .int()
          .min(1)
          .describe(
            "The message's seq number — the [N] label shown in chat/groupchat/inbox output.",
          ),
        reaction: z
          .string()
          .describe(
            "Short reaction: emoji or word, ≤32 chars, no whitespace. " +
              "Examples: '👍', '✓', 'lgtm', '🎉'. Pass empty to clear.",
          ),
      },
    },
    async ({ message_seq, reaction }) => {
      touchInstance(me.pseudonym);
      const msg = getMessage(message_seq);
      if (!msg) return error(`Unknown message_seq ${message_seq}.`);
      // Only chat members can react.
      const members = listChatMembers(msg.chat_id).map((m) => m.pseudonym);
      if (!members.includes(me.pseudonym)) {
        return error(`You're not a member of ${msg.chat_id}; cannot react.`);
      }
      if (reaction.trim().length === 0) {
        const cleared = clearReaction(msg.id, me.pseudonym);
        return text(
          cleared
            ? `Cleared your reaction on message ${message_seq}.`
            : `No reaction was set on message ${message_seq}.`,
        );
      }
      const trimmed = reaction.trim();
      if (!REACTION_RE.test(trimmed)) {
        return error(
          `Reaction '${trimmed}' invalid. Must be 1-${REACTION_MAX_LEN} chars, no whitespace, emoji or short token.`,
        );
      }
      setReaction(msg.id, me.pseudonym, trimmed);
      return text(`Reacted to message ${message_seq} with '${trimmed}'.`);
    },
  );
}
