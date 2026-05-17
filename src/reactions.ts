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

const REACTION_MAX_LEN = 32;
/** Allowed reaction characters: Unicode letters/digits, underscore, dash,
 *  plus, and the full Unicode pictographic / emoji range. We rely on
 *  Unicode property classes — any visible non-whitespace token under the
 *  length cap qualifies. */
const REACTION_RE = /^[\p{L}\p{N}\p{Emoji}\p{Extended_Pictographic}_+\-]{1,32}$/u;

export interface ReactionRow {
  message_id: number;
  reactor: string;
  reaction: string;
  created_at: number;
}

export function setReaction(messageId: number, reactor: string, reaction: string): void {
  db().run(
    `INSERT INTO message_reactions (message_id, reactor, reaction, created_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(message_id, reactor) DO UPDATE SET
       reaction = excluded.reaction,
       created_at = excluded.created_at`,
    [messageId, reactor, reaction, _now()],
  );
}

export function clearReaction(messageId: number, reactor: string): boolean {
  const res = db().run(
    "DELETE FROM message_reactions WHERE message_id = ? AND reactor = ?",
    [messageId, reactor],
  );
  return res.changes > 0;
}

export function listReactionsFor(messageId: number): ReactionRow[] {
  return db()
    .query<ReactionRow, [number]>(
      `SELECT message_id, reactor, reaction, created_at
       FROM message_reactions WHERE message_id = ?
       ORDER BY created_at ASC`,
    )
    .all(messageId);
}

/** Render reactions as " · 👍 from A,B · ✓ from C", or empty string. */
export function summariseReactions(messageId: number): string {
  const rows = listReactionsFor(messageId);
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

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }] };
}
function error(s: string) {
  return { content: [{ type: "text" as const, text: s }], isError: true };
}

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
        message_id: z
          .number()
          .int()
          .min(1)
          .describe("The message id (visible as [N] in chat/groupchat/inbox output)."),
        reaction: z
          .string()
          .describe(
            "Short reaction: emoji or word, ≤32 chars, no whitespace. " +
              "Examples: '👍', '✓', 'lgtm', '🎉'. Pass empty to clear.",
          ),
      },
    },
    async ({ message_id, reaction }) => {
      touchInstance(me.pseudonym);
      const msg = getMessage(message_id);
      if (!msg) return error(`Unknown message_id ${message_id}.`);
      // Only chat members can react.
      const members = listChatMembers(msg.chat_id).map((m) => m.pseudonym);
      if (!members.includes(me.pseudonym)) {
        return error(`You're not a member of ${msg.chat_id}; cannot react.`);
      }
      if (reaction.trim().length === 0) {
        const cleared = clearReaction(message_id, me.pseudonym);
        return text(
          cleared
            ? `Cleared your reaction on message ${message_id}.`
            : `No reaction was set on message ${message_id}.`,
        );
      }
      const trimmed = reaction.trim();
      if (!REACTION_RE.test(trimmed)) {
        return error(
          `Reaction '${trimmed}' invalid. Must be 1-${REACTION_MAX_LEN} chars, no whitespace, emoji or short token.`,
        );
      }
      setReaction(message_id, me.pseudonym, trimmed);
      return text(`Reacted to message ${message_id} with '${trimmed}'.`);
    },
  );
}
