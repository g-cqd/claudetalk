#!/usr/bin/env bun
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { pseudonymFor } from "./pseudonym.ts";
import { resolveProjectDir, ensureRootDir } from "./paths.ts";
import {
  db,
  listChatMembers,
  listChatsFor,
  touchInstance,
  unreadSummary,
  upsertInstance,
} from "./db.ts";
import { registerTools } from "./tools.ts";
import { fmtUnread } from "./format.ts";
import { flushNow, instrumentServer, instrumentTransport } from "./audit-log.ts";

// stdio MCP: all logging MUST go to stderr.
const log = (...args: unknown[]) => console.error("[claudetalk]", ...args);

// Crash forensics: when the server dies unexpectedly, append a stack trace
// to ~/.claudetalk/crash.log BEFORE the process exits. Claude Code does not
// auto-reconnect stdio MCP servers, so a silent death stalls the asking
// Claude indefinitely — this log gives us the smoking gun for next time.
function installCrashHandlers(pseudonym: string): void {
  const writeCrash = (kind: string, err: unknown) => {
    try {
      const home = process.env.CLAUDETALK_HOME ?? `${process.env.HOME}/.claudetalk`;
      const path = `${home}/crash.log`;
      const stack =
        err instanceof Error ? `${err.message}\n${err.stack ?? ""}` : String(err);
      const line = `${new Date().toISOString()}  ${pseudonym}  pid=${process.pid}  ${kind}\n${stack}\n---\n`;
      require("node:fs").appendFileSync(path, line, { mode: 0o644 });
    } catch {
      // We're dying anyway; best-effort.
    }
  };
  process.on("uncaughtException", (err) => {
    writeCrash("uncaughtException", err);
    log("uncaughtException — exiting", err);
    process.exit(1);
  });
  process.on("unhandledRejection", (err) => {
    writeCrash("unhandledRejection", err);
    log("unhandledRejection — exiting", err);
    process.exit(1);
  });
}

const HEARTBEAT_MS = 30_000;
const POLL_MS = 2_000;
/** Phase 0 (channel mode): poll for new chat messages addressed at us and
 *  push them through the channel API. Tight loop because the whole point is
 *  real-time delivery; cost is one MAX(id) per chat membership per tick. */
const CHANNEL_POLL_MS = 1_000;

const INSTRUCTIONS = `\
ClaudeTalk lets multiple Claude Code instances talk to each other across folders.
You have a stable pseudonym derived from your folder path (call 'whoami' to see it).

Common flows:
  - 'discover' to find other instances by pseudonym
  - 'ask' to send a one-shot question (optionally block with wait_seconds)
  - 'chat' for direct 1:1 conversations, 'groupchat' for named multi-party rooms
  - 'inbox' to check pending asks and unread chat messages
  - 'answer' to reply to a pending ask addressed to you
  - 'wait_for_messages' to long-poll when idle

You SHOULD check 'inbox' once at the start of a session, and any time a system
reminder tells you new ClaudeTalk messages have arrived. Answer pending asks
addressed to you promptly via 'answer'. Reply to direct chat messages from
peers when relevant via 'chat'.`;

