#!/usr/bin/env bun
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { pseudonymFor, pseudonymForKey } from "./pseudonym.ts";
import { resolveProjectDir, ensureRootDir } from "./paths.ts";
import { getOrCreateMachineId } from "./machine-id.ts";
import { getKeyPairForFolder } from "./keys.ts";
import { isNetworkConfigured } from "./network-config.ts";
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
import { flushNow, instrumentServer, instrumentTransport, stopAuditFlusher } from "./audit-log.ts";

// stdio MCP: all logging MUST go to stderr.
const log = (...args: unknown[]) => console.error("[claudetalk]", ...args);

/** Phase K3 transition: if this machine + folder used to have a path-
 *  derived pseudonym (pre-v0.6.1) and now has a key-derived one, rewrite
 *  every row referencing the old name to the new name. Idempotent and
 *  safe to run on every startup — the WHERE clause is `pseudonym = old`
 *  so once the migration ran (and the old pseudonym no longer exists in
 *  the DB) subsequent runs are no-ops.
 *
 *  Tables touched (every column that ever stores a pseudonym):
 *    instances.pseudonym
 *    chat_members.pseudonym
 *    messages.from_pseudonym
 *    message_reactions.reactor
 *    message_mentions.target
 *    asks.from_pseudonym, asks.to_pseudonym
 *    personal_nicknames.viewer, personal_nicknames.target
 *    group_nickname_votes.target, group_nickname_votes.voter
 *    instance_status.pseudonym
 *    chat_preferences.viewer
 */
