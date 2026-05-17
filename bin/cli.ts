#!/usr/bin/env bun
import { resolve, join, dirname } from "node:path";
import { existsSync, readSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";
import { pseudonymFor } from "../src/pseudonym.ts";
import { db } from "../src/db.ts";
import { listToolCalls, type ToolCallRow } from "../src/audit-log.ts";
import { readJson, type WriteResult } from "../src/safe-write.ts";
import { serveDashboard } from "../src/web/server.ts";
import {
  buildDoctorReport,
  buildMetrics,
  exportChat,
  formatDoctorReport,
  formatMetrics,
  runGc,
} from "../src/cli-commands.ts";
import {
  type InstallContext,
  type InstallOptions,
  type InstallSummary,
  installProject as installProjectFn,
  installUser as installUserFn,
  uninstall as uninstallFn,
} from "../src/installer.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = resolve(HERE, "..");
const SERVER_ENTRY = join(PKG_ROOT, "src", "server.ts");
const HOOK_ENTRY = join(PKG_ROOT, "hooks", "check-inbox.ts");
const BUN_BIN = process.execPath; // current Bun path; reused so the install pins to this Bun

const HOME = homedir();
const USER_MCP_JSON = join(HOME, ".claude.json");
const USER_SETTINGS = join(HOME, ".claude", "settings.json");

function help(): void {
  console.log(`claudetalk — Inter-Claude messaging over MCP

USAGE
  bun run bin/cli.ts <command> [options]

COMMANDS
  install [--scope user|project] [--no-hooks] [--dry-run] [--yes] [--no-backup]
      Register the MCP server with Claude Code.
        --scope user      ~/.claude.json (default; available in every project)
        --scope project   ./.mcp.json    (committed to the current repo)
        --no-hooks        skip writing the SessionStart / PostToolUse hooks
        --dry-run         show the unified diff of what would change; write nothing
        --yes             skip the confirmation prompt (default: prompt for --scope user)
        --no-backup       skip writing the .bak.<timestamp> sidecar (NOT recommended)
  uninstall [--scope user|project] [--dry-run] [--yes] [--no-backup]
      Remove the MCP server (and matching hooks) from the chosen scope.
  whoami [--path PATH]
      Print the deterministic pseudonym for a folder (default: cwd).
  doctor
      Sanity-check DB, list registered instances, show install state.
  tail
      Print recent presence + activity from the DB.
  web [--port 4242] [--host 127.0.0.1] [--open]
      Start a read-only browser dashboard. Live-updates over SSE every 500 ms.
      Bound to 127.0.0.1 by default — never exposed to the network.
  log [--follow] [--pseudonym X] [--tool Y] [--kind K] [--limit N]
      Show recent MCP traffic across all instances. Each row is a tool
      handler invocation, JSON-RPC request, response or notification.
      Filter --kind tool|request|response|notification. With --follow, tail
      new entries as they arrive (Ctrl-C to stop). Default --limit 50.
  gc [--older-than-days N] [--vacuum]
      Prune audit-log rows older than N days (default 30). With --vacuum,
      also reclaim file space.
  export <chat_id> [--format md|json]
      Dump a chat's full history to stdout. Default format: md.
  metrics [--window-hours N]
      Per-tool p50/p95/p99 latency, per-pseudonym call counts, hook dedup
      ratio, rate-limit events over the last N hours (default 24).
  help
      Show this help.
`);
}

type JsonObj = Record<string, any>;
const readObj = (path: string): JsonObj => readJson(path) as JsonObj;

const INSTALL_CTX: InstallContext = {
  bunBin: BUN_BIN,
  serverEntry: SERVER_ENTRY,
  hookEntry: HOOK_ENTRY,
  userMcpJson: USER_MCP_JSON,
  userSettings: USER_SETTINGS,
};

/** Prompt y/N from a TTY. Returns true on 'y' / 'yes'. */
function confirm(question: string): boolean {
  if (!process.stdin.isTTY) return false;
  process.stdout.write(`${question} [y/N] `);
  const buf = Buffer.alloc(4);
  let n = 0;
  try {
    n = readSync(0, buf, 0, 4, null);
  } catch {
    return false;
  }
  const ans = buf.slice(0, n).toString("utf8").trim().toLowerCase();
  return ans === "y" || ans === "yes";
}

function describeChange(path: string, result: WriteResult, summary: string): void {
  const verb =
    result === "wrote" ? "✔ wrote" : result === "skipped" ? "› would change" : "= unchanged";
  console.log(`${verb}  ${path}   ${summary}`);
}

function reportInstall(results: InstallSummary[]): void {
  for (const r of results) describeChange(r.path, r.result, r.summary);
}

function installUser(withHooks: boolean, opts: InstallOptions): void {
  if (opts.promptIfRisky && !opts.dryRun) {
    console.log(
      `About to modify ${USER_MCP_JSON} (your main Claude Code config) ` +
        `and ${USER_SETTINGS}. A timestamped .bak copy will be written next to each file. ` +
        "Run with --dry-run first to see the unified diff.",
    );
    if (!confirm("Proceed?")) {
      console.log("Aborted (no files changed). Use --dry-run to preview, --yes to skip this prompt.");
      return;
    }
  }
  reportInstall(installUserFn(INSTALL_CTX, withHooks, opts));
  if (opts.dryRun) {
    console.log("\nNothing was written (--dry-run). Re-run without --dry-run to apply.");
  } else {
    console.log("\nRestart any open Claude Code sessions to pick up the new server.");
  }
}

function installProject(withHooks: boolean, opts: InstallOptions): void {
  reportInstall(installProjectFn(INSTALL_CTX, withHooks, opts));
  if (opts.dryRun) {
    console.log("\nNothing was written (--dry-run). Re-run without --dry-run to apply.");
  } else {
    console.log(
      "\nRestart this Claude Code session (and acknowledge the new MCP / hooks prompts) to pick it up.",
    );
  }
}

function uninstall(scope: "user" | "project", opts: InstallOptions): void {
  reportInstall(uninstallFn(INSTALL_CTX, scope, opts));
}

function whoami(path: string): void {
  const id = pseudonymFor(resolve(path));
  console.log(`pseudonym: ${id.pseudonym}`);
  console.log(`folder:    ${id.path}`);
  console.log(`hash:      ${id.hash}`);
}

function doctor(): void {
  db(); // open + migrate first so schema_version is populated
  const installPaths = [
    { label: "user .claude.json", path: USER_MCP_JSON },
    { label: "user settings.json", path: USER_SETTINGS },
    { label: "project .mcp.json", path: join(process.cwd(), ".mcp.json") },
    { label: "project settings.json", path: join(process.cwd(), ".claude", "settings.json") },
  ];
  const report = buildDoctorReport(
    BUN_BIN,
    SERVER_ENTRY,
    HOOK_ENTRY,
    installPaths,
    (p) => readObj(p),
    (p) => existsSync(p),
  );
  console.log(formatDoctorReport(report));
}

async function web(port: number, host: string, openInBrowser: boolean): Promise<void> {
  db(); // ensure schema exists so the dashboard has something to read
  const dash = serveDashboard({ port, hostname: host });
  console.log(`ClaudeTalk dashboard: ${dash.url}`);
  console.log("(read-only; press Ctrl-C to stop)");
  if (openInBrowser) {
    try {
      const opener =
        process.platform === "darwin"
          ? "open"
          : process.platform === "win32"
            ? "start"
            : "xdg-open";
      Bun.spawn([opener, dash.url], { stdout: "ignore", stderr: "ignore" });
    } catch (e) {
      console.error(`(could not open browser automatically: ${(e as Error).message})`);
    }
  }
  const stop = async () => {
    console.log("\nstopping dashboard…");
    await dash.stop();
    process.exit(0);
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
  // Keep the process alive — Bun.serve doesn't block by itself.
  await new Promise<void>(() => {});
}

function tail(): void {
  const d = db();
  const recent = d
    .query<
      { kind: string; from_pseudonym: string; to_pseudonym: string | null; body: string; created_at: number },
      []
    >(
      `SELECT 'ask' AS kind, from_pseudonym, to_pseudonym, body, created_at FROM asks
       UNION ALL
       SELECT 'msg' AS kind, from_pseudonym, chat_id AS to_pseudonym, body, created_at FROM messages
       ORDER BY created_at DESC LIMIT 30`,
    )
    .all();
  for (const r of recent.reverse()) {
    const when = new Date(r.created_at).toISOString();
    const arrow = r.kind === "ask" ? "->" : "in";
    console.log(`${when}  ${r.kind.toUpperCase()}  ${r.from_pseudonym} ${arrow} ${r.to_pseudonym}: ${r.body}`);
  }
}

function fmtToolCall(c: ToolCallRow): string {
  const when = new Date(c.started_at).toISOString();
  const status = c.is_error ? "ERR" : "OK ";
  const kind = c.kind.padEnd(12); // 'tool        '|'request     '|'response    '|'notification'
  const dir = c.direction === "out" ? "↑" : "↓";
  const idTag = c.jrpc_id !== null ? `#${c.jrpc_id}` : "  ";
  const dur = c.duration_ms > 0 ? `  (${c.duration_ms}ms)` : "";
  const args = c.args_json ? `  payload=${c.args_json}` : "";
  const summary = c.error
    ? `  error=${c.error}`
    : c.result_summary
      ? `  result=${(c.result_summary.split("\n")[0] ?? "").slice(0, 120)}`
      : "";
  return `${when}  ${status} ${dir} ${kind} ${idTag}  ${c.pseudonym}  ${c.tool}${dur}${args}${summary}`;
}

async function logCmd(opts: {
  follow: boolean;
  pseudonym?: string;
  tool?: string;
  kind?: string;
  limit: number;
}): Promise<void> {
  db(); // ensure schema
  let lastId = 0;
  const rows = listToolCalls({
    pseudonym: opts.pseudonym,
    tool: opts.tool,
    kind: opts.kind,
    limit: opts.limit,
  });
  for (const r of rows) console.log(fmtToolCall(r));
  if (rows.length > 0) lastId = rows[rows.length - 1]!.id;
  if (!opts.follow) return;
  console.error("(following — Ctrl-C to stop)");
  while (true) {
    await Bun.sleep(500);
    const fresh = listToolCalls({
      pseudonym: opts.pseudonym,
      tool: opts.tool,
      kind: opts.kind,
      sinceId: lastId,
      limit: 500,
    });
    for (const r of fresh) {
      console.log(fmtToolCall(r));
      lastId = r.id;
    }
  }
}

function getArg(name: string, def?: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0) return def;
  const next = process.argv[i + 1];
  if (!next || next.startsWith("--")) return def;
  return next;
}

function hasFlag(name: string): boolean {
  return process.argv.includes(`--${name}`);
}

function writeOptsFromArgs(scope: "user" | "project"): InstallOptions {
  return {
    dryRun: hasFlag("dry-run"),
    backup: !hasFlag("no-backup"),
    // For user-scope writes, prompt by default because we touch ~/.claude.json.
    promptIfRisky: scope === "user" && !hasFlag("yes"),
  };
}

const cmd = process.argv[2] ?? "help";
switch (cmd) {
  case "install": {
    const scope = (getArg("scope", "user") as "user" | "project") ?? "user";
    const withHooks = !hasFlag("no-hooks");
    const opts = writeOptsFromArgs(scope);
    if (scope === "user") installUser(withHooks, opts);
    else if (scope === "project") installProject(withHooks, opts);
    else {
      console.error(`Unknown --scope '${scope}'. Use 'user' or 'project'.`);
      process.exit(2);
    }
    break;
  }
  case "uninstall": {
    const scope = (getArg("scope", "user") as "user" | "project") ?? "user";
    const opts = writeOptsFromArgs(scope);
    uninstall(scope, opts);
    break;
  }
  case "whoami":
    whoami(getArg("path", process.cwd())!);
    break;
  case "doctor":
    doctor();
    break;
  case "tail":
    tail();
    break;
  case "web": {
    const port = Number(getArg("port", "4242"));
    const host = getArg("host", "127.0.0.1")!;
    const openInBrowser = hasFlag("open");
    await web(port, host, openInBrowser);
    break;
  }
  case "log": {
    await logCmd({
      follow: hasFlag("follow"),
      pseudonym: getArg("pseudonym"),
      tool: getArg("tool"),
      kind: getArg("kind"),
      limit: Number(getArg("limit", "50")),
    });
    break;
  }
  case "gc": {
    db();
    const r = runGc({
      olderThanDays: Number(getArg("older-than-days", "30")),
      vacuum: hasFlag("vacuum"),
    });
    console.log(
      `✓ pruned ${r.pruned_tool_calls} audit rows, ${r.retained_tool_calls} retained${r.vacuumed ? " (vacuumed)" : ""}.`,
    );
    break;
  }
  case "export": {
    db();
    const chatId = process.argv[3];
    if (!chatId) {
      console.error("Usage: claudetalk export <chat_id> [--format md|json]");
      process.exit(2);
    }
    const fmt = (getArg("format", "md") as "md" | "json") ?? "md";
    if (fmt !== "md" && fmt !== "json") {
      console.error(`Unknown --format '${fmt}'. Use md or json.`);
      process.exit(2);
    }
    const r = exportChat(chatId, fmt);
    if (!r.ok) {
      console.error(r.output);
      process.exit(1);
    }
    console.log(r.output);
    break;
  }
  case "metrics": {
    db();
    const m = buildMetrics({ windowHours: Number(getArg("window-hours", "24")) });
    console.log(formatMetrics(m));
    break;
  }
  case "help":
  case "--help":
  case "-h":
    help();
    break;
  default:
    console.error(`Unknown command '${cmd}'. Run with 'help' for usage.`);
    process.exit(2);
}
