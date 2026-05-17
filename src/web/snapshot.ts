/**
 * Pure snapshot builder for the web dashboard. Reads the shared SQLite store
 * and returns a JSON-serialisable view of "what's interesting right now".
 *
 * Kept pure (no IO beyond DB reads, no Bun.serve dependencies) so tests can
 * exercise it without spinning a server.
 */
import {
  db,
  listChatMembers,
  listInstances,
  listMessages,
} from "../db.ts";
import type {
  AskRow,
  ChatRow,
  InstanceRow,
  MessageRow,
} from "../db.ts";
import { listToolCalls, type ToolCallRow } from "../audit-log.ts";

interface ChatSummary {
  chat: ChatRow;
  members: string[];
  unread_per_member: Record<string, number>;
  recent_messages: MessageRow[];
}

interface Snapshot {
  generated_at: number;
  instances: InstanceRow[];
  chats: ChatSummary[];
  asks: AskRow[];
  recent_calls: ToolCallRow[];
}

const DEFAULT_ACTIVE_MS = 15 * 60_000; // last 15 minutes
const DEFAULT_RECENT_MESSAGES = 50;
const DEFAULT_ASK_LOOKBACK_MS = 60 * 60_000; // 1 hour

export function snapshot(
  opts: { recentMessages?: number; activeMs?: number; askLookbackMs?: number } = {},
): Snapshot {
  const recentMessages = opts.recentMessages ?? DEFAULT_RECENT_MESSAGES;
  const activeMs = opts.activeMs ?? DEFAULT_ACTIVE_MS;
  const askLookbackMs = opts.askLookbackMs ?? DEFAULT_ASK_LOOKBACK_MS;
  const d = db();

  const instances = listInstances(activeMs);

  const chatRows = d
    .query<ChatRow, []>(
      "SELECT id, kind, title, created_at FROM chats ORDER BY created_at DESC",
    )
    .all();

  const chats: ChatSummary[] = chatRows.map((chat) => {
    const members = listChatMembers(chat.id);
    const recent = listMessages(chat.id, 0, 10_000).slice(-recentMessages);
    const unread_per_member: Record<string, number> = {};
    for (const m of members) {
      const row = d
        .query<{ c: number }, [string, number, string]>(
          `SELECT COUNT(*) AS c FROM messages
           WHERE chat_id = ? AND id > ? AND from_pseudonym != ?`,
        )
        .get(chat.id, m.last_read_message_id, m.pseudonym);
      unread_per_member[m.pseudonym] = row?.c ?? 0;
    }
    return {
      chat,
      members: members.map((m) => m.pseudonym),
      unread_per_member,
      recent_messages: recent,
    };
  });

  const cutoff = Date.now() - askLookbackMs;
  const asks = d
    .query<AskRow, [number]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks
       WHERE answered_at IS NULL OR answered_at >= ?
       ORDER BY id DESC LIMIT 200`,
    )
    .all(cutoff);

  const recent_calls = listToolCalls({ limit: 50 });

  return { generated_at: Date.now(), instances, chats, asks, recent_calls };
}
