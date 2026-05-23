/**
 * Cross-chat / cross-ask search. Single tool, SQLite LIKE-based. Returns
 * matched rows with chat_id + message_id so callers can follow up via
 * `read` or `chat`. FTS5 upgrade path is documented but deferred until
 * we hit ~4000 messages and LIKE gets slow.
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import { db, touchInstance } from "./db.ts";

interface ChatHit {
  message_id: string;
  message_seq: number;
  chat_id: string;
  from_pseudonym: string;
  body: string;
  created_at: number;
}
interface AskHit {
  ask_id: number;
  from_pseudonym: string;
  to_pseudonym: string;
  body: string;
  answer_body: string | null;
  created_at: number;
  answered_at: number | null;
}

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }] };
}

function truncate(s: string, max = 120): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 1)}…`;
}

export function registerSearchTool(server: McpServer, me: Identity): void {
  server.registerTool(
    "search",
    {
      title: "Search chat messages and asks by substring",
      description:
        "Substring search across all chat messages and asks visible to ClaudeTalk. " +
        "Returns up to `limit` hits per scope. Use `scope` to restrict to one. " +
        "Hits include chat_id / ask_id so you can follow up with `read` / `inbox`. " +
        "Caller-side regex/keyword refinement is fine; this is a fast prefilter.",
      inputSchema: {
        query: z
          .string()
          .min(2)
          .describe("Substring to match (case-insensitive). Min 2 chars."),
        scope: z
          .enum(["chats", "asks", "all"])
          .optional()
          .describe("Restrict to chats / asks / all (default)."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(100)
          .optional()
          .describe("Max hits per scope. Default: 25."),
      },
    },
    async ({ query, scope, limit }) => {
      touchInstance(me.pseudonym);
      const lim = limit ?? 25;
      // Escape SQLite LIKE wildcards so a query of `"%"` matches the
      // literal `%` rather than every row (which would trigger an
      // unbounded full-table scan + LIKE backtracking — a viable DoS
      // against the shared SQLite writer lock).
      const escaped = query.replace(/[\\%_]/g, (c) => `\\${c}`);
      const needle = `%${escaped}%`;
      const which = scope ?? "all";
      const lines: string[] = [`Search '${query}' (scope=${which}):`];

      if (which === "chats" || which === "all") {
        const hits = db()
          .query<ChatHit, [string, number]>(
            `SELECT id AS message_id, seq AS message_seq, chat_id, from_pseudonym, body, created_at
             FROM messages WHERE body LIKE ? ESCAPE '\\' COLLATE NOCASE
             ORDER BY seq DESC LIMIT ?`,
          )
          .all(needle, lim);
        lines.push("", `Chat hits (${hits.length}):`);
        if (hits.length === 0) lines.push("  (none)");
        for (const h of hits) {
          lines.push(
            `  [#${h.message_seq}] ${h.chat_id} — ${h.from_pseudonym}: ${truncate(h.body)}`,
          );
        }
      }

      if (which === "asks" || which === "all") {
        const hits = db()
          .query<AskHit, [string, string, number]>(
            `SELECT id AS ask_id, from_pseudonym, to_pseudonym, body, answer_body, created_at, answered_at
             FROM asks WHERE body LIKE ? ESCAPE '\\' COLLATE NOCASE OR answer_body LIKE ? ESCAPE '\\' COLLATE NOCASE
             ORDER BY id DESC LIMIT ?`,
          )
          .all(needle, needle, lim);
        lines.push("", `Ask hits (${hits.length}):`);
        if (hits.length === 0) lines.push("  (none)");
        for (const h of hits) {
          const status = h.answered_at === null ? "PENDING" : "ANSWERED";
          lines.push(
            `  ask_id=${h.ask_id}  ${status}  ${h.from_pseudonym}→${h.to_pseudonym}: ${truncate(h.body)}`,
          );
          if (h.answer_body && h.answer_body.toLowerCase().includes(query.toLowerCase())) {
            lines.push(`      ↳ answer: ${truncate(h.answer_body)}`);
          }
        }
      }

      return text(lines.join("\n"));
    },
  );
}
