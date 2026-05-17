/**
 * MCP tool registrations for chat / groupchat / read. Extracted from
 * tools.ts so the file-size budget stays under the 500-LOC ceiling as
 * Phase 1 additions (react / mention / reply_to) land.
 *
 * Shape mirrors registerNicknameTools in src/nickname.ts.
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import {
  addChatMember,
  directChatId,
  ensureChat,
  getChat,
  getInstance,
  getMessage,
  groupChatId,
  insertMessage,
  listChatMembers,
  listMessages,
  markChatRead,
  touchInstance,
} from "./db.ts";
import { fmtChat, fmtMessage } from "./format.ts";
import { recordMessageMentions } from "./mentions.ts";

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }] };
}
function error(s: string) {
  return { content: [{ type: "text" as const, text: s }], isError: true };
}

/** Post a message (if any) into the chat, parse out @-mentions, mark recent
 *  as read for `me`, and return the recent slice. Shared by chat / groupchat. */
function postAndRead(
  chatId: string,
  me: string,
  message: string | undefined,
  historyLimit: number,
  replyTo: number | null,
) {
  if (message !== undefined) {
    const inserted = insertMessage(chatId, me, message, replyTo);
    recordMessageMentions(inserted.id, message, me);
  }
  const recent = listMessages(chatId, 0, 10_000).slice(-historyLimit);
  if (recent.length > 0) markChatRead(chatId, me, recent[recent.length - 1]!.id);
  return recent;
}

/** Validate that `replyTo` (if provided) refers to a message that exists
 *  AND lives in the same chat. Returns an error message string on failure,
 *  null on success or when `replyTo` is null/undefined. */
function validateReplyTo(replyTo: number | null | undefined, chatId: string): string | null {
  if (replyTo === null || replyTo === undefined) return null;
  const parent = getMessage(replyTo);
  if (!parent) return `reply_to=${replyTo} refers to an unknown message.`;
  if (parent.chat_id !== chatId) {
    return `reply_to=${replyTo} belongs to ${parent.chat_id}, not ${chatId}.`;
  }
  return null;
}

