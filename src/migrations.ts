/**
 * SQLite schema for ClaudeTalk. Called once at db() startup; idempotent.
 * Initial CREATE TABLEs are wrapped in a single exec for atomicity; later
 * ALTERs go through addColumnIfMissing() so concurrent server starts don't
 * race between an introspection check and the actual ALTER.
 */
import type { Database } from "bun:sqlite";

export function migrate(d: Database): void {
  d.exec(`
    CREATE TABLE IF NOT EXISTS instances (
      pseudonym   TEXT PRIMARY KEY,
      path        TEXT NOT NULL,
      first_seen  INTEGER NOT NULL,
      last_seen   INTEGER NOT NULL,
      pid         INTEGER
    );

    CREATE TABLE IF NOT EXISTS chats (
      id          TEXT PRIMARY KEY,
      kind        TEXT NOT NULL CHECK (kind IN ('direct','group')),
      title       TEXT,
      created_at  INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS chat_members (
      chat_id              TEXT NOT NULL,
      pseudonym            TEXT NOT NULL,
      joined_at            INTEGER NOT NULL,
      last_read_message_id INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (chat_id, pseudonym),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS messages (
      id              INTEGER PRIMARY KEY AUTOINCREMENT,
      chat_id         TEXT NOT NULL,
      from_pseudonym  TEXT NOT NULL,
      body            TEXT NOT NULL,
      created_at      INTEGER NOT NULL,
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_messages_chat_id ON messages(chat_id, id);

    CREATE TABLE IF NOT EXISTS asks (
      id              INTEGER PRIMARY KEY AUTOINCREMENT,
      from_pseudonym  TEXT NOT NULL,
      to_pseudonym    TEXT NOT NULL,
      body            TEXT NOT NULL,
      created_at      INTEGER NOT NULL,
      answered_at     INTEGER,
      answer_body     TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_asks_to_pending ON asks(to_pseudonym, answered_at);
    CREATE INDEX IF NOT EXISTS idx_asks_from ON asks(from_pseudonym, answered_at);

    -- Nicknames I (viewer) assign to other instances. Unilateral, immediate.
    CREATE TABLE IF NOT EXISTS personal_nicknames (
      viewer    TEXT NOT NULL,
      target    TEXT NOT NULL,
      nickname  TEXT NOT NULL,
      set_at    INTEGER NOT NULL,
      PRIMARY KEY (viewer, target)
    );

    -- Per-(chat, target, voter) vote for what to call target in that chat.
    -- A group nickname is "active" when at least 2 voters (including target)
    -- agree on the same nickname. Derived at query time.
    CREATE TABLE IF NOT EXISTS group_nickname_votes (
      chat_id   TEXT NOT NULL,
      target    TEXT NOT NULL,
      voter     TEXT NOT NULL,
      nickname  TEXT NOT NULL,
      voted_at  INTEGER NOT NULL,
      PRIMARY KEY (chat_id, target, voter),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_gnv_target ON group_nickname_votes(chat_id, target);

    -- Audit log of every MCP tool call across all instances. Args & result
    -- are JSON snippets, truncated. Useful for live debugging and the
    -- dashboard's activity feed.
    CREATE TABLE IF NOT EXISTS tool_calls (
      id              INTEGER PRIMARY KEY AUTOINCREMENT,
      pseudonym       TEXT NOT NULL,
      tool            TEXT NOT NULL,
      args_json       TEXT,
      result_summary  TEXT,
      is_error        INTEGER NOT NULL DEFAULT 0,
      error           TEXT,
      started_at      INTEGER NOT NULL,
      duration_ms     INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_tool_calls_time ON tool_calls(started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_tool_calls_pseudonym ON tool_calls(pseudonym, started_at DESC);

    -- Phase 1.1 reactions: each (message_id, reactor) holds one reaction.
    -- Re-reacting replaces. ON DELETE CASCADE for message lifecycle parity.
    CREATE TABLE IF NOT EXISTS message_reactions (
      message_id INTEGER NOT NULL,
      reactor    TEXT NOT NULL,
      reaction   TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      PRIMARY KEY (message_id, reactor),
      FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_reactions_msg ON message_reactions(message_id);

    -- Phase 1.2 mentions: parsed at insert time. Lets the hook surface a
    -- high-priority signal for @-mentions independent of the chat dedup
    -- cursor.
    CREATE TABLE IF NOT EXISTS message_mentions (
      message_id INTEGER NOT NULL,
      target     TEXT NOT NULL,
      PRIMARY KEY (message_id, target),
      FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_mentions_target ON message_mentions(target, message_id);

    -- Phase 2.1 instance status: optional short status string (+ emoji) per
    -- instance. Visible in discover so peers know who is busy, away, etc.
    CREATE TABLE IF NOT EXISTS instance_status (
      pseudonym  TEXT PRIMARY KEY,
      status     TEXT NOT NULL,
      emoji      TEXT,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY (pseudonym) REFERENCES instances(pseudonym) ON DELETE CASCADE
    );

    -- Phase 2.3 per-chat preferences (per viewer). Today: mute flag for the
    -- hook (silenced chats produce no header). Future: verbosity / digest
    -- intervals.
    CREATE TABLE IF NOT EXISTS chat_preferences (
      viewer    TEXT NOT NULL,
      chat_id   TEXT NOT NULL,
      muted     INTEGER NOT NULL DEFAULT 0,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (viewer, chat_id),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
  `);

  // Forward-compat ALTERs. Concurrent server starts would race between a
  // pragma-introspect check and the ALTER itself, so we try-catch and
  // swallow "duplicate column name" instead.
  const addColumnIfMissing = (sql: string) => {
    try {
      d.exec(sql);
    } catch (e: any) {
      const msg = String(e?.message ?? e ?? "");
      if (!/duplicate column name/i.test(msg)) throw e;
    }
  };
  addColumnIfMissing(`ALTER TABLE tool_calls ADD COLUMN kind TEXT NOT NULL DEFAULT 'tool';`);
  addColumnIfMissing(`ALTER TABLE tool_calls ADD COLUMN direction TEXT NOT NULL DEFAULT 'in';`);
  addColumnIfMissing("ALTER TABLE tool_calls ADD COLUMN jrpc_id INTEGER;");

  // Hook dedup cursors.
  addColumnIfMissing(
    "ALTER TABLE chat_members ADD COLUMN last_notified_message_id INTEGER NOT NULL DEFAULT 0;",
  );
  addColumnIfMissing(
    "ALTER TABLE instances ADD COLUMN last_notified_ask_id INTEGER NOT NULL DEFAULT 0;",
  );

  // Phase 1.2 mention cursor — separate from chat dedup so @-mentions
  // can break through even when the chat itself is "caught up".
  addColumnIfMissing(
    "ALTER TABLE instances ADD COLUMN last_notified_mention_id INTEGER NOT NULL DEFAULT 0;",
  );

  // Phase 1.3 reply threading.
  addColumnIfMissing("ALTER TABLE messages ADD COLUMN parent_id INTEGER;");
}