async function main(): Promise<void> {
  ensureRootDir();
  const projectDir = resolveProjectDir();
  const me = pseudonymFor(projectDir);
  installCrashHandlers(me.pseudonym);
  db(); // open + migrate
  upsertInstance(me.pseudonym, me.path, process.pid);
  log(`identity: ${me.pseudonym}  folder=${me.path}`);

  const server = new McpServer(
    { name: "claudetalk", version: "0.4.3" },
    {
      capabilities: {
        tools: {},
        logging: {},
        // Phase 0: opt into the claude/channel capability so sessions
        // launched with `--channels file:/path/to/claudetalk` get real-time
        // push of new messages instead of waiting for the next hook fire.
        // When NOT loaded as a channel, the notifications are silently
        // dropped by Claude Code — no-op cost.
        experimental: { "claude/channel": {} },
      },
      instructions: INSTRUCTIONS,
    },
  );
  instrumentServer(server, me.pseudonym);
  registerTools(server, me);

  // Heartbeat keeps us "online" for discover/peer-online checks.
  const heartbeat = setInterval(() => {
    try {
      touchInstance(me.pseudonym);
    } catch (e) {
      log("heartbeat failed", e);
    }
  }, HEARTBEAT_MS);

  // Background poll: when new activity arrives, emit a logging notification.
  // Claude Code itself doesn't surface logging notifications — the hook is
  // the reliable nudge channel — but other MCP clients DO render them, so
  // we still emit. Use the unread-summary timestamp to dedup.
  let lastNotifiedAt = 0;
  const poll = setInterval(async () => {
    try {
      const u = unreadSummary(me.pseudonym);
      const hasNew = u.pendingAsks.length > 0 || u.unreadChats.length > 0;
      if (!hasNew) return;
      const newest = Math.max(
        ...u.pendingAsks.map((a) => a.created_at),
        ...u.unreadChats.map((c) => c.latest?.created_at ?? 0),
        0,
      );
      if (newest <= lastNotifiedAt) return;
      lastNotifiedAt = newest;
      await server.sendLoggingMessage({
        level: "info",
        logger: "claudetalk",
        data: { kind: "activity", summary: fmtUnread(u) },
      });
    } catch {
      // sendLoggingMessage throws if not connected yet; ignore.
    }
  }, POLL_MS);

  // Phase 0 — channel push. Track the max message id we've already pushed
  // per chat so we only push the delta. When Claude Code loads this server
  // as a channel, these notifications arrive as <channel source="claudetalk"
  // chat_id="..." sender="..." message_id="N"> events in the conversation
  // mid-turn, with no hook involved.
  const channelCursors = new Map<string, number>();
  const channelPoll = setInterval(async () => {
    try {
      const myChats = listChatsFor(me.pseudonym);
      for (const { chat } of myChats) {
        const lastSeen = channelCursors.get(chat.id) ?? 0;
        // Initialize the cursor to "current max" on first observation so
        // we don't replay the whole history on startup.
        if (lastSeen === 0) {
          const maxRow = db()
            .query<{ m: number | null }, [string]>(
              "SELECT MAX(id) AS m FROM messages WHERE chat_id = ?",
            )
            .get(chat.id);
          channelCursors.set(chat.id, maxRow?.m ?? 0);
          continue;
        }
        const newRows = db()
          .query<
            { id: number; from_pseudonym: string; body: string; created_at: number },
            [string, number, string]
          >(
            `SELECT id, from_pseudonym, body, created_at
             FROM messages
             WHERE chat_id = ? AND id > ? AND from_pseudonym != ?
             ORDER BY id ASC LIMIT 20`,
          )
          .all(chat.id, lastSeen, me.pseudonym);
        for (const row of newRows) {
          try {
            // Suppress the row's members lookup for direct chats (the other
            // member IS the sender). For groups, members list is useful.
            const isGroup = chat.kind === "group";
            await server.server.notification({
              method: "notifications/claude/channel",
              params: {
                content: `${row.from_pseudonym}: ${row.body}`,
                meta: {
                  chat_id: chat.id,
                  sender: row.from_pseudonym,
                  message_id: String(row.id),
                  ts: String(row.created_at),
                  kind: isGroup ? "group" : "direct",
                  ...(isGroup && chat.title ? { title: chat.title } : {}),
                  ...(isGroup
                    ? {
                        members: listChatMembers(chat.id)
                          .map((m) => m.pseudonym)
                          .join(","),
                      }
                    : {}),
                },
              },
            });
            channelCursors.set(chat.id, row.id);
          } catch {
            // Either we're not loaded as a channel (silently dropped), or
            // the transport is closed. Move on.
          }
        }
      }
    } catch {
      // DB unavailable or other transient — try again next tick.
    }
  }, CHANNEL_POLL_MS);

  const shutdown = (sig: string) => {
    log(`shutting down (${sig})`);
    clearInterval(heartbeat);
    clearInterval(poll);
    clearInterval(channelPoll);
    try {
      flushNow();
    } catch {}
    try {
      server.close();
    } catch {}
    process.exit(0);
  };
  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));

  const transport = new StdioServerTransport();
  await server.connect(transport);
  // Wrap AFTER connect so we replace the Protocol-assigned onmessage and
  // the transport's send method.
  instrumentTransport(transport, me.pseudonym);
  log("ready on stdio");
}

main().catch((err) => {
  console.error("[claudetalk] fatal:", err);
  process.exit(1);
});
