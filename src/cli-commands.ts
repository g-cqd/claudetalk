/**
 * CLI subcommands that don't touch ~/.claude.json (the installer keeps
 * those in bin/cli.ts). Extracted to keep bin/cli.ts under the file-size
 * budget as Phase 4 adds new commands.
 */
import { db, listInstances } from "./db.ts";
import { listToolCalls } from "./audit-log.ts";
import {
  currentSchemaVersion,
  targetSchemaVersion,
} from "./migrations.ts";
import { fmtInstance } from "./format.ts";

interface DoctorReport {
  bun_path: string;
  server_entry: string;
  hook_entry: string;
  schema_version: number;
  schema_target: number;
  schema_up_to_date: boolean;
  install_state: Array<{
    label: string;
    path: string;
    exists: boolean;
    mcp_registered: boolean;
    hook_count: number;
  }>;
  audit: {
    total_rows: number;
    errors_last_hour: number;
    rate_limited_last_hour: number;
  };
  active_instances_last_hour: number;
}

export function buildDoctorReport(
  bunBin: string,
  serverEntry: string,
  hookEntry: string,
  installPaths: Array<{ label: string; path: string }>,
  readJson: (p: string) => any,
  existsSync: (p: string) => boolean,
): DoctorReport {
  const d = db();
  const installState = installPaths.map(({ label, path }) => {
    const exists = existsSync(path);
    const j = exists ? readJson(path) : {};
    const mcpRegistered = !!j?.mcpServers?.claudetalk;
    let hookCount = 0;
    const hookCmd = `${bunBin} run ${hookEntry}`;
    if (j?.hooks) {
      for (const ev of Object.keys(j.hooks)) {
        for (const block of j.hooks[ev] ?? []) {
          for (const h of block.hooks ?? []) {
            if (h?.command === hookCmd) hookCount++;
          }
        }
      }
    }
    return { label, path, exists, mcp_registered: mcpRegistered, hook_count: hookCount };
  });

  const totalRows = d.query<{ c: number }, []>(
    "SELECT COUNT(*) AS c FROM tool_calls",
  ).get()?.c ?? 0;
  const errorsLastHour = d.query<{ c: number }, []>(
    `SELECT COUNT(*) AS c FROM tool_calls
     WHERE is_error = 1
       AND started_at >= (strftime('%s','now') - 3600) * 1000`,
  ).get()?.c ?? 0;
  const rateLimitedLastHour = d.query<{ c: number }, []>(
    `SELECT COUNT(*) AS c FROM tool_calls
     WHERE error = 'rate_limited'
       AND started_at >= (strftime('%s','now') - 3600) * 1000`,
  ).get()?.c ?? 0;

  return {
    bun_path: bunBin,
    server_entry: serverEntry,
    hook_entry: hookEntry,
    schema_version: currentSchemaVersion(d),
    schema_target: targetSchemaVersion(),
    schema_up_to_date: currentSchemaVersion(d) >= targetSchemaVersion(),
    install_state: installState,
    audit: {
      total_rows: totalRows,
      errors_last_hour: errorsLastHour,
      rate_limited_last_hour: rateLimitedLastHour,
    },
    active_instances_last_hour: listInstances(60 * 60 * 1000).length,
  };
}

export function formatDoctorReport(r: DoctorReport): string {
  const lines: string[] = [];
  lines.push("ClaudeTalk doctor");
  lines.push(`  bun:    ${r.bun_path}`);
  lines.push(`  server: ${r.server_entry}`);
  lines.push(`  hook:   ${r.hook_entry}`);
  lines.push("");
  lines.push(
    `  schema: v${r.schema_version} / target v${r.schema_target}  ${r.schema_up_to_date ? "✔ up to date" : "✗ MIGRATION PENDING"}`,
  );
  lines.push("");
  for (const s of r.install_state) {
    const mark = s.exists ? "x" : " ";
    const mcp = s.mcp_registered ? "✔ MCP" : "  ";
    const hooks = s.hook_count > 0 ? `✔ ${s.hook_count} hooks` : "";
    lines.push(
      `  [${mark}] ${s.label.padEnd(22)} ${s.path}   ${mcp} ${hooks}`,
    );
  }
  lines.push("");
  lines.push(`  audit log:   ${r.audit.total_rows} total rows`);
  lines.push(`               ${r.audit.errors_last_hour} errors in last hour`);
  lines.push(`               ${r.audit.rate_limited_last_hour} rate-limited in last hour`);
  lines.push("");
  lines.push(`Active instances in last hour (${r.active_instances_last_hour}):`);
  for (const i of listInstances(60 * 60 * 1000)) lines.push("  " + fmtInstance(i));
  return lines.join("\n");
}