function migrateLegacyPseudonym(oldName: string, newName: string): void {
  const d = db();
  try {
    d.transaction(() => {
      // If the new pseudonym already exists in instances, the old one is
      // stale (from a stale earlier session) — drop it to avoid PK
      // collision on the UPDATE.
      const collides = d
        .query<{ c: number }, [string]>("SELECT COUNT(*) AS c FROM instances WHERE pseudonym = ?")
        .get(newName);
      if ((collides?.c ?? 0) > 0) {
        d.run("DELETE FROM instances WHERE pseudonym = ?", [oldName]);
      } else {
        d.run("UPDATE instances SET pseudonym = ? WHERE pseudonym = ?", [newName, oldName]);
      }
      d.run("UPDATE chat_members SET pseudonym = ? WHERE pseudonym = ?", [newName, oldName]);
      d.run("UPDATE messages SET from_pseudonym = ? WHERE from_pseudonym = ?", [newName, oldName]);
      d.run("UPDATE message_reactions SET reactor = ? WHERE reactor = ?", [newName, oldName]);
      d.run("UPDATE message_mentions SET target = ? WHERE target = ?", [newName, oldName]);
      d.run("UPDATE asks SET from_pseudonym = ? WHERE from_pseudonym = ?", [newName, oldName]);
      d.run("UPDATE asks SET to_pseudonym = ? WHERE to_pseudonym = ?", [newName, oldName]);
      d.run("UPDATE personal_nicknames SET viewer = ? WHERE viewer = ?", [newName, oldName]);
      d.run("UPDATE personal_nicknames SET target = ? WHERE target = ?", [newName, oldName]);
      d.run("UPDATE group_nickname_votes SET target = ? WHERE target = ?", [newName, oldName]);
      d.run("UPDATE group_nickname_votes SET voter = ? WHERE voter = ?", [newName, oldName]);
      d.run("UPDATE instance_status SET pseudonym = ? WHERE pseudonym = ?", [newName, oldName]);
      d.run("UPDATE chat_preferences SET viewer = ? WHERE viewer = ?", [newName, oldName]);
    }).immediate();
    log(`migrated legacy pseudonym '${oldName}' → '${newName}' (Phase K3 transition)`);
  } catch (e) {
    log("legacy-pseudonym migration failed (continuing under new identity):", e);
  }
}

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
      // mode 0o600: stack traces can leak in-flight payload fragments
      // and source paths; keep the crash log owner-only.
      require("node:fs").appendFileSync(path, line, { mode: 0o600 });
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
// Activity-summary poll for the logging-notification path. Claude Code
// doesn't surface MCP logging messages, so this loop is mostly
// no-op overhead. Raised from 2 s → 10 s; if/when a non-Claude-Code MCP
// client subscribes, a few-second delay is fine. (Perf audit M6.)
const POLL_MS = 10_000;
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
  // Bootstrap-only pseudonym so crash handlers + db init can log
  // something sensible. Replaced below with the key-derived identity
  // once the Ed25519 keypair is ready.
  const bootstrap = pseudonymFor(projectDir);
  installCrashHandlers(bootstrap.pseudonym);
  db(); // open + migrate
  const machineId = getOrCreateMachineId();
  // Phase K0: derive the deterministic Ed25519 keypair for this folder.
  // Same folder + same machine ⇒ same keypair, every run.
  const keyPair = await getKeyPairForFolder(projectDir);
  // Phase K3 (v0.6.1+): the pseudonym IS the key fingerprint. Forgery
  // requires compromising the private key, not just guessing the folder
  // path. Pre-v0.6.1 sessions had a path-derived pseudonym for this
  // (machine, folder); migrate any DB rows referencing the old name to
  // the new one so existing chats / cursors keep working.
  const me = pseudonymForKey(keyPair.publicKey, projectDir);
  me.keyPair = keyPair;
  if (bootstrap.pseudonym !== me.pseudonym) {
    migrateLegacyPseudonym(bootstrap.pseudonym, me.pseudonym);
  }
  upsertInstance(
    me.pseudonym,
    me.path,
    process.pid,
    machineId,
    me.keyPair.publicKey,
  );
  log(
    `identity: ${me.pseudonym}  folder=${me.path}  ` +
      `machine=${machineId.slice(0, 8)}  pubkey=${me.keyPair.publicKey.slice(0, 8)}…`,
  );
  if (isNetworkConfigured()) {
    log("network: ~/.claudetalk/network.json present (Phase N1 relay not yet wired)");
  }

  const server = new McpServer(
    { name: "claudetalk", version: "0.6.1" },
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
  heartbeat.unref?.();

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
  poll.unref?.();

  // Phase 0 — channel push. Track the max message seq we've already pushed
  // per chat so we only push the delta. When Claude Code loads this server
  // as a channel, these notifications arrive as <channel source="claudetalk"
  // chat_id="..." sender="..." message_id="<uuid>" seq="N"> events in the
  // conversation mid-turn, with no hook involved.
  //
  // v0.5.0+ note: cursor is the integer `seq` (chronological), NOT the
  // TEXT UUID `id` (which sorts alphabetically — wrong). The push payload
  // still carries the UUID `id` as `message_id` because that's the
  // cross-machine identity; `seq` is added as a sibling for the receiving
  // Claude's "[N]" rendering.
  const channelCursors = new Map<string, number>();
  const channelPoll = setInterval(async () => {
    try {
      const myChats = listChatsFor(me.pseudonym);
      for (const { chat } of myChats) {
        const lastSeen = channelCursors.get(chat.id) ?? 0;
        // Initialize the cursor to "current max seq" on first observation
        // so we don't replay the whole history on startup.
        if (lastSeen === 0) {
          const maxRow = db()
            .query<{ m: number | null }, [string]>(
              "SELECT MAX(seq) AS m FROM messages WHERE chat_id = ?",
            )
            .get(chat.id);
          channelCursors.set(chat.id, maxRow?.m ?? 0);
          continue;
        }
        const newRows = db()
          .query<
            {
              id: string;
              seq: number;
              from_pseudonym: string;
              body: string;
              created_at: number;
              signature: string | null;
            },
            [string, number, string]
          >(
            `SELECT id, seq, from_pseudonym, body, created_at, signature
             FROM messages
             WHERE chat_id = ? AND seq > ? AND from_pseudonym != ?
             ORDER BY seq ASC LIMIT 20`,
          )
          .all(chat.id, lastSeen, me.pseudonym);
        for (const row of newRows) {
          try {
            // Suppress the row's members lookup for direct chats (the other
            // member IS the sender). For groups, members list is useful.
            const isGroup = chat.kind === "group";
            // K4 (v0.6.1+): include the Ed25519 signature in the push so
            // a receiving Claude (when the relay forwards a frame from
            // another machine) can verify against the author's published
            // pubkey. Pre-K1 rows have null signature; we pass through as
            // an empty string the receiver can recognise as "legacy
            // unsigned, do not trust the body as authenticated".
            await server.server.notification({
              method: "notifications/claude/channel",
              params: {
                content: `${row.from_pseudonym}: ${row.body}`,
                meta: {
                  chat_id: chat.id,
                  sender: row.from_pseudonym,
                  message_id: row.id,
                  seq: String(row.seq),
                  ts: String(row.created_at),
                  sig: row.signature ?? "",
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
            channelCursors.set(chat.id, row.seq);
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
  channelPoll.unref?.();

  const shutdown = (sig: string) => {
    log(`shutting down (${sig})`);
    clearInterval(heartbeat);
    clearInterval(poll);
    clearInterval(channelPoll);
    // Stop the audit log's own 200ms flusher BEFORE the final flush so it
    // doesn't fire against a closed DB handle a tick after we exit.
    stopAuditFlusher();
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
