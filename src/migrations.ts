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
  {
    version: 2,
    name: "dashboard_version counter + write triggers (Phase 3.5)",
    up: (d) => {
      d.exec(`
        CREATE TABLE IF NOT EXISTS dashboard_version (
          id INTEGER PRIMARY KEY CHECK (id = 1),
          v  INTEGER NOT NULL DEFAULT 0
        );
        INSERT OR IGNORE INTO dashboard_version (id, v) VALUES (1, 0);

        CREATE TRIGGER IF NOT EXISTS dv_messages_ins
          AFTER INSERT ON messages BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_asks_ins
          AFTER INSERT ON asks BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_asks_upd
          AFTER UPDATE OF answered_at ON asks BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_instances_ins
          AFTER INSERT ON instances BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_chat_members_ins
          AFTER INSERT ON chat_members BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_reactions_ins
          AFTER INSERT ON message_reactions BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_reactions_del
          AFTER DELETE ON message_reactions BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_status_ins
          AFTER INSERT ON instance_status BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_status_upd
          AFTER UPDATE ON instance_status BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_chats_ins
          AFTER INSERT ON chats BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
      `);
    },
  },
  {
    version: 3,
    name: "Phase N0: messages.id → TEXT UUID + seq sidecar + machine_id",
    up: (d) => {
      // SQLite can't ALTER a column's type. Rebuild every table that
      // references messages.id (which is becoming TEXT). FK enforcement
      // is ON (PRAGMA foreign_keys = ON in db.ts), so disable it for
      // the duration of this migration — re-enabled at the bottom.
      d.exec("PRAGMA foreign_keys = OFF;");

      // 1. Sequence counter for messages.seq (per-DB monotonic; the unit
      //    that cursors compare against, NOT a cross-machine identity).
      d.exec(`
        CREATE TABLE IF NOT EXISTS message_seq (
          id   INTEGER PRIMARY KEY CHECK (id = 1),
          next INTEGER NOT NULL DEFAULT 1
        );
        INSERT OR IGNORE INTO message_seq (id, next) VALUES (1, 1);
      `);

      // Bootstrap the counter past any existing autoincrement ids so a
      // new insert can't collide with a legacy row.
      const maxOld = d
        .query<{ m: number | null }, []>("SELECT MAX(id) AS m FROM messages")
        .get();
      const startNext = (maxOld?.m ?? 0) + 1;
      d.run("UPDATE message_seq SET next = ? WHERE id = 1", [startNext]);

      // 2. Rebuild messages with TEXT id (UUID) + INTEGER seq.
      d.exec(`
        CREATE TABLE messages_new (
          id          TEXT PRIMARY KEY,
          seq         INTEGER NOT NULL UNIQUE,
          chat_id     TEXT NOT NULL,
          from_pseudonym TEXT NOT NULL,
          body        TEXT NOT NULL,
          created_at  INTEGER NOT NULL,
          parent_id   TEXT,
          FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );
        INSERT INTO messages_new (id, seq, chat_id, from_pseudonym, body, created_at, parent_id)
          SELECT CAST(id AS TEXT), id, chat_id, from_pseudonym, body, created_at,
                 CASE WHEN parent_id IS NOT NULL THEN CAST(parent_id AS TEXT) ELSE NULL END
          FROM messages;
        DROP TABLE messages;
        ALTER TABLE messages_new RENAME TO messages;
        CREATE INDEX idx_messages_chat_id ON messages(chat_id, seq);
      `);

      // 3. Rebuild message_reactions with TEXT message_id.
      d.exec(`
        CREATE TABLE message_reactions_new (
          message_id TEXT NOT NULL,
          reactor    TEXT NOT NULL,
          reaction   TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          PRIMARY KEY (message_id, reactor),
          FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
        );
        INSERT INTO message_reactions_new (message_id, reactor, reaction, created_at)
          SELECT CAST(message_id AS TEXT), reactor, reaction, created_at FROM message_reactions;
        DROP TABLE message_reactions;
        ALTER TABLE message_reactions_new RENAME TO message_reactions;
        CREATE INDEX idx_reactions_msg ON message_reactions(message_id);
      `);

      // 4. Rebuild message_mentions with TEXT message_id.
      d.exec(`
        CREATE TABLE message_mentions_new (
          message_id TEXT NOT NULL,
          target     TEXT NOT NULL,
          PRIMARY KEY (message_id, target),
          FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
        );
        INSERT INTO message_mentions_new (message_id, target)
          SELECT CAST(message_id AS TEXT), target FROM message_mentions;
        DROP TABLE message_mentions;
        ALTER TABLE message_mentions_new RENAME TO message_mentions;
        CREATE INDEX idx_mentions_target ON message_mentions(target, message_id);
      `);

      // 5. Rebuild chat_members with seq-based cursors. The old integer
      //    *_message_id columns were storing what is now the seq value
      //    (because pre-v3 id == seq for every row), so the rename is a
      //    pure column-name swap with no value transformation needed.
      d.exec(`
        CREATE TABLE chat_members_new (
          chat_id                   TEXT NOT NULL,
          pseudonym                 TEXT NOT NULL,
          joined_at                 INTEGER NOT NULL,
          last_read_message_seq     INTEGER NOT NULL DEFAULT 0,
          last_notified_message_seq INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (chat_id, pseudonym),
          FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
        );
        INSERT INTO chat_members_new
          (chat_id, pseudonym, joined_at, last_read_message_seq, last_notified_message_seq)
          SELECT chat_id, pseudonym, joined_at,
                 last_read_message_id, last_notified_message_id
            FROM chat_members;
        DROP TABLE chat_members;
        ALTER TABLE chat_members_new RENAME TO chat_members;
      `);

      // 6. Phase N0: machine_id on instances. Additive, nullable.
      try {
        d.exec("ALTER TABLE instances ADD COLUMN machine_id TEXT;");
      } catch (e: any) {
        if (!/duplicate column name/i.test(String(e?.message ?? e ?? ""))) throw e;
      }

      // 6b. Rename last_notified_mention_id → _seq. Mentions now reference
      //     messages.seq (the cursor unit) since the actual messages.id is
      //     a non-orderable UUID. Pre-v3 id == seq → value is compatible.
      try {
        d.exec(
          "ALTER TABLE instances RENAME COLUMN last_notified_mention_id TO last_notified_mention_seq;",
        );
      } catch (e: any) {
        if (!/no such column|duplicate column/i.test(String(e?.message ?? e ?? ""))) {
          throw e;
        }
      }

      // 7. Re-create the dashboard_version triggers — rebuilding the
      //    messages / chat_members / message_reactions tables dropped them.
      d.exec(`
        CREATE TRIGGER IF NOT EXISTS dv_messages_ins
          AFTER INSERT ON messages BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_chat_members_ins
          AFTER INSERT ON chat_members BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_reactions_ins
          AFTER INSERT ON message_reactions BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
        CREATE TRIGGER IF NOT EXISTS dv_reactions_del
          AFTER DELETE ON message_reactions BEGIN
            UPDATE dashboard_version SET v = v + 1 WHERE id = 1;
          END;
      `);

      d.exec("PRAGMA foreign_keys = ON;");
    },
  },
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