// ---------------- gc ----------------

export interface GcResult {
  pruned_tool_calls: number;
  retained_tool_calls: number;
  vacuumed: boolean;
}

export function runGc(opts: { olderThanDays: number; vacuum: boolean }): GcResult {
  const d = db();
  const cutoff = Date.now() - opts.olderThanDays * 24 * 60 * 60 * 1000;
  const res = d.run("DELETE FROM tool_calls WHERE started_at < ?", [cutoff]);
  const retained = d.query<{ c: number }, []>(
    "SELECT COUNT(*) AS c FROM tool_calls",
  ).get()?.c ?? 0;
  if (opts.vacuum) d.exec("VACUUM;");
  return {
    pruned_tool_calls: res.changes,
    retained_tool_calls: retained,
    vacuumed: opts.vacuum,
  };
}

// ---------------- export ----------------

interface ExportMessage {
  id: number;
  from_pseudonym: string;
  body: string;
  created_at: number;
  parent_id: number | null;
}

export function exportChat(
  chatId: string,
  format: "md" | "json",
): { ok: boolean; output: string } {
  const d = db();
  const chat = d
    .query<{ id: string; kind: string; title: string | null; created_at: number }, [string]>(
      "SELECT id, kind, title, created_at FROM chats WHERE id = ?",
    )
    .get(chatId);
  if (!chat) return { ok: false, output: `unknown chat_id ${chatId}` };
  const msgs = d
    .query<ExportMessage, [string]>(
      `SELECT id, from_pseudonym, body, created_at, parent_id
       FROM messages WHERE chat_id = ? ORDER BY id ASC`,
    )
    .all(chatId);
  if (format === "json") {
    return {
      ok: true,
      output: JSON.stringify(
        { chat, messages: msgs, exported_at: Date.now() },
        null,
        2,
      ),
    };
  }
  // markdown
  const lines: string[] = [];
  lines.push(`# ${chat.title ?? chat.id}`);
  lines.push("");
  lines.push(
    `Kind: ${chat.kind} · Created: ${new Date(chat.created_at).toISOString()} · ${msgs.length} messages`,
  );
  lines.push("");
  for (const m of msgs) {
    const replyMark = m.parent_id ? ` _(replying to #${m.parent_id})_` : "";
    lines.push(`## #${m.id} — ${m.from_pseudonym} · ${new Date(m.created_at).toISOString()}${replyMark}`);
    lines.push("");
    lines.push(m.body);
    lines.push("");
  }
  return { ok: true, output: lines.join("\n") };
}

// ---------------- metrics ----------------

export interface MetricsReport {
  window_hours: number;
  per_tool: Array<{
    tool: string;
    calls: number;
    errors: number;
    p50_ms: number;
    p95_ms: number;
    p99_ms: number;
    max_ms: number;
  }>;
  per_pseudonym: Array<{ pseudonym: string; calls: number; errors: number }>;
  rate_limited: number;
  hook_dedup_estimate: {
    chat_notifications_emitted: number;
    chat_messages_total: number;
    suppression_ratio_estimate: number;
  };
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length));
  return sorted[idx]!;
}

