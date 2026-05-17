/**
 * Notification-cursor helpers + discoverable-groups query. Used by the hook
 * (hooks/check-inbox.ts) and the `inbox` tool to dedup re-notifications and
 * surface group chats the viewer hasn't joined yet.
 */
import { db, listChatsFor } from "./db.ts";
import type { AskRow, ChatRow, MessageRow } from "./db.ts";

export interface NotificationDelta {
  /** Asks for me, strictly newer than my last notification cursor. */
  newAsks: AskRow[];
  /** Chats where >=1 message has arrived from someone else past my cursor. */
  newChats: Array<{
    chat: ChatRow;
    new_count: number;
    latest: MessageRow;
    last_notified_id: number;
    max_message_id: number;
  }>;
  /** Cursor I should write back if I emit. */
  ask_cursor_target: number;
}

export function notificationDeltaFor(pseudonym: string): NotificationDelta {
  const d = db();
  const me = d
    .query<{ last_notified_ask_id: number }, [string]>(
      `SELECT last_notified_ask_id FROM instances WHERE pseudonym = ?`,
    )
    .get(pseudonym);
  const askCursor = me?.last_notified_ask_id ?? 0;
  const newAsks = d
    .query<AskRow, [string, number]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks WHERE to_pseudonym = ? AND id > ? AND answered_at IS NULL
       ORDER BY id ASC`,
    )
    .all(pseudonym, askCursor);
  const ask_cursor_target =
    newAsks.length > 0 ? newAsks[newAsks.length - 1]!.id : askCursor;

  const chats = listChatsFor(pseudonym);
  const newChats: NotificationDelta["newChats"] = [];
  for (const { chat, member } of chats) {
    const notifCursor = member.last_notified_message_id;
    const maxRow = d
      .query<{ max_id: number | null }, [string, string]>(
        `SELECT MAX(id) AS max_id FROM messages
         WHERE chat_id = ? AND from_pseudonym != ?`,
      )
      .get(chat.id, pseudonym);
    const maxId = maxRow?.max_id ?? 0;
    if (maxId <= notifCursor) continue;
    const countRow = d
      .query<{ c: number }, [string, number, string]>(
        `SELECT COUNT(*) AS c FROM messages
         WHERE chat_id = ? AND id > ? AND from_pseudonym != ?`,
      )
      .get(chat.id, notifCursor, pseudonym);
    const latest = d
      .query<MessageRow, [string, string]>(
        `SELECT id, chat_id, from_pseudonym, body, created_at
         FROM messages WHERE chat_id = ? AND from_pseudonym != ?
         ORDER BY id DESC LIMIT 1`,
      )
      .get(chat.id, pseudonym);
    if (!latest) continue;
    newChats.push({
      chat,
      new_count: countRow?.c ?? 1,
      latest,
      last_notified_id: notifCursor,
      max_message_id: maxId,
    });
  }
  return { newAsks, newChats, ask_cursor_target };
}

/** Advance the per-pseudonym ask cursor and per-(pseudonym, chat) message
 *  cursors. Idempotent — never moves a cursor backwards. */
export function advanceNotificationCursors(
  pseudonym: string,
  delta: NotificationDelta,
): void {
  const d = db();
  if (delta.ask_cursor_target > 0) {
    d.run(
      `UPDATE instances SET last_notified_ask_id = ?
       WHERE pseudonym = ? AND last_notified_ask_id < ?`,
      [delta.ask_cursor_target, pseudonym, delta.ask_cursor_target],
    );
  }
  for (const c of delta.newChats) {
    d.run(
      `UPDATE chat_members SET last_notified_message_id = ?
       WHERE chat_id = ? AND pseudonym = ? AND last_notified_message_id < ?`,
      [c.max_message_id, c.chat.id, pseudonym, c.max_message_id],
    );
  }
}

/** Group chats with recent activity that `pseudonym` is NOT a member of —
 *  surface them in inbox so the user knows to call `groupchat slug=X` to
 *  join. Returns at most `limit` rows. */
export function discoverableGroupsFor(
  pseudonym: string,
  activeWithinMs: number,
  limit = 20,
): Array<{ chat: ChatRow; member_count: number; latest_at: number; latest_from: string }> {
  const cutoff = Date.now() - activeWithinMs;
  return db()
    .query<
      {
        id: string;
        kind: string;
        title: string | null;
        created_at: number;
        member_count: number;
        latest_at: number;
        latest_from: string;
      },
      [string, number, number]
    >(
      `SELECT c.id AS id, c.kind AS kind, c.title AS title, c.created_at AS created_at,
              (SELECT COUNT(*) FROM chat_members cm WHERE cm.chat_id = c.id) AS member_count,
              (SELECT MAX(m.created_at) FROM messages m WHERE m.chat_id = c.id) AS latest_at,
              (SELECT m.from_pseudonym FROM messages m WHERE m.chat_id = c.id
                 ORDER BY m.id DESC LIMIT 1) AS latest_from
       FROM chats c
       WHERE c.kind = 'group'
         AND NOT EXISTS (
           SELECT 1 FROM chat_members cm
            WHERE cm.chat_id = c.id AND cm.pseudonym = ?
         )
         AND EXISTS (
           SELECT 1 FROM messages m
            WHERE m.chat_id = c.id AND m.created_at >= ?
         )
       ORDER BY latest_at DESC LIMIT ?`,
    )
    .all(pseudonym, cutoff, limit)
    .map((r) => ({
      chat: { id: r.id, kind: r.kind as "group", title: r.title, created_at: r.created_at },
      member_count: r.member_count,
      latest_at: r.latest_at,
      latest_from: r.latest_from,
    }));
}
