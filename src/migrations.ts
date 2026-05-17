/**
 * SQLite schema for ClaudeTalk. Versioned migrations run inside a single
 * transaction per upgrade step so concurrent server starts don't race.
 *
 * Each entry in MIGRATIONS is `{ version, name, up(d) }`. On startup we
 * read `PRAGMA user_version`, then apply every migration with version >
 * that value, in order, each inside its own `BEGIN IMMEDIATE` so two
 * processes can't run the same step twice.
 *
 * Past schema (versions 1..N) is recorded as one big idempotent step so
 * upgrades from any prior commit reach the same shape.
 */
import type { Database } from "bun:sqlite";

interface Migration {
  version: number;
  name: string;
  up: (d: Database) => void;
}

const MIGRATIONS: Migration[] = [
  {
    version: 1,
    name: "initial schema (v0.1.0 .. v0.3.1)",
    up: (d) => {
      d.exec(`
        CREATE TABLE IF NOT EXISTS instances (
          pseudonym   TEXT PRIMARY KEY,
          path        TEXT NOT NULL,
          first_seen  INTEGER NOT NULL,
          last_seen   INTEGER NOT NULL,
          pid         INTEGER,
          last_notified_ask_id INTEGER NOT NULL DEFAULT 0,
          last_notified_mention_id INTEGER NOT NULL DEFAULT 0
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
          last_notified_message_id INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (chat_id, pseudonym),
          FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS messages (
          id              INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_id         TEXT NOT NULL,
          from_pseudonym  TEXT NOT NULL,
          body            TEXT NOT NULL,
          created_at      INTEGER NOT NULL,
          parent_id       INTEGER,
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

        CREATE TABLE IF NOT EXISTS personal_nicknames (
          viewer    TEXT NOT NULL,
          target    TEXT NOT NULL,
          nickname  TEXT NOT NULL,
          set_at    INTEGER NOT NULL,
          PRIMARY KEY (viewer, target)
        );

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

        CREATE TABLE IF NOT EXISTS tool_calls (
          id              INTEGER PRIMARY KEY AUTOINCREMENT,
          pseudonym       TEXT NOT NULL,
          tool            TEXT NOT NULL,
          args_json       TEXT,
          result_summary  TEXT,
          is_error        INTEGER NOT NULL DEFAULT 0,
          error           TEXT,
          started_at      INTEGER NOT NULL,
          duration_ms     INTEGER NOT NULL,
          kind            TEXT NOT NULL DEFAULT 'tool',
          direction       TEXT NOT NULL DEFAULT 'in',
          jrpc_id         INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tool_calls_time ON tool_calls(started_at DESC);
        CREATE INDEX IF NOT EXISTS idx_tool_calls_pseudonym ON tool_calls(pseudonym, started_at DESC);

        CREATE TABLE IF NOT EXISTS message_reactions (
          message_id INTEGER NOT NULL,
          reactor    TEXT NOT NULL,
          reaction   TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          PRIMARY KEY (message_id, reactor),
          FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_reactions_msg ON message_reactions(message_id);

        CREATE TABLE IF NOT EXISTS message_mentions (
          message_id INTEGER NOT NULL,
          target     TEXT NOT NULL,
          PRIMARY KEY (message_id, target),
          FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_mentions_target ON message_mentions(target, message_id);

        CREATE TABLE IF NOT EXISTS instance_status (
          pseudonym  TEXT PRIMARY KEY,
          status     TEXT NOT NULL,
          emoji      TEXT,
          updated_at INTEGER NOT NULL,
          FOREIGN KEY (pseudonym) REFERENCES instances(pseudonym) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS chat_preferences (
          viewer    TEXT NOT NULL,
          chat_id   TEXT NOT NULL,
          muted     INTEGER NOT NULL DEFAULT 0,
          updated_at INTEGER NOT NULL,
          PRIMARY KEY (viewer, chat_id),
          FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );
      `);

      // Pre-v0.4 deployments created this schema piecemeal via
      // addColumnIfMissing(). For those installs, ensure the columns
      // exist (the CREATE TABLE IF NOT EXISTS above keeps the old
      // narrower schema). idempotent ALTERs with try/catch:
      const addCol = (sql: string) => {
        try {
          d.exec(sql);
        } catch (e: any) {
          if (!/duplicate column name/i.test(String(e?.message ?? e ?? ""))) throw e;
        }
      };
      addCol("ALTER TABLE instances ADD COLUMN last_notified_ask_id INTEGER NOT NULL DEFAULT 0;");
      addCol("ALTER TABLE instances ADD COLUMN last_notified_mention_id INTEGER NOT NULL DEFAULT 0;");
      addCol("ALTER TABLE chat_members ADD COLUMN last_notified_message_id INTEGER NOT NULL DEFAULT 0;");
      addCol("ALTER TABLE messages ADD COLUMN parent_id INTEGER;");
      addCol("ALTER TABLE tool_calls ADD COLUMN kind TEXT NOT NULL DEFAULT 'tool';");
      addCol("ALTER TABLE tool_calls ADD COLUMN direction TEXT NOT NULL DEFAULT 'in';");
      addCol("ALTER TABLE tool_calls ADD COLUMN jrpc_id INTEGER;");
    },
  },
  // Future migrations: { version: 2, name: "...", up: (d) => { d.exec("ALTER TABLE ..."); } },
];

export function currentSchemaVersion(d: Database): number {
  const row = d.query<{ user_version: number }, []>("PRAGMA user_version").get();
  return row?.user_version ?? 0;
}

export function targetSchemaVersion(): number {
  return MIGRATIONS.reduce((max, m) => Math.max(max, m.version), 0);
}

export function migrate(d: Database): void {
  const have = currentSchemaVersion(d);
  const target = targetSchemaVersion();
  if (have >= target) return;
  for (const m of MIGRATIONS) {
    if (m.version <= have) continue;
    // BEGIN IMMEDIATE so two concurrent migrators race on the lock
    // instead of both attempting the same ALTER. The second one will
    // wake up with user_version already bumped past this step and skip.
    try {
      d.exec("BEGIN IMMEDIATE;");
      // Re-check inside the transaction (another process may have raced ahead).
      const current = currentSchemaVersion(d);
      if (current >= m.version) {
        d.exec("ROLLBACK;");
        continue;
      }
      m.up(d);
      d.exec(`PRAGMA user_version = ${m.version};`);
      d.exec("COMMIT;");
    } catch (e: any) {
      try { d.exec("ROLLBACK;"); } catch {}
      // SQLITE_BUSY here is fine — another process is migrating; we'll
      // observe the bumped user_version on the next iteration / startup.
      if (
        e?.code === "SQLITE_BUSY" ||
        e?.code === "SQLITE_LOCKED" ||
        /database is locked/i.test(String(e?.message ?? ""))
      ) {
        return;
      }
      throw e;
    }
  }
}
