import { Database } from "bun:sqlite";
import { migrate } from "./migrations.ts";
import { dbPath, ensureRootDir } from "./paths.ts";

export interface InstanceRow {
  pseudonym: string;
  path: string;
  first_seen: number;
  last_seen: number;
  pid: number | null;
}

export interface MessageRow {
  id: number;
  chat_id: string;
  from_pseudonym: string;
  body: string;
  created_at: number;
  parent_id: number | null;
}

export interface AskRow {
  id: number;
  from_pseudonym: string;
  to_pseudonym: string;
  body: string;
  created_at: number;
  answered_at: number | null;
  answer_body: string | null;
}

export interface ChatRow {
  id: string;
  kind: "direct" | "group";
  title: string | null;
  created_at: number;
}

export interface ChatMemberRow {
  chat_id: string;
  pseudonym: string;
  joined_at: number;
  last_read_message_id: number;
  last_notified_message_id: number;
}

let _db: Database | null = null;

/** Reset the cached DB handle. Tests use this between cases to avoid sharing state. */
export function resetDb(): void {
  if (_db) {
    try {
      _db.close();
    } catch {}
    _db = null;
  }
}

/** Force a TRUNCATE-mode checkpoint to reclaim WAL space. Used by long-lived
 *  processes (dashboard, MCP server) on a timer to keep db.sqlite-wal small.
 *  Safe to call any time; busy-tolerant — failures are swallowed. */
export function checkpointWal(): void {
  try {
    db().exec("PRAGMA wal_checkpoint(TRUNCATE);");
  } catch {
    // Another writer may hold the lock briefly; harmless to skip.
  }
}

export function db(): Database {
  if (_db) return _db;
  ensureRootDir();
  const d = new Database(dbPath(), { create: true });
  // busy_timeout MUST be set first so subsequent pragmas / migrations wait
  // out the brief locks taken by other concurrently-starting MCP servers.
  // Kept short (2s) so contended writes from hot paths (audit log,
  // multi-instance traffic) fail fast and drop rather than blocking the
  // MCP tool handler itself for 10s+.
  d.exec("PRAGMA busy_timeout = 500;");
  retryBusy(() => d.exec("PRAGMA journal_mode = WAL;"));
  d.exec("PRAGMA synchronous = NORMAL;");
  d.exec("PRAGMA foreign_keys = ON;");
  // Phase 5.1: more aggressive auto-checkpoint than SQLite's default 1000
  // pages. Our access pattern (many short writes from concurrent processes)
  // grew the WAL to ~1 MB while the main DB was only ~250 KB. 256 pages ≈
  // 1 MB cap → checkpoint kicks in well before the file balloons.
  d.exec("PRAGMA wal_autocheckpoint = 256;");
  retryBusy(() => migrate(d));
  _db = d;
  return d;
}

function retryBusy(fn: () => void, attempts = 30, sleepMs = 100): void {
  for (let i = 0; i < attempts; i++) {
    try {
      fn();
      return;
    } catch (e: any) {
      if (e?.code === "SQLITE_BUSY" || e?.code === "SQLITE_LOCKED") {
        Bun.sleepSync(sleepMs);
        continue;
      }
      throw e;
    }
  }
  fn(); // final attempt, throws if still locked
}

// Schema migration lives in src/migrations.ts.

// Tool-call audit log helpers live in src/audit-log.ts (the migration above
// owns the `tool_calls` table). Keeping the helpers next to the wrapper that
// uses them keeps this file under its file-size budget.

function now(): number {
  return Date.now();
}

/** Stable direct-chat id from a pair of pseudonyms (order-independent). */
export function directChatId(a: string, b: string): string {
  const [x, y] = [a, b].sort();
  return `direct:${x}|${y}`;
}

/** Namespaced group-chat id from a slug. */
export function groupChatId(slug: string): string {
  return `group:${slug}`;
}

// ---------- presence ----------

export function upsertInstance(pseudonym: string, path: string, pid: number): void {
  const t = now();
  db().run(
    `INSERT INTO instances (pseudonym, path, first_seen, last_seen, pid)
     VALUES (?, ?, ?, ?, ?)
     ON CONFLICT(pseudonym) DO UPDATE SET
       path = excluded.path,
       last_seen = excluded.last_seen,
       pid = excluded.pid`,
    [pseudonym, path, t, t, pid],
  );
}

