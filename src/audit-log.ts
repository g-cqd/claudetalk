/**
 * Audit log: writes every MCP tool call AND every JSON-RPC protocol message
 * (initialize, tools/list, notifications, etc.) to the `tool_calls` table.
 *
 * Writes are queued in memory and flushed in batches every 200 ms inside a
 * single SQLite transaction. This keeps hot-path latency at ~µs (just push
 * to an array) and collapses N writes into one writer-lock acquisition,
 * which is what eliminates the "MCP server frozen on contended writes"
 * symptom under multi-Claude load.
 *
 * Two integration points:
 *   - instrumentServer(server, pseudonym): wraps registerTool() so every
 *     tool handler is logged with kind='tool'.
 *   - instrumentTransport(transport, pseudonym): wraps the stdio transport
 *     to log every inbound request/notification (kind='request' or
 *     'notification', direction='in') and outbound response/notification
 *     (kind='response' or 'notification', direction='out').
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { db } from "./db.ts";
import { ErrorCode, toolError } from "./errors.ts";
import { acquire } from "./rate-limit.ts";

const ARGS_MAX = 1_000;
const RESULT_MAX = 2_000;
const FLUSH_INTERVAL_MS = 200;

// ---------------- store types ----------------

export interface ToolCallRow {
  id: number;
  pseudonym: string;
  kind: string; // 'tool' | 'request' | 'response' | 'notification'
  direction: string; // 'in' | 'out'
  tool: string; // tool name OR jsonrpc method
  jrpc_id: number | null;
  args_json: string | null;
  result_summary: string | null;
  is_error: number;
  error: string | null;
  started_at: number;
  duration_ms: number;
}

interface ToolCallInsert {
  pseudonym: string;
  kind: "tool" | "request" | "response" | "notification";
  direction: "in" | "out";
  tool: string;
  jrpc_id: number | null;
  args_json: string | null;
  result_summary: string | null;
  is_error: boolean;
  error: string | null;
  started_at: number;
  duration_ms: number;
}

export interface ToolCallQuery {
  pseudonym?: string;
  tool?: string;
  kind?: string;
  sinceId?: number;
  limit?: number;
}

// ---------------- batched writer ----------------

const queue: ToolCallInsert[] = [];
let flushTimer: ReturnType<typeof setInterval> | null = null;

function ensureFlusher(): void {
  if (flushTimer !== null) return;
  flushTimer = setInterval(flushNow, FLUSH_INTERVAL_MS);
  // Don't keep the event loop alive just for the flusher.
  if (typeof flushTimer === "object" && "unref" in flushTimer) {
    (flushTimer as { unref: () => void }).unref();
  }
}

/** Drain the queue inside a single transaction. Idempotent and safe to call
 *  on shutdown. Errors during flush are swallowed — the log must never
 *  break a tool call. */
