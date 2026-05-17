/**
 * @-mentions parsed from chat message bodies at insert time. Each (message,
 * target) row lets the hook surface a high-priority "you were mentioned"
 * signal independent of the per-chat dedup cursor — so a mention breaks
 * through even when the chat is otherwise caught up.
 *
 * v1: only literal pseudonym matches (e.g. `@SwiftFox-a3f`). Nicknames are
 * deliberately NOT resolved because they're per-viewer / per-chat and would
 * give surprising results across sessions. Future work can layer in
 * group-nickname resolution scoped to the message's own chat.
 */
import { _now, db, getInstance } from "./db.ts";

/** Pseudonym shape from src/pseudonym.ts: `<Adjective><Animal>-<3hex>`.
 *  Adjective + Animal are CamelCase, hex is lowercase. */
const MENTION_RE = /@([A-Z][a-z]+[A-Z][a-z]+-[0-9a-f]{3})/g;

export interface MentionForTarget {
  message_id: number;
  chat_id: string;
  from_pseudonym: string;
}

/** Pure parser. Returns the unique set of valid pseudonym tokens in `body`,
 *  ignoring duplicates. Does NOT verify the pseudonyms exist (caller does). */
export function parseMentions(body: string): string[] {
  const found = new Set<string>();
  for (const m of body.matchAll(MENTION_RE)) {
    found.add(m[1]!);
  }
  return [...found];
}

/** Insert mention rows for every pseudonym in `body` that's an actually-known
 *  instance. Self-mentions are skipped (you can't @ yourself meaningfully). */
export function recordMessageMentions(
  messageId: number,
  body: string,
  authorPseudonym: string,
): void {
  const targets = parseMentions(body);
  if (targets.length === 0) return;
  for (const target of targets) {
    if (target === authorPseudonym) continue;
    if (!getInstance(target)) continue; // unknown pseudonym → silently drop
    db().run(
      `INSERT INTO message_mentions (message_id, target) VALUES (?, ?)
       ON CONFLICT(message_id, target) DO NOTHING`,
      [messageId, target],
    );
  }
  void _now; // silence "unused" — kept available for future timestamping
}

/** Mentions of `target` strictly newer than `sinceId`, with the message
 *  chat + author resolved. Used by the hook to surface "@ you" priority
 *  signals independent of the chat dedup cursor. */
export function mentionsForTargetSince(
  target: string,
  sinceId: number,
): MentionForTarget[] {
  return db()
    .query<MentionForTarget, [string, number]>(
      `SELECT mm.message_id AS message_id, m.chat_id AS chat_id, m.from_pseudonym AS from_pseudonym
       FROM message_mentions mm
       JOIN messages m ON m.id = mm.message_id
       WHERE mm.target = ? AND mm.message_id > ?
       ORDER BY mm.message_id ASC`,
    )
    .all(target, sinceId);
}

export function getMentionCursor(pseudonym: string): number {
  const row = db()
    .query<{ last_notified_mention_id: number }, [string]>(
      "SELECT last_notified_mention_id FROM instances WHERE pseudonym = ?",
    )
    .get(pseudonym);
  return row?.last_notified_mention_id ?? 0;
}

export function advanceMentionCursor(pseudonym: string, maxId: number): void {
  if (maxId <= 0) return;
  db().run(
    `UPDATE instances SET last_notified_mention_id = ?
     WHERE pseudonym = ? AND last_notified_mention_id < ?`,
    [maxId, pseudonym, maxId],
  );
}