export function touchInstance(pseudonym: string): void {
  db().run("UPDATE instances SET last_seen = ? WHERE pseudonym = ?", [now(), pseudonym]);
}

export function listInstances(activeWithinMs: number): InstanceRow[] {
  const cutoff = now() - activeWithinMs;
  return db()
    .query<InstanceRow, [number]>(
      `SELECT pseudonym, path, first_seen, last_seen, pid
       FROM instances WHERE last_seen >= ? ORDER BY pseudonym ASC`,
    )
    .all(cutoff);
}

export function getInstance(pseudonym: string): InstanceRow | null {
  return (
    db()
      .query<InstanceRow, [string]>(
        "SELECT pseudonym, path, first_seen, last_seen, pid FROM instances WHERE pseudonym = ?",
      )
      .get(pseudonym) ?? null
  );
}

// ---------- chats ----------

export function ensureChat(id: string, kind: "direct" | "group", title: string | null): void {
  db().run(
    `INSERT INTO chats (id, kind, title, created_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(id) DO NOTHING`,
    [id, kind, title, now()],
  );
}

export function getChat(id: string): ChatRow | null {
  return (
    db()
      .query<ChatRow, [string]>("SELECT id, kind, title, created_at FROM chats WHERE id = ?")
      .get(id) ?? null
  );
}

export function addChatMember(chatId: string, pseudonym: string): void {
  db().run(
    `INSERT INTO chat_members (chat_id, pseudonym, joined_at, last_read_message_id)
     VALUES (?, ?, ?, 0)
     ON CONFLICT(chat_id, pseudonym) DO NOTHING`,
    [chatId, pseudonym, now()],
  );
}

export function listChatMembers(chatId: string): ChatMemberRow[] {
  return db()
    .query<ChatMemberRow, [string]>(
      `SELECT chat_id, pseudonym, joined_at, last_read_message_id, last_notified_message_id
       FROM chat_members WHERE chat_id = ? ORDER BY joined_at ASC`,
    )
    .all(chatId);
}

export function listChatsFor(pseudonym: string): { chat: ChatRow; member: ChatMemberRow }[] {
  const rows = db()
    .query<ChatRow & ChatMemberRow, [string]>(
      `SELECT c.id AS id, c.kind AS kind, c.title AS title, c.created_at AS created_at,
              m.chat_id AS chat_id, m.pseudonym AS pseudonym, m.joined_at AS joined_at,
              m.last_read_message_id AS last_read_message_id,
              m.last_notified_message_id AS last_notified_message_id
       FROM chats c
       INNER JOIN chat_members m ON m.chat_id = c.id
       WHERE m.pseudonym = ?
       ORDER BY c.created_at DESC`,
    )
    .all(pseudonym);
  return rows.map((r) => ({
    chat: { id: r.id, kind: r.kind, title: r.title, created_at: r.created_at },
    member: {
      chat_id: r.chat_id,
      pseudonym: r.pseudonym,
      joined_at: r.joined_at,
      last_read_message_id: r.last_read_message_id,
      last_notified_message_id: r.last_notified_message_id,
    },
  }));
}

export function insertMessage(
  chatId: string,
  from: string,
  body: string,
  parentId: number | null = null,
): MessageRow {
  const t = now();
  const res = db().run(
    "INSERT INTO messages (chat_id, from_pseudonym, body, created_at, parent_id) VALUES (?, ?, ?, ?, ?)",
    [chatId, from, body, t, parentId],
  );
  return {
    id: Number(res.lastInsertRowid),
    chat_id: chatId,
    from_pseudonym: from,
    body,
    created_at: t,
    parent_id: parentId,
  };
}

export function listMessages(chatId: string, sinceId: number, limit: number): MessageRow[] {
  return db()
    .query<MessageRow, [string, number, number]>(
      `SELECT id, chat_id, from_pseudonym, body, created_at, parent_id
       FROM messages WHERE chat_id = ? AND id > ?
       ORDER BY id ASC LIMIT ?`,
    )
    .all(chatId, sinceId, limit);
}

