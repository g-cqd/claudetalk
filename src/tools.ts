import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import {
  answerAsk,
  getAsk,
  getInstance,
  insertAsk,
  listInstances,
  touchInstance,
} from "./db.ts";
import { fmtInstance, renderInbox } from "./format.ts";
import { displayName, registerNicknameTools } from "./nickname.ts";
import { registerChatTools } from "./chat-tools.ts";
import { registerReactionTools } from "./reactions.ts";
import { fmtStatus, getStatus, registerStatusTools } from "./status.ts";
import { registerSearchTool } from "./search.ts";
import { registerMuteTools } from "./mute.ts";
import { ErrorCode, toolError, toolText } from "./errors.ts";
import { resetNotificationCursors } from "./notifications.ts";
import { getChat } from "./db.ts";

const ACTIVE_WINDOW_MS_DEFAULT = 10 * 60 * 1000;
/** Cap on the optional inline-wait in `ask`. Kept short so Claude is never
 *  blocked for long when it explicitly opts into a synchronous answer. */
const ASK_WAIT_MAX_SECONDS = 10;

// text/error helpers come from src/errors.ts (Phase 5.4 — codes).
const text = (s: string) => toolText(s);
const error = (s: string, code: ErrorCode = ErrorCode.UNSPECIFIED) => toolError(s, code);



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
        ...rows.map((i) => {
          const base = fmtInstance(i, me.pseudonym);
          const s = fmtStatus(getStatus(i.pseudonym));
          return s ? `${base}\n  status: ${s}` : base;
        }),
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

  // chat / groupchat / read are registered separately in registerChatTools
  registerChatTools(server, me);
  // react lives in src/reactions.ts (one tool, table + helpers co-located).
  registerReactionTools(server, me);

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

  // ---------- wait_for_messages ----------
  // Compatibility stub: this used to long-poll for up to 25–60 seconds,
  // which held the stdio JSON-RPC channel hostage and made the server
  // look hung to Claude. We removed the blocking but kept the tool so
  // older callers (and other Claude builds that may still have it in
  // their tool list) don't error out and trigger a transport disconnect.
  // Returns immediately with the current inbox snapshot.
  server.registerTool(
    "wait_for_messages",
    {
      title: "Return the current inbox snapshot (non-blocking)",
      description:
        "Compatibility stub. Returns immediately with whatever 'inbox' would return; " +
        "ignores any timeout/wait parameters. The hook stack (SessionStart, " +
        "UserPromptSubmit, PostToolUse, PostToolBatch, SubagentStop, Stop) and the " +
        "claude/channel push (when loaded as a channel) cover the real-time path. " +
        "Use 'inbox' directly going forward.",
      inputSchema: {
        timeout_seconds: z
          .number()
          .int()
          .min(0)
          .max(60)
          .optional()
          .describe("Ignored. Kept for schema compatibility with the old long-poll tool."),
      },
    },
    async () => {
      touchInstance(me.pseudonym);
      return text(renderInbox(me.pseudonym, 0));
    },
  );

  // Phase 2 tools — each module owns its tables + helpers + registration.
  registerStatusTools(server, me);
  registerSearchTool(server, me);
  registerMuteTools(server, me);

  // Phase 5.3 — notifications_reset
  server.registerTool(
    "notifications_reset",
    {
      title: "Rewind your notification cursors so the hook re-surfaces content",
      description:
        "Resets your `last_notified_message_id` (for the given chat, or every chat you're in) " +
        "AND your `last_notified_ask_id` (when no chat_id is given). The hook will then re-emit a " +
        "header on its next fire if there are messages from others past the (now-zero) cursor. " +
        "Useful when you accidentally advanced the cursor past content you wanted to read.",
      inputSchema: {
        chat_id: z
          .string()
          .min(1)
          .optional()
          .describe("Optional chat id (e.g. 'group:design'). Omit to reset ALL your cursors."),
      },
    },
    async ({ chat_id }) => {
      touchInstance(me.pseudonym);
      if (chat_id !== undefined) {
        if (!getChat(chat_id)) return error(`Unknown chat_id '${chat_id}'.`, ErrorCode.UNKNOWN_CHAT);
        const n = resetNotificationCursors(me.pseudonym, chat_id);
        return text(
          n > 0
            ? `Reset notification cursor for ${chat_id}.`
            : `No cursor to reset for ${chat_id} (already at 0 or you're not a member).`,
        );
      }
      const n = resetNotificationCursors(me.pseudonym, null);
      return text(
        `Reset ${n} notification cursor(s). The hook's next fire will re-surface unread content.`,
      );
    },
  );

  // ---------- nickname_* tools (moved to src/nickname.ts) ----------
  registerNicknameTools(server, me);
}
