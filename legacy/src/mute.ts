/**
 * Per-viewer per-chat mute. When set, the hook suppresses the chat from
 * notification deltas — you'll still see it via `inbox` if you check, but
 * the hook won't surface a header for new content in that chat.
 *
 * Plumbed through `notificationDeltaFor` (filtered after the cursor check).
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import { dynamicIdentity } from "./identity-context.ts";
import { _now, db, getChat, touchInstance } from "./db.ts";
import { ErrorCode, toolError, toolText } from "./errors.ts";

export function isChatMutedFor(viewer: string, chatId: string): boolean {
  const row = db()
    .query<{ muted: number }, [string, string]>(
      "SELECT muted FROM chat_preferences WHERE viewer = ? AND chat_id = ?",
    )
    .get(viewer, chatId);
  return (row?.muted ?? 0) === 1;
}

export function setChatMuted(viewer: string, chatId: string, muted: boolean): void {
  db().run(
    `INSERT INTO chat_preferences (viewer, chat_id, muted, updated_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(viewer, chat_id) DO UPDATE SET
       muted = excluded.muted, updated_at = excluded.updated_at`,
    [viewer, chatId, muted ? 1 : 0, _now()],
  );
}

export function listMutedChatsFor(viewer: string): string[] {
  return db()
    .query<{ chat_id: string }, [string]>(
      "SELECT chat_id FROM chat_preferences WHERE viewer = ? AND muted = 1 ORDER BY chat_id",
    )
    .all(viewer)
    .map((r) => r.chat_id);
}

// text/error helpers come from src/errors.ts (Phase 5.4 — codes).
const text = (s: string) => toolText(s);
const error = (s: string, code: ErrorCode = ErrorCode.UNSPECIFIED) => toolError(s, code);

export function registerMuteTools(server: McpServer, staticMe: Identity): void {
  const me = dynamicIdentity(staticMe);
  server.registerTool(
    "mute",
    {
      title: "Mute / unmute a chat's hook notifications",
      description:
        "When muted, the hook suppresses notifications for new content in this chat. The chat " +
        "still appears in `inbox` when you check explicitly. Mute is per-viewer; muting affects " +
        "only YOUR hook stream, never other members'.",
      inputSchema: {
        chat_id: z.string().min(1).describe("Chat id (e.g. 'group:design' or 'direct:A|B')."),
        muted: z
          .boolean()
          .optional()
          .describe("true = mute, false = unmute. Default: true (toggle to muted)."),
      },
    },
    async ({ chat_id, muted }) => {
      touchInstance(me.pseudonym);
      if (!getChat(chat_id)) return error(`Unknown chat_id '${chat_id}'.`);
      const m = muted ?? true;
      setChatMuted(me.pseudonym, chat_id, m);
      return text(m ? `Muted ${chat_id}.` : `Unmuted ${chat_id}.`);
    },
  );
}