export function getMessage(id: number): MessageRow | null {
  return (
    db()
      .query<MessageRow, [number]>(
        `SELECT id, chat_id, from_pseudonym, body, created_at, parent_id
         FROM messages WHERE id = ?`,
      )
      .get(id) ?? null
  );
}

export function markChatRead(chatId: string, pseudonym: string, upToId: number): void {
  db().run(
    `UPDATE chat_members SET last_read_message_id = ?
     WHERE chat_id = ? AND pseudonym = ? AND last_read_message_id < ?`,
    [upToId, chatId, pseudonym, upToId],
  );
}

// ---------- asks ----------

export function insertAsk(from: string, to: string, body: string): AskRow {
  const t = now();
  const res = db().run(
    "INSERT INTO asks (from_pseudonym, to_pseudonym, body, created_at) VALUES (?, ?, ?, ?)",
    [from, to, body, t],
  );
  return {
    id: Number(res.lastInsertRowid),
    from_pseudonym: from,
    to_pseudonym: to,
    body,
    created_at: t,
    answered_at: null,
    answer_body: null,
  };
}

export function getAsk(id: number): AskRow | null {
  return (
    db()
      .query<AskRow, [number]>(
        `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
         FROM asks WHERE id = ?`,
      )
      .get(id) ?? null
  );
}

export function answerAsk(id: number, answerer: string, answer: string): AskRow | null {
  const ask = getAsk(id);
  if (!ask) return null;
  if (ask.to_pseudonym !== answerer) return null;
  if (ask.answered_at !== null) return ask;
  db().run("UPDATE asks SET answered_at = ?, answer_body = ? WHERE id = ?", [now(), answer, id]);
  return getAsk(id);
}

export function listPendingAsksFor(to: string): AskRow[] {
  return db()
    .query<AskRow, [string]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks WHERE to_pseudonym = ? AND answered_at IS NULL
       ORDER BY id ASC`,
    )
    .all(to);
}

export function listAnsweredAsksFrom(
  from: string,
  sinceSeenId: number,
): AskRow[] {
  return db()
    .query<AskRow, [string, number]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks WHERE from_pseudonym = ? AND answered_at IS NOT NULL AND id > ?
       ORDER BY id ASC`,
    )
    .all(from, sinceSeenId);
}

// Nickname tables are migrated above; the helpers live in src/nickname.ts
// to keep this module focused on chats / asks / presence. Tests and tools
// import the nickname helpers from there.

/** Internal: exposed only so src/nickname.ts can share the singleton handle. */
export function _now(): number {
  return now();
}

// Notification-cursor + discoverable-groups helpers live in src/notifications.ts
// (split out of this file to keep it under its file-size budget). Re-exports
// preserve the old import path for callers that haven't migrated yet.
export {
  advanceNotificationCursors,
  discoverableGroupsFor,
  notificationDeltaFor,
  type NotificationDelta,
} from "./notifications.ts";

// ---------- long-poll helper ----------

export interface Unread {
  pendingAsks: AskRow[];
  unreadChats: Array<{
    chat: ChatRow;
    unreadCount: number;
    latest: MessageRow | null;
    lastReadId: number;
  }>;
}

export function unreadSummary(pseudonym: string): Unread {
  const pendingAsks = listPendingAsksFor(pseudonym);
  const chats = listChatsFor(pseudonym);
  const unreadChats = chats
    .map(({ chat, member }) => {
      const row = db()
        .query<{ c: number }, [string, number, string]>(
          `SELECT COUNT(*) AS c FROM messages
           WHERE chat_id = ? AND id > ? AND from_pseudonym != ?`,
        )
        .get(chat.id, member.last_read_message_id, pseudonym);
      const latest = db()
        .query<MessageRow, [string]>(
          `SELECT id, chat_id, from_pseudonym, body, created_at, parent_id
           FROM messages WHERE chat_id = ? ORDER BY id DESC LIMIT 1`,
        )
        .get(chat.id) ?? null;
      const c = row?.c ?? 0;
      return { chat, unreadCount: c, latest, lastReadId: member.last_read_message_id };
    })
    .filter((x) => x.unreadCount > 0);
  return { pendingAsks, unreadChats };
}

