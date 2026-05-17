import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import {
  addChatMember,
  answerAsk,
  directChatId,
  ensureChat,
  getAsk,
  getChat,
  getInstance,
  groupChatId,
  insertAsk,
  insertMessage,
  listChatMembers,
  listInstances,
  listMessages,
  markChatRead,
  touchInstance,
} from "./db.ts";
import {
  fmtChat,
  fmtInstance,
  fmtMessage,
  renderInbox,
} from "./format.ts";
import { displayName, registerNicknameTools } from "./nickname.ts";

const ACTIVE_WINDOW_MS_DEFAULT = 10 * 60 * 1000;
/** Cap on the optional inline-wait in `ask`. Kept short so Claude is never
 *  blocked for long when it explicitly opts into a synchronous answer. */
const ASK_WAIT_MAX_SECONDS = 10;

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }] };
}

function error(s: string) {
  return { content: [{ type: "text" as const, text: s }], isError: true };
}

/** Post a message (if any) into the chat, mark recent as read for `me`, and
 *  return the recent slice. Shared by chat / groupchat to avoid duplication. */
function postAndRead(
  chatId: string,
  me: string,
  message: string | undefined,
  historyLimit: number,
) {
  if (message !== undefined) insertMessage(chatId, me, message);
  const recent = listMessages(chatId, 0, 10_000).slice(-historyLimit);
  if (recent.length > 0) markChatRead(chatId, me, recent[recent.length - 1]!.id);
  return recent;
}