export function buildMetrics(opts: { windowHours: number }): MetricsReport {
  const d = db();
  const cutoff = Date.now() - opts.windowHours * 60 * 60 * 1000;

  const rows = listToolCalls({ limit: 1000 }).filter(
    (r) => r.kind === "tool" && r.started_at >= cutoff,
  );
  const byTool = new Map<string, number[]>();
  const errsByTool = new Map<string, number>();
  for (const r of rows) {
    const arr = byTool.get(r.tool) ?? [];
    arr.push(r.duration_ms);
    byTool.set(r.tool, arr);
    if (r.is_error) errsByTool.set(r.tool, (errsByTool.get(r.tool) ?? 0) + 1);
  }
  const perTool = [...byTool.entries()]
    .map(([tool, durs]) => {
      const sorted = durs.slice().sort((a, b) => a - b);
      return {
        tool,
        calls: durs.length,
        errors: errsByTool.get(tool) ?? 0,
        p50_ms: percentile(sorted, 50),
        p95_ms: percentile(sorted, 95),
        p99_ms: percentile(sorted, 99),
        max_ms: sorted[sorted.length - 1] ?? 0,
      };
    })
    .sort((a, b) => b.calls - a.calls);

  const byPseudonym = new Map<string, { calls: number; errors: number }>();
  for (const r of rows) {
    const ent = byPseudonym.get(r.pseudonym) ?? { calls: 0, errors: 0 };
    ent.calls++;
    if (r.is_error) ent.errors++;
    byPseudonym.set(r.pseudonym, ent);
  }
  const perPseudonym = [...byPseudonym.entries()]
    .map(([pseudonym, v]) => ({ pseudonym, ...v }))
    .sort((a, b) => b.calls - a.calls);

  const rateLimited = rows.filter((r) => r.error === "rate_limited").length;

  // Hook dedup estimate: how many chat messages exist vs how many were
  // notified. Per-pseudonym, "notified" ≈ count of distinct (chat, viewer)
  // last_notified_message_id rows > 0. Rough heuristic; exact number would
  // require tracking the hook's emission count separately.
  const totalMsgs = d.query<{ c: number }, [number]>(
    "SELECT COUNT(*) AS c FROM messages WHERE created_at >= ?",
  ).get(cutoff)?.c ?? 0;
  const notifiedRows = d.query<{ c: number }, []>(
    "SELECT COUNT(*) AS c FROM chat_members WHERE last_notified_message_id > 0",
  ).get()?.c ?? 0;
  const suppressionRatio = totalMsgs > 0
    ? Math.max(0, 1 - notifiedRows / Math.max(1, totalMsgs))
    : 0;

  return {
    window_hours: opts.windowHours,
    per_tool: perTool,
    per_pseudonym: perPseudonym,
    rate_limited: rateLimited,
    hook_dedup_estimate: {
      chat_notifications_emitted: notifiedRows,
      chat_messages_total: totalMsgs,
      suppression_ratio_estimate: Number(suppressionRatio.toFixed(3)),
    },
  };
}

export function formatMetrics(m: MetricsReport): string {
  const lines: string[] = [];
  lines.push(`ClaudeTalk metrics (last ${m.window_hours}h)`);
  lines.push("");
  lines.push("Tool latency:");
  lines.push("  tool                        calls  errs   p50    p95    p99    max");
  for (const t of m.per_tool) {
    lines.push(
      `  ${t.tool.padEnd(28)}${String(t.calls).padStart(5)}${String(t.errors).padStart(6)}${String(t.p50_ms).padStart(6)}ms${String(t.p95_ms).padStart(6)}ms${String(t.p99_ms).padStart(6)}ms${String(t.max_ms).padStart(6)}ms`,
    );
  }
  if (m.per_tool.length === 0) lines.push("  (no tool calls in window)");
  lines.push("");
  lines.push("Per pseudonym:");
  for (const p of m.per_pseudonym) {
    lines.push(`  ${p.pseudonym.padEnd(28)}${String(p.calls).padStart(5)} calls, ${p.errors} errors`);
  }
  if (m.per_pseudonym.length === 0) lines.push("  (none)");
  lines.push("");
  lines.push(`Rate-limited calls: ${m.rate_limited}`);
  lines.push(
    `Hook dedup (estimate): ~${m.hook_dedup_estimate.suppression_ratio_estimate * 100}% of message hook fires suppressed by cursors`,
  );
  return lines.join("\n");
}
