#!/usr/bin/env bun
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { pseudonymFor } from "./pseudonym.ts";
import { resolveProjectDir, ensureRootDir } from "./paths.ts";
import { db, touchInstance, unreadSummary, upsertInstance } from "./db.ts";
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
    { name: "claudetalk", version: "0.1.0" },
    {
      capabilities: { tools: {}, logging: {} },
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
  // (Claude Code may not surface this; install hooks/check-inbox.ts for guaranteed nudging.)
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

  const shutdown = (sig: string) => {
    log(`shutting down (${sig})`);
    clearInterval(heartbeat);
    clearInterval(poll);
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