export function registerTools(server: McpServer, me: Identity): void {
  // ---------- whoami ----------
  server.registerTool(
    "whoami",
    {
      title: "Show this instance's ClaudeTalk identity",
      description:
        "Returns the deterministic pseudonym assigned to this folder and the folder path. " +
        "Use this once at session start so you know how others see you.",
      inputSchema: {},
    },
    async () => {
      touchInstance(me.pseudonym);
      return text(
        [
          `You are: ${me.pseudonym}`,
          `Folder:  ${me.path}`,
          "(pseudonym is a deterministic SHA-256 hash of the folder path)",
        ].join("\n"),
      );
    },
  );

  // ---------- discover ----------
  server.registerTool(
    "discover",
    {
      title: "List / search active ClaudeTalk instances",
      description:
        "Lists Claude instances currently connected to ClaudeTalk, with their pseudonyms, " +
        "any nickname you have for them, and the folder they were opened in. " +
        "Optional filters: 'folder_contains' (substring match on the absolute path) and " +
        "'name' (matches either the pseudonym OR your personal nickname for them, case-insensitive substring). " +
        "Use this to find someone to ask or chat with.",
      inputSchema: {
        active_within_minutes: z
          .number()
          .int()
          .min(1)
          .max(7 * 24 * 60)
          .optional()
          .describe("Only include instances last seen within this many minutes. Default: 10."),
        folder_contains: z
          .string()
          .min(1)
          .optional()
          .describe("Case-insensitive substring filter on the instance folder path."),
        name: z
          .string()
          .min(1)
          .optional()
          .describe(
            "Case-insensitive substring matching either the pseudonym or your personal nickname for the instance.",
          ),
      },
    },
    async ({ active_within_minutes, folder_contains, name }) => {
      touchInstance(me.pseudonym);
      const windowMs =
        (active_within_minutes ?? Math.floor(ACTIVE_WINDOW_MS_DEFAULT / 60000)) * 60_000;
      let rows = listInstances(windowMs);
      if (folder_contains) {
        const needle = folder_contains.toLowerCase();
        rows = rows.filter((i) => i.path.toLowerCase().includes(needle));
      }
      if (name) {
        const needle = name.toLowerCase();
        rows = rows.filter((i) => {
          const d = displayName(me.pseudonym, i.pseudonym, null).toLowerCase();
          return (
            i.pseudonym.toLowerCase().includes(needle) || d.includes(needle)
          );
        });
      }
      if (rows.length === 0) {
        const filters: string[] = [];
        if (folder_contains) filters.push(`folder_contains=${folder_contains}`);
        if (name) filters.push(`name=${name}`);
        return text(
          `No active ClaudeTalk instances found${filters.length ? ` (filters: ${filters.join(", ")})` : ""}.`,
        );
      }
      const lines = [
        `Active ClaudeTalk instances (${rows.length}):`,
        ...rows.map((i) => fmtInstance(i, me.pseudonym)),
        "",
        `(You are ${me.pseudonym}.)`,
      ];
      return text(lines.join("\n"));
    },
  );

  // ---------- ask ----------
  server.registerTool(
    "ask",
    {
      title: "Ask another Claude a one-off question",
      description:
        "Send a single question to another Claude instance identified by its pseudonym. " +
        "Optionally block for up to wait_seconds to receive the answer; if the peer is offline or slow, " +
        "the ask is queued and you can poll later via 'inbox'. Returns ask_id for follow-up.",
      inputSchema: {
        to: z.string().min(1).describe("Recipient pseudonym (e.g. 'SwiftFox-a3f')."),
        question: z.string().min(1).describe("The question text."),
        wait_seconds: z
          .number()
          .int()
          .min(0)
          .max(ASK_WAIT_MAX_SECONDS)
          .optional()
          .describe(
            "If > 0, block up to this many seconds waiting for the answer. " +
              `Capped at ${ASK_WAIT_MAX_SECONDS}. Default: 0 (return immediately).`,
          ),
      },
    },
    async ({ to, question, wait_seconds }) => {
      touchInstance(me.pseudonym);
      if (to === me.pseudonym) return error("You cannot ask yourself.");
      const target = getInstance(to);
      if (!target) {
        return error(
          `Unknown pseudonym '${to}'. Use 'discover' to see who's online. ` +
            "Note: the recipient must have connected to ClaudeTalk at least once.",
        );
      }
      const ask = insertAsk(me.pseudonym, to, question);
      const onlineSoftLimitMs = 5 * 60_000;
      const peerOnline = Date.now() - target.last_seen < onlineSoftLimitMs;
      const wait = Math.min(wait_seconds ?? 0, ASK_WAIT_MAX_SECONDS);
      if (wait === 0) {
        return text(
          [
            `ask_id=${ask.id}  sent to ${to}.`,
            peerOnline
              ? `Recipient appears online (last seen ${Math.floor((Date.now() - target.last_seen) / 1000)}s ago).`
              : `Recipient appears offline (last seen ${Math.floor((Date.now() - target.last_seen) / 1000)}s ago); they'll see it when they reconnect.`,
            `Poll 'inbox' later (or call ask again with wait_seconds>0) to collect the answer.`,
          ].join("\n"),
        );
      }
      const deadline = Date.now() + wait * 1000;
      while (Date.now() < deadline) {
        const cur = getAsk(ask.id);
        if (cur && cur.answered_at !== null && cur.answer_body !== null) {
          return text(
            [
              `ask_id=${ask.id}  ANSWERED by ${to}:`,
              "",
              cur.answer_body,
            ].join("\n"),
          );
        }
        await Bun.sleep(500);
      }
      return text(
        [
          `ask_id=${ask.id}  still pending after ${wait}s.`,
          `Peer last seen ${Math.floor((Date.now() - target.last_seen) / 1000)}s ago.`,
          `Call 'inbox' later to collect the answer when it arrives.`,
        ].join("\n"),
      );
    },
  );

  // ---------- answer ----------
  server.registerTool(
    "answer",
    {
      title: "Answer a pending ask addressed to you",
      description:
        "Record your answer to a one-off ask that another Claude sent you. " +
        "Find ask_id via 'inbox'. Once answered, the asker can collect the reply.",
      inputSchema: {
        ask_id: z.number().int().min(1).describe("The ask_id returned by 'inbox'."),
        answer: z.string().min(1).describe("Your answer text."),
      },
    },
    async ({ ask_id, answer }) => {
      touchInstance(me.pseudonym);
      const before = getAsk(ask_id);
      if (!before) return error(`No ask with id=${ask_id}.`);
      if (before.to_pseudonym !== me.pseudonym) {
        return error(
          `ask_id=${ask_id} was addressed to ${before.to_pseudonym}, not you (${me.pseudonym}).`,
        );
      }
      if (before.answered_at !== null) {
        return error(
          `ask_id=${ask_id} was already answered ${Math.floor((Date.now() - before.answered_at) / 1000)}s ago.`,
        );
      }
      const after = answerAsk(ask_id, me.pseudonym, answer);
      if (!after) return error(`Could not record answer for ask_id=${ask_id}.`);
      return text(`Recorded answer for ask_id=${ask_id}. ${before.from_pseudonym} will receive it.`);
    },
  );

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
        history: z
          .number()
          .int()
          .min(0)
          .max(200)
          .optional()
          .describe("How many recent messages to return. Default: 20."),
      },
    },
    async ({ with: other, message, history }) => {
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
      const recent = postAndRead(chatId, me.pseudonym, message, history ?? 20);
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
        history: z
          .number()
          .int()
          .min(0)
          .max(200)
          .optional()
          .describe("How many recent messages to return. Default: 20."),
      },
    },
    async ({ slug, message, title, invite, history }) => {
      touchInstance(me.pseudonym);
      const chatId = groupChatId(slug);
      ensureChat(chatId, "group", title ?? null);
      addChatMember(chatId, me.pseudonym);

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

      const recent = postAndRead(chatId, me.pseudonym, message, history ?? 20);
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

  // ---------- inbox ----------
  server.registerTool(
    "inbox",
    {
      title: "Check your ClaudeTalk inbox",
      description:
        "Returns pending asks you should answer, unread chat messages across all chats, and " +
        "any answers to asks you sent recently. Call this at session start and whenever a hook " +
        "tells you there are new messages.",
      inputSchema: {
        include_my_answered_asks_since_id: z
          .number()
          .int()
          .min(0)
          .optional()
          .describe(
            "Also list asks you sent that have been answered, with id > this value. Default 0.",
          ),
      },
    },
    async ({ include_my_answered_asks_since_id }) => {
      touchInstance(me.pseudonym);
      return text(
        renderInbox(me.pseudonym, include_my_answered_asks_since_id ?? 0),
      );
    },
  );

  // (wait_for_messages was removed: it blocked the JSON-RPC channel for up
  //  to its timeout, making Claude appear stuck. The hook stack now fires
  //  check-inbox.ts on every Claude Code event — SessionStart, UserPromptSubmit,
  //  PostToolUse, PostToolBatch, SubagentStop, Stop — so new activity is
  //  surfaced organically without blocking.)

  // ---------- nickname_* tools (moved to src/nickname.ts) ----------
  registerNicknameTools(server, me);
}