export function flushNow(): void {
  if (queue.length === 0) return;
  const batch = queue.splice(0, queue.length);
  try {
    const d = db();
    const stmt = d.prepare(
      `INSERT INTO tool_calls
        (pseudonym, kind, direction, tool, jrpc_id, args_json, result_summary,
         is_error, error, started_at, duration_ms)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    );
    const tx = d.transaction((rows: ToolCallInsert[]) => {
      for (const row of rows) {
        stmt.run(
          row.pseudonym,
          row.kind,
          row.direction,
          row.tool,
          row.jrpc_id,
          row.args_json,
          row.result_summary,
          row.is_error ? 1 : 0,
          row.error,
          row.started_at,
          row.duration_ms,
        );
      }
    });
    tx(batch);
  } catch {
    // Under contention the whole batch may be lost; that's acceptable.
    // Re-queueing risks runaway growth, so we drop and move on.
  }
}

function enqueue(row: ToolCallInsert): void {
  queue.push(row);
  ensureFlusher();
}

/** Enqueue with the new defaults (kind=tool, direction=in) — used by the
 *  rate-limit short-circuit + handler-wrap defer path. */
function enqueueRow(row: {
  pseudonym: string;
  tool: string;
  args_json: string | null;
  result_summary: string | null;
  is_error: boolean;
  error: string | null;
  started_at: number;
  duration_ms: number;
}): void {
  enqueue({ ...row, kind: "tool", direction: "in", jrpc_id: null });
}

/** Test helper: drop the timer + any pending rows so suites stay hermetic. */
export function _resetAuditLogForTests(): void {
  if (flushTimer !== null) {
    clearInterval(flushTimer);
    flushTimer = null;
  }
  queue.length = 0;
}

// ---------------- queries ----------------

export function listToolCalls(q: ToolCallQuery = {}): ToolCallRow[] {
  const where: string[] = [];
  const params: any[] = [];
  if (q.pseudonym) {
    where.push("pseudonym = ?");
    params.push(q.pseudonym);
  }
  if (q.tool) {
    where.push("tool = ?");
    params.push(q.tool);
  }
  if (q.kind) {
    where.push("kind = ?");
    params.push(q.kind);
  }
  if (q.sinceId !== undefined) {
    where.push("id > ?");
    params.push(q.sinceId);
  }
  const whereSql = where.length > 0 ? `WHERE ${where.join(" AND ")}` : "";
  const limit = Math.max(1, Math.min(q.limit ?? 100, 1000));
  params.push(limit);
  return db()
    .query<ToolCallRow, any[]>(
      `SELECT id, pseudonym, kind, direction, tool, jrpc_id, args_json,
              result_summary, is_error, error, started_at, duration_ms
       FROM tool_calls ${whereSql}
       ORDER BY id DESC LIMIT ?`,
    )
    .all(...params)
    .reverse();
}

/** Sync insert path for tests that need to assert without waiting on a
 *  batch flush. Production code should always go through the queue. */
export function insertToolCall(row: {
  pseudonym: string;
  tool: string;
  args_json: string | null;
  result_summary: string | null;
  is_error: boolean;
  error: string | null;
  started_at: number;
  duration_ms: number;
}): void {
  enqueue({
    ...row,
    kind: "tool",
    direction: "in",
    jrpc_id: null,
  });
  flushNow();
}

// ---------------- helpers ----------------

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max - 3)}...`;
}

function safeStringify(v: unknown): string | null {
  try {
    return JSON.stringify(v ?? null);
  } catch {
    return null;
  }
}

function summariseResult(result: unknown): string | null {
  if (result === null || result === undefined) return null;
  const r = result as { content?: Array<{ type: string; text?: string }> };
  if (Array.isArray(r.content)) {
    const text = r.content
      .map((c) => (c?.type === "text" && typeof c.text === "string" ? c.text : ""))
      .join("\n")
      .trim();
    if (text.length > 0) return truncate(text, RESULT_MAX);
  }
  const json = safeStringify(result);
  return json === null ? null : truncate(json, RESULT_MAX);
}

// ---------------- instrumentation ----------------

/** Wrap registerTool so every tool handler is logged with kind='tool'. */
export function instrumentServer(server: McpServer, pseudonym: string): McpServer {
  const original = server.registerTool.bind(server);
  const patched: any = (
    name: string,
    schema: any,
    handler: (args: any, extra?: any) => any,
  ) => {
    const wrapped = async (args: any, extra?: any) => {
      const started_at = Date.now();
      const j = safeStringify(args);
      const args_json = j === null ? null : truncate(j, ARGS_MAX);

      // Phase 5.2: per-(pseudonym, tool) token bucket. If this caller is
      // hammering the same tool in a tight loop, drop it with a structured
      // RATE_LIMITED error instead of letting it overwhelm SQLite.
      const decision = acquire(pseudonym, name);
      if (!decision.ok) {
        const result = toolError(
          `Rate limit exceeded for tool '${name}' (30 calls per 10s). Retry in ~${decision.retry_after_seconds}s.`,
          ErrorCode.RATE_LIMITED,
        );
        enqueueRow({
          pseudonym,
          tool: name,
          args_json,
          result_summary: `[${ErrorCode.RATE_LIMITED}] retry in ${decision.retry_after_seconds}s`,
          is_error: true,
          error: ErrorCode.RATE_LIMITED,
          started_at,
          duration_ms: Date.now() - started_at,
        });
        return result;
      }
      try {
        const result = await handler(args, extra);
        enqueue({
          pseudonym,
          kind: "tool",
          direction: "in",
          tool: name,
          jrpc_id: null,
          args_json,
          result_summary: summariseResult(result),
          is_error: !!(result as any)?.isError,
          error: null,
          started_at,
          duration_ms: Date.now() - started_at,
        });
        return result;
      } catch (e) {
        const err = e instanceof Error ? e.message : String(e);
        enqueue({
          pseudonym,
          kind: "tool",
          direction: "in",
          tool: name,
          jrpc_id: null,
          args_json,
          result_summary: null,
          is_error: true,
          error: truncate(err, RESULT_MAX),
          started_at,
          duration_ms: Date.now() - started_at,
        });
        throw e;
      }
    };
    return original(name, schema, wrapped);
  };
  (server as any).registerTool = patched;
  return server;
}

/** Wrap the stdio transport to log every inbound/outbound JSON-RPC message.
 *  Must be called AFTER server.connect(transport) so we override the
 *  Protocol-assigned `onmessage` and the transport's `send`. */
export function instrumentTransport(
  transport: StdioServerTransport,
  pseudonym: string,
): void {
  const originalOnMessage = transport.onmessage;
  transport.onmessage = (msg: any) => {
    try {
      const isResponse = "result" in msg || "error" in msg;
      const isNotification = msg && msg.method != null && msg.id == null;
      const kind = isResponse ? "response" : isNotification ? "notification" : "request";
      const method = msg?.method ?? "<response>";
      // Skip 'tools/call' inbound: it's already logged in detail by
      // instrumentServer with the actual handler outcome.
      if (kind === "request" && method === "tools/call") {
        originalOnMessage?.(msg);
        return;
      }
      const payload = safeStringify(isResponse ? (msg.result ?? msg.error) : msg.params);
      enqueue({
        pseudonym,
        kind,
        direction: "in",
        tool: method,
        jrpc_id: typeof msg?.id === "number" ? msg.id : null,
        args_json: payload === null ? null : truncate(payload, ARGS_MAX),
        result_summary: null,
        is_error: !!msg?.error,
        error: msg?.error ? truncate(safeStringify(msg.error) ?? "", RESULT_MAX) : null,
        started_at: Date.now(),
        duration_ms: 0,
      });
    } catch {}
    originalOnMessage?.(msg);
  };

  const originalSend = transport.send.bind(transport);
  transport.send = async (msg: any) => {
    try {
      const isResponse = "result" in msg || "error" in msg;
      const isNotification = msg && msg.method != null && msg.id == null;
      const kind = isResponse ? "response" : isNotification ? "notification" : "request";
      const method = msg?.method ?? "<response>";
      // Skip outbound responses to 'tools/call' (already logged by handler wrap).
      // We don't know the request method from the response alone, but we can
      // safely log; duplicates are tolerable for debugging.
      const payload = safeStringify(isResponse ? (msg.result ?? msg.error) : msg.params);
      enqueue({
        pseudonym,
        kind,
        direction: "out",
        tool: method,
        jrpc_id: typeof msg?.id === "number" ? msg.id : null,
        args_json: payload === null ? null : truncate(payload, ARGS_MAX),
        result_summary: null,
        is_error: !!msg?.error,
        error: msg?.error ? truncate(safeStringify(msg.error) ?? "", RESULT_MAX) : null,
        started_at: Date.now(),
        duration_ms: 0,
      });
    } catch {}
    return originalSend(msg);
  };
}
