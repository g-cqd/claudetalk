/**
 * `claudetalk replay` — re-run a recorded tool sequence against a fresh,
 * isolated CLAUDETALK_HOME. Useful for bug reproduction: extract a
 * pseudonym's audit-log trail from a buggy session, replay it on a clean
 * DB, diff the new responses against what was originally recorded.
 *
 * Single-pseudonym, single-process by design. Spawns one MCP server (the
 * same `src/server.ts` users hit) in a subprocess so replay goes through
 * the exact same JSON-RPC code path as a live session.
 */
import { spawn, type Subprocess } from "bun";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { db } from "./db.ts";
import { type ToolCallRow } from "./audit-log.ts";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const SERVER_ENTRY = resolve(HERE, "server.ts");

export interface ReplayOptions {
  pseudonym: string;
  sinceId?: number;
  untilId?: number;
  limit?: number;
  home?: string;
  keep?: boolean;
}

interface ReplayDiff {
  id: number;
  tool: string;
  originalSummary: string | null;
  replayedSummary: string | null;
  match: boolean;
  error: string | null;
}

export interface ReplayReport {
  pseudonym: string;
  homeDir: string;
  startedAt: number;
  durationMs: number;
  rowsConsidered: number;
  rowsReplayed: number;
  matches: number;
  mismatches: number;
  errors: number;
  diffs: ReplayDiff[];
}

/** What we actually use off the reader; both DOM and node:stream/web variants
 *  satisfy this shape, and explicit annotation lets TS pick a single one. */
interface MinimalReader {
  read(): Promise<{ done: boolean; value: string | undefined }>;
}

interface ServerHandle {
  proc: Subprocess<"pipe", "pipe", "inherit">;
  reader: MinimalReader;
  buffer: string;
  nextId: number;
}

function selectRows(opts: ReplayOptions): ToolCallRow[] {
  const where: string[] = ["pseudonym = ?", "kind = 'tool'", "direction = 'in'"];
  const params: Array<string | number> = [opts.pseudonym];
  if (opts.sinceId !== undefined) {
    where.push("id > ?");
    params.push(opts.sinceId);
  }
  if (opts.untilId !== undefined) {
    where.push("id <= ?");
    params.push(opts.untilId);
  }
  const limit = Math.max(1, Math.min(opts.limit ?? 200, 5000));
  params.push(limit);
  return db()
    .query<ToolCallRow, Array<string | number>>(
      `SELECT id, pseudonym, kind, direction, tool, jrpc_id, args_json,
              result_summary, is_error, error, started_at, duration_ms
       FROM tool_calls
       WHERE ${where.join(" AND ")}
       ORDER BY id ASC
       LIMIT ?`,
    )
    .all(...params);
}

async function spawnServer(home: string, projectDir: string): Promise<ServerHandle> {
  const proc = spawn({
    cmd: ["bun", "run", SERVER_ENTRY],
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, CLAUDETALK_HOME: home, CLAUDE_PROJECT_DIR: projectDir },
  });
  const reader = proc.stdout.pipeThrough(new TextDecoderStream()).getReader();
  return { proc, reader, buffer: "", nextId: 1 };
}

async function rpc(h: ServerHandle, method: string, params: unknown): Promise<any> {
  const id = h.nextId++;
  const msg = `${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`;
  h.proc.stdin.write(msg);
  await h.proc.stdin.flush?.();
  while (true) {
    const nl = h.buffer.indexOf("\n");
    if (nl < 0) {
      const { value, done } = await h.reader.read();
      if (done) throw new Error("server closed stdout");
      h.buffer += value;
      continue;
    }
    const line = h.buffer.slice(0, nl).trim();
    h.buffer = h.buffer.slice(nl + 1);
    if (!line) continue;
    const parsed = JSON.parse(line);
    if (parsed.id === id) return parsed;
  }
}