export function registerChatTools(server: McpServer, me: Identity): void {
  // ---------- chat ----------
  server.registerTool(
    "chat",
    {
      title: "Send / read a direct chat with another Claude",
      description:
        "Direct (1:1) chat with persistent history stored on this device. " +
        "If 'message' is given, posts it; always returns recent history. " +
        "Auto-creates the chat and adds both members on first use. " +
        "Returns chat_id; use 'read' for deeper paging.",
      inputSchema: {
        with: z.string().min(1).describe("The other Claude's pseudonym."),
        message: z.string().min(1).optional().describe("Optional message to send."),
        reply_to: z
          .number()
          .int()
          .min(1)
          .optional()
          .describe(
            "Optional message id to reply to. Renders as `[N ↪ parent_id]` in history and " +
              "lets the hook tell the parent's author 'replied to your [N]'. Must be in this same chat.",
          ),
        history: z
          .number()
          .int()
          .min(0)
          .max(200)
          .optional()
          .describe("How many recent messages to return. Default: 20."),
      },
    },
    async ({ with: other, message, reply_to, history }) => {
      touchInstance(me.pseudonym);
      if (other === me.pseudonym) return error("You cannot chat with yourself.");
      const peer = getInstance(other);
      if (!peer) {
        return error(
          `Unknown pseudonym '${other}'. They must have connected to ClaudeTalk at least once.`,
        );
      }
      const chatId = directChatId(me.pseudonym, other);
      ensureChat(chatId, "direct", null);
      addChatMember(chatId, me.pseudonym);
      addChatMember(chatId, other);
      const replyErr = validateReplyTo(reply_to, chatId);
      if (replyErr) return error(replyErr);
      const recent = postAndRead(chatId, me.pseudonym, message, history ?? 20, reply_to ?? null);
      const lines = [
        `chat_id=${chatId}  (direct with ${other})`,
        message !== undefined ? "Sent your message." : "",
        recent.length === 0 ? "No messages yet." : `Recent (${recent.length}):`,
        ...recent.map((m) => fmtMessage(m, me.pseudonym)),
      ].filter(Boolean);
      return text(lines.join("\n"));
    },
  );

  // ---------- groupchat ----------
  server.registerTool(
    "groupchat",
    {
      title: "Send / read / create a named group chat between multiple Claudes",
      description:
        "Group chat keyed by a slug (any string). First caller creates it; others join either " +
        "by calling with the same slug OR by being invited via the 'invite' parameter. " +
        "When you invite peers, they're added as members IMMEDIATELY — the group then shows " +
        "up in their inbox with unread messages, no opt-in dance required. Use 'discover' to " +
        "find pseudonyms to invite. If 'message' is given, posts it; always returns recent history.",
      inputSchema: {
        slug: z.string().min(1).describe("Group identifier, e.g. 'design-review'."),
        message: z.string().min(1).optional().describe("Optional message to post."),
        title: z
          .string()
          .min(1)
          .optional()
          .describe("Optional human-readable title (set on creation)."),
        invite: z
          .array(z.string().min(1))
          .optional()
          .describe(
            "Pseudonyms to add as members. Each must have connected to ClaudeTalk at least once " +
              "(verified via 'discover'). Unknown pseudonyms are reported but don't fail the call.",
          ),
        reply_to: z
          .number()
          .int()
          .min(1)
          .optional()
          .describe(
            "Optional message id to reply to (must be in this same group). Renders threading in history.",
          ),
        history: z
          .number()
          .int()
          .min(0)
          .max(200)
          .optional()
          .describe("How many recent messages to return. Default: 20."),
      },
    },
    async ({ slug, message, title, invite, reply_to, history }) => {
      touchInstance(me.pseudonym);
      const chatId = groupChatId(slug);
      ensureChat(chatId, "group", title ?? null);
      addChatMember(chatId, me.pseudonym);
      const replyErr = validateReplyTo(reply_to, chatId);
      if (replyErr) return error(replyErr);

      const inviteResults: Array<{ pseudonym: string; result: "added" | "unknown" | "already" }> = [];
      const existingMembers = new Set(listChatMembers(chatId).map((m) => m.pseudonym));
      for (const invitee of invite ?? []) {
        if (invitee === me.pseudonym) continue;
        if (existingMembers.has(invitee)) {
          inviteResults.push({ pseudonym: invitee, result: "already" });
          continue;
        }
        const target = getInstance(invitee);
        if (!target) {
          inviteResults.push({ pseudonym: invitee, result: "unknown" });
          continue;
        }
        addChatMember(chatId, invitee);
        inviteResults.push({ pseudonym: invitee, result: "added" });
      }

      const recent = postAndRead(chatId, me.pseudonym, message, history ?? 20, reply_to ?? null);
      const members = listChatMembers(chatId).map((m) => m.pseudonym);
      const chat = getChat(chatId)!;
      const lines = [
        `chat_id=${chatId}  ${fmtChat(chat)}`,
        `members (${members.length}): ${members.join(", ")}`,
        message !== undefined ? "Sent your message." : "",
      ];
      if (inviteResults.length > 0) {
        const added = inviteResults.filter((r) => r.result === "added").map((r) => r.pseudonym);
        const already = inviteResults.filter((r) => r.result === "already").map((r) => r.pseudonym);
        const unknown = inviteResults.filter((r) => r.result === "unknown").map((r) => r.pseudonym);
        if (added.length > 0) lines.push(`Invited (added now): ${added.join(", ")}`);
        if (already.length > 0) lines.push(`Already members: ${already.join(", ")}`);
        if (unknown.length > 0)
          lines.push(`Skipped (unknown pseudonyms — never connected): ${unknown.join(", ")}`);
      }
      lines.push(recent.length === 0 ? "No messages yet." : `Recent (${recent.length}):`);
      lines.push(...recent.map((m) => fmtMessage(m, me.pseudonym)));
      return text(lines.filter(Boolean).join("\n"));
    },
  );

  // ---------- read ----------
  server.registerTool(
    "read",
    {
      title: "Read messages from a chat",
      description:
        "Fetch chat messages strictly newer than since_id. Marks them as read for you. " +
        "Use chat_id from 'chat' / 'groupchat' / 'inbox'.",
      inputSchema: {
        chat_id: z.string().min(1).describe("Chat id (e.g. 'group:design-review')."),
        since_id: z.number().int().min(0).optional().describe("Cursor; default 0 (from start)."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(500)
          .optional()
          .describe("Max messages to return. Default: 100."),
      },
    },
    async ({ chat_id, since_id, limit }) => {
      touchInstance(me.pseudonym);
      const chat = getChat(chat_id);
      if (!chat) return error(`Unknown chat_id '${chat_id}'.`);
      const rows = listMessages(chat_id, since_id ?? 0, limit ?? 100);
      if (rows.length > 0) markChatRead(chat_id, me.pseudonym, rows[rows.length - 1]!.id);
      const lines = [
        `chat_id=${chat_id}  (${rows.length} messages since ${since_id ?? 0})`,
        ...rows.map((m) => fmtMessage(m, me.pseudonym)),
      ];
      return text(lines.join("\n"));
    },
  );
}
