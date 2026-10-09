/**
 * T1: load a pre-v3 (path-derived id, INTEGER PK message id) database
 * and verify migrate() lifts it cleanly to current schema with all
 * existing data preserved. This was the originally-uncovered case that
 * could have caught both v0.5.0 regressions (last_notified_message_id
 * + exportChat ordering) — instead they shipped silently because every
 * test ran against fresh-migrated DBs.
 */
import { Database } from "bun:sqlite";
import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { currentSchemaVersion, migrate, targetSchemaVersion } from "../../src/migrations.ts";

let dir: string;
let dbPath: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "claudetalk-migr-"));
  dbPath = join(dir, "db.sqlite");
});

afterEach(() => {
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {}
});

function seedV1(d: Database): void {
  // Hand-roll the v1 schema (matches the body of migrations[0]).
  d.exec(`
    CREATE TABLE instances (
      pseudonym TEXT PRIMARY KEY, path TEXT NOT NULL,
      first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, pid INTEGER,
      last_notified_ask_id INTEGER NOT NULL DEFAULT 0,
      last_notified_mention_id INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE chats (
      id TEXT PRIMARY KEY,
      kind TEXT NOT NULL CHECK (kind IN ('direct','group')),
      title TEXT, created_at INTEGER NOT NULL
    );
    CREATE TABLE chat_members (
      chat_id TEXT NOT NULL, pseudonym TEXT NOT NULL,
      joined_at INTEGER NOT NULL,
      last_read_message_id INTEGER NOT NULL DEFAULT 0,
      last_notified_message_id INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (chat_id, pseudonym),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
    CREATE TABLE messages (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      chat_id TEXT NOT NULL, from_pseudonym TEXT NOT NULL,
      body TEXT NOT NULL, created_at INTEGER NOT NULL,
      parent_id INTEGER,
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
    CREATE TABLE asks (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      from_pseudonym TEXT NOT NULL, to_pseudonym TEXT NOT NULL,
      body TEXT NOT NULL, created_at INTEGER NOT NULL,
      answered_at INTEGER, answer_body TEXT
    );
    CREATE TABLE personal_nicknames (
      viewer TEXT NOT NULL, target TEXT NOT NULL,
      nickname TEXT NOT NULL, set_at INTEGER NOT NULL,
      PRIMARY KEY (viewer, target)
    );
    CREATE TABLE group_nickname_votes (
      chat_id TEXT NOT NULL, target TEXT NOT NULL,
      voter TEXT NOT NULL, nickname TEXT NOT NULL,
      voted_at INTEGER NOT NULL,
      PRIMARY KEY (chat_id, target, voter),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
    CREATE TABLE tool_calls (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      pseudonym TEXT NOT NULL, tool TEXT NOT NULL,
      args_json TEXT, result_summary TEXT,
      is_error INTEGER NOT NULL DEFAULT 0, error TEXT,
      started_at INTEGER NOT NULL, duration_ms INTEGER NOT NULL,
      kind TEXT NOT NULL DEFAULT 'tool',
      direction TEXT NOT NULL DEFAULT 'in',
      jrpc_id INTEGER
    );
    CREATE TABLE message_reactions (
      message_id INTEGER NOT NULL, reactor TEXT NOT NULL,
      reaction TEXT NOT NULL, created_at INTEGER NOT NULL,
      PRIMARY KEY (message_id, reactor),
      FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
    );
    CREATE TABLE message_mentions (
      message_id INTEGER NOT NULL, target TEXT NOT NULL,
      PRIMARY KEY (message_id, target),
      FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
    );
    CREATE TABLE instance_status (
      pseudonym TEXT PRIMARY KEY, status TEXT NOT NULL,
      emoji TEXT, updated_at INTEGER NOT NULL,
      FOREIGN KEY (pseudonym) REFERENCES instances(pseudonym) ON DELETE CASCADE
    );
    CREATE TABLE chat_preferences (
      viewer TEXT NOT NULL, chat_id TEXT NOT NULL,
      muted INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
      PRIMARY KEY (viewer, chat_id),
      FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE
    );
  `);
  d.exec("PRAGMA user_version = 1");
  // Insert some pre-v3 data: integer message id, last_read_message_id cursor.
  d.run("INSERT INTO instances (pseudonym, path, first_seen, last_seen, pid) VALUES ('Alice', '/a', 1, 1, 100)");
  d.run("INSERT INTO chats (id, kind, title, created_at) VALUES ('group:x', 'group', 'X', 1)");
  d.run("INSERT INTO chat_members (chat_id, pseudonym, joined_at, last_read_message_id) VALUES ('group:x', 'Alice', 1, 0)");
  d.run("INSERT INTO messages (chat_id, from_pseudonym, body, created_at) VALUES ('group:x', 'Alice', 'first', 100)");
  d.run("INSERT INTO messages (chat_id, from_pseudonym, body, created_at) VALUES ('group:x', 'Alice', 'second', 200)");
  d.run("INSERT INTO message_reactions (message_id, reactor, reaction, created_at) VALUES (1, 'Alice', '👍', 150)");
}

test("v1 → target migration preserves data and lifts ids to TEXT/seq", () => {
  const d = new Database(dbPath);
  d.exec("PRAGMA foreign_keys = ON;");
  seedV1(d);
  expect(currentSchemaVersion(d)).toBe(1);

  migrate(d);

  // Migrated to latest target.
  expect(currentSchemaVersion(d)).toBe(targetSchemaVersion());

  // Messages now have TEXT id + INTEGER seq; legacy rows kept their
  // numeric-as-text id with seq=int id.
  const rows = d
    .query<{ id: string; seq: number; body: string }, []>(
      "SELECT id, seq, body FROM messages ORDER BY seq ASC",
    )
    .all();
  expect(rows.length).toBe(2);
  expect(rows[0]!.id).toBe("1");
  expect(rows[0]!.seq).toBe(1);
  expect(rows[0]!.body).toBe("first");
  expect(rows[1]!.id).toBe("2");
  expect(rows[1]!.seq).toBe(2);

  // chat_members.last_read_message_id was renamed to _seq.
  const m = d.query<{ last_read_message_seq: number }, []>(
    "SELECT last_read_message_seq FROM chat_members WHERE pseudonym = 'Alice'",
  ).get();
  expect(m).not.toBeNull();
  expect(m!.last_read_message_seq).toBe(0);

  // message_reactions.message_id is now TEXT — legacy "1" preserved.
  const r = d.query<{ message_id: string; reaction: string }, []>(
    "SELECT message_id, reaction FROM message_reactions",
  ).all();
  expect(r.length).toBe(1);
  expect(r[0]!.message_id).toBe("1");

  // instances gained machine_id (NULL for legacy rows) + public_key.
  const i = d.query<{ machine_id: string | null; public_key: string | null }, []>(
    "SELECT machine_id, public_key FROM instances WHERE pseudonym = 'Alice'",
  ).get();
  expect(i!.machine_id).toBeNull();
  expect(i!.public_key).toBeNull();

  // messages.signature column exists, NULL for legacy rows.
  const sigRow = d.query<{ signature: string | null }, []>(
    "SELECT signature FROM messages WHERE seq = 1",
  ).get();
  expect(sigRow!.signature).toBeNull();
});