async function notify(h: ServerHandle, method: string): Promise<void> {
  h.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method })}\n`);
  await h.proc.stdin.flush?.();
}

function summariseResult(payload: any): string | null {
  const content = payload?.result?.content;
  if (!Array.isArray(content) || content.length === 0) return null;
  const first = content[0];
  if (typeof first?.text === "string") return first.text;
  return JSON.stringify(first);
}

/** Run the replay. Caller is responsible for calling db() beforehand. */
export async function runReplay(opts: ReplayOptions): Promise<ReplayReport> {
  const rows = selectRows(opts);
  const home = opts.home ?? mkdtempSync(join(tmpdir(), "claudetalk-replay-"));
  const projectDir = `/tmp/claudetalk-replay-${opts.pseudonym}`;
  const startedAt = Date.now();
  const report: ReplayReport = {
    pseudonym: opts.pseudonym,
    homeDir: home,
    startedAt,
    durationMs: 0,
    rowsConsidered: rows.length,
    rowsReplayed: 0,
    matches: 0,
    mismatches: 0,
    errors: 0,
    diffs: [],
  };

  const h = await spawnServer(home, projectDir);
  try {
    await rpc(h, "initialize", {
      protocolVersion: "2024-11-05",
      capabilities: { roots: { listChanged: false } },
      clientInfo: { name: "claudetalk-replay", version: "0" },
    });
    await notify(h, "notifications/initialized");

    for (const row of rows) {
      let replayed: string | null = null;
      let err: string | null = null;
      // Audit-log truncates args_json at ARGS_MAX (1000) chars and marks
      // with a `...` suffix. We can't reconstruct the original payload,
      // so skip these and surface a clear marker rather than crashing
      // with a JSON parse error.
      if (row.args_json && row.args_json.endsWith("...")) {
        err = "args_json was truncated by audit log; cannot replay";
      } else {
        try {
          const args = row.args_json ? JSON.parse(row.args_json) : {};
          const resp = await rpc(h, "tools/call", { name: row.tool, arguments: args });
          if (resp.error) {
            err = JSON.stringify(resp.error);
          } else {
            replayed = summariseResult(resp);
          }
        } catch (e) {
          err = e instanceof Error ? e.message : String(e);
        }
      }
      const match = err === null && replayed === row.result_summary;
      report.diffs.push({
        id: row.id,
        tool: row.tool,
        originalSummary: row.result_summary,
        replayedSummary: replayed,
        match,
        error: err,
      });
      report.rowsReplayed++;
      if (err) report.errors++;
      else if (match) report.matches++;
      else report.mismatches++;
    }
  } finally {
    try {
      h.proc.kill();
    } catch {}
    if (!opts.keep && opts.home === undefined) {
      try {
        rmSync(home, { recursive: true, force: true });
      } catch {}
    }
  }
  report.durationMs = Date.now() - startedAt;
  return report;
}

export function formatReplayReport(r: ReplayReport, verbose: boolean): string {
  const lines: string[] = [];
  lines.push(`Replay of '${r.pseudonym}' — ${r.rowsReplayed}/${r.rowsConsidered} tool calls`);
  lines.push(`  home: ${r.homeDir}`);
  lines.push(`  match=${r.matches}  mismatch=${r.mismatches}  error=${r.errors}`);
  lines.push(`  took ${r.durationMs}ms`);
  if (verbose || r.mismatches > 0 || r.errors > 0) {
    lines.push("");
    for (const d of r.diffs) {
      if (d.match && !verbose) continue;
      const tag = d.error ? "ERR " : d.match ? "OK  " : "DIFF";
      lines.push(`  [${tag}] #${d.id} ${d.tool}`);
      if (d.error) {
        lines.push(`        error: ${d.error}`);
      } else if (!d.match) {
        lines.push(`        original: ${truncate(d.originalSummary)}`);
        lines.push(`        replayed: ${truncate(d.replayedSummary)}`);
      }
    }
  }
  return lines.join("\n");
}

function truncate(s: string | null, max = 160): string {
  if (s === null) return "(null)";
  if (s.length <= max) return s;
  return `${s.slice(0, max)}…`;
}
