/**
 * Per-instance status: a short string ("busy with Wave 8 retrofit",
 * "available", "blocked on lock") plus optional leading emoji. Visible in
 * `discover` output so peers can coordinate without sending a message.
 *
 * Free-form (max 80 chars) — caller's responsibility to keep it useful.
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import { _now, db, touchInstance } from "./db.ts";
import { ErrorCode, toolError, toolText } from "./errors.ts";
import { dynamicIdentity } from "./identity-context.ts";

const STATUS_MAX = 80;

export interface InstanceStatusRow {
  pseudonym: string;
  status: string;
  emoji: string | null;
  updated_at: number;
}

export function setStatus(pseudonym: string, status: string, emoji: string | null): void {
  db().run(
    `INSERT INTO instance_status (pseudonym, status, emoji, updated_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(pseudonym) DO UPDATE SET
       status = excluded.status,
       emoji = excluded.emoji,
       updated_at = excluded.updated_at`,
    [pseudonym, status, emoji, _now()],
  );
}

export function clearStatus(pseudonym: string): boolean {
  const res = db().run("DELETE FROM instance_status WHERE pseudonym = ?", [pseudonym]);
  return res.changes > 0;
}

export function getStatus(pseudonym: string): InstanceStatusRow | null {
  return (
    db()
      .query<InstanceStatusRow, [string]>(
        "SELECT pseudonym, status, emoji, updated_at FROM instance_status WHERE pseudonym = ?",
      )
      .get(pseudonym) ?? null
  );
}

/** Render a status row inline: "🟢 busy with X" or "(no status set)". */
export function fmtStatus(row: InstanceStatusRow | null): string {
  if (row === null) return "";
  return row.emoji ? `${row.emoji} ${row.status}` : row.status;
}

// text/error helpers come from src/errors.ts (Phase 5.4 — codes).
const text = (s: string) => toolText(s);
const error = (s: string, code: ErrorCode = ErrorCode.UNSPECIFIED) => toolError(s, code);

export function registerStatusTools(server: McpServer, staticMe: Identity): void {
  const me = dynamicIdentity(staticMe);
  server.registerTool(
    "status_set",
    {
      title: "Set your visible status",
      description:
        "Short freeform status (≤80 chars) + optional leading emoji, visible to peers via " +
        "'discover'. Useful for coordination signals like 'busy: Wave 8 retrofit', 'available', " +
        "'blocked on lock'. Pass empty status to clear (or call 'status_clear').",
      inputSchema: {
        status: z
          .string()
          .describe("Status text. ≤80 chars. Pass empty to clear."),
        emoji: z
          .string()
          .max(8)
          .optional()
          .describe("Optional leading emoji or short symbol (≤8 chars). Rendered before status."),
      },
    },
    async ({ status, emoji }) => {
      touchInstance(me.pseudonym);
      const trimmed = status.trim();
      if (trimmed.length === 0) {
        const cleared = clearStatus(me.pseudonym);
        return text(
          cleared ? "Cleared your status." : "No status was set.",
        );
      }
      if (trimmed.length > STATUS_MAX) {
        return error(`Status too long (${trimmed.length} chars). Max ${STATUS_MAX}.`);
      }
      setStatus(me.pseudonym, trimmed, emoji?.trim() || null);
      return text(`Set status: ${emoji ? `${emoji} ` : ""}${trimmed}`);
    },
  );

  server.registerTool(
    "status_clear",
    {
      title: "Clear your visible status",
      description: "Remove any status text you previously set with 'status_set'.",
      inputSchema: {},
    },
    async () => {
      touchInstance(me.pseudonym);
      const cleared = clearStatus(me.pseudonym);
      return text(cleared ? "Cleared your status." : "No status was set.");
    },
  );
}
