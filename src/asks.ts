/**
 * One-shot ask/answer flow. Persistent inbox primitive: one Claude asks
 * another a question; the recipient sees it in `inbox` and replies via
 * `answer`. Extracted from db.ts to keep that file under its
 * file-size budget.
 */
import { db, type AskRow } from "./db.ts";

function now(): number {
  return Date.now();
}

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
  db().run("UPDATE asks SET answered_at = ?, answer_body = ? WHERE id = ?", [
    now(),
    answer,
    id,
  ]);
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

export function listAnsweredAsksFrom(from: string, sinceSeenId: number): AskRow[] {
  return db()
    .query<AskRow, [string, number]>(
      `SELECT id, from_pseudonym, to_pseudonym, body, created_at, answered_at, answer_body
       FROM asks WHERE from_pseudonym = ? AND answered_at IS NOT NULL AND id > ?
       ORDER BY id ASC`,
    )
    .all(from, sinceSeenId);
}
