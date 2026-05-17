/**
 * Pure snapshot builder for the web dashboard. Reads the shared SQLite store
 * and returns a JSON-serialisable view of "what's interesting right now".
 *
 * When `viewer` is provided, every pseudonym in the output is decorated with
 * a `display_name` field resolved via src/nickname.ts (personal nickname,
 * else group nickname in chat context, else the pseudonym itself).
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
import { displayName } from "../nickname.ts";

interface ChatSummary {
  chat: ChatRow;
  members: Array<{ pseudonym: string; display_name: string }>;
  unread_per_member: Record<string, number>;
  recent_messages: Array<MessageRow & { display_from_name: string }>;
}

interface InstanceWithDisplay extends InstanceRow {
  display_name: string;
}

interface AskWithDisplay extends AskRow {
  display_from_name: string;
  display_to_name: string;
}

interface ToolCallWithDisplay extends ToolCallRow {
  display_pseudonym_name: string;
}

interface Snapshot {
  generated_at: number;
  viewer: string | null;
  instances: InstanceWithDisplay[];
  chats: ChatSummary[];
  asks: AskWithDisplay[];
  recent_calls: ToolCallWithDisplay[];
}

const DEFAULT_ACTIVE_MS = 15 * 60_000;
const DEFAULT_RECENT_MESSAGES = 50;
const DEFAULT_ASK_LOOKBACK_MS = 60 * 60_000;

/** Resolve display name with a passthrough when viewer is null. */
function nameFor(viewer: string | null, target: string, chatId: string | null = null): string {
  if (viewer === null) return target;
  return displayName(viewer, target, chatId);
}

export function snapshot(
  opts: {
    recentMessages?: number;
    activeMs?: number;
    askLookbackMs?: number;
    viewer?: string | null;
  } = {},
): Snapshot {
  const recentMessages = opts.recentMessages ?? DEFAULT_RECENT_MESSAGES;
  const activeMs = opts.activeMs ?? DEFAULT_ACTIVE_MS;
  const askLookbackMs = opts.askLookbackMs ?? DEFAULT_ASK_LOOKBACK_MS;
  const viewer = opts.viewer ?? null;
  const d = db();

  const instances: InstanceWithDisplay[] = listInstances(activeMs).map((i) => ({
    ...i,
    display_name: nameFor(viewer, i.pseudonym, null),
  }));

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
      members: members.map((m) => ({
        pseudonym: m.pseudonym,
        display_name: nameFor(viewer, m.pseudonym, chat.id),
      })),
      unread_per_member,
      recent_messages: recent.map((msg) => ({
        ...msg,
        display_from_name: nameFor(viewer, msg.from_pseudonym, chat.id),
      })),
    };
  });

  const cutoff = Date.now() - askLookbackMs;
  const asks: AskWithDisplay[] = d
    .query<AskRow, [number]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks
       WHERE answered_at IS NULL OR answered_at >= ?
       ORDER BY id DESC LIMIT 200`,
    )
    .all(cutoff)
    .map((a) => ({
      ...a,
      display_from_name: nameFor(viewer, a.from_pseudonym, null),
      display_to_name: nameFor(viewer, a.to_pseudonym, null),
    }));

  const recent_calls: ToolCallWithDisplay[] = listToolCalls({ limit: 50 }).map((c) => ({
    ...c,
    display_pseudonym_name: nameFor(viewer, c.pseudonym, null),
  }));

  return { generated_at: Date.now(), viewer, instances, chats, asks, recent_calls };
}
