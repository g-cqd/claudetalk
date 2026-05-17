#!/usr/bin/env bun
import { resolve, join, dirname } from "node:path";
import { existsSync, readSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";
import { pseudonymFor } from "../src/pseudonym.ts";
import { db, listInstances } from "../src/db.ts";
import { listToolCalls, type ToolCallRow } from "../src/audit-log.ts";
import { fmtInstance } from "../src/format.ts";
import {
  readJson,
  safeWriteJson,
  type WriteOptions,
  type WriteResult,
} from "../src/safe-write.ts";
import { serveDashboard } from "../src/web/server.ts";

type JsonObj = Record<string, any>;
const readObj = (path: string): JsonObj => readJson(path) as JsonObj;

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
  help
      Show this help.
`);
}

interface InstallOptions extends WriteOptions {
  promptIfRisky: boolean;
}

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

function serverEntry() {
  return {
    type: "stdio" as const,
    command: BUN_BIN,
    args: ["run", SERVER_ENTRY],
  };
}

function ensureHooks(cfg: any): { added: number; existed: number; removed: number } {
  cfg.hooks ??= {};
  const hookCmd = `${BUN_BIN} run ${HOOK_ENTRY}`;
  let added = 0;
  let existed = 0;
  let removed = 0;

  /** Drop our hook command from any block that matches (eventName, matcher).
   *  Foreign commands in the same block are preserved. */
  const dropOurs = (eventName: string, matcher: string | null) => {
    const blocks = cfg.hooks[eventName];
    if (!Array.isArray(blocks)) return;
    for (const block of blocks) {
      const matcherEq = matcher === null ? block.matcher == null : block.matcher === matcher;
      if (!matcherEq || !Array.isArray(block.hooks)) continue;
      const before = block.hooks.length;
      block.hooks = block.hooks.filter(
        (h: any) => !(h.type === "command" && h.command === hookCmd),
      );
      removed += before - block.hooks.length;
    }
    cfg.hooks[eventName] = blocks.filter(
      (b: any) => Array.isArray(b.hooks) && b.hooks.length > 0,
    );
    if (cfg.hooks[eventName].length === 0) delete cfg.hooks[eventName];
  };

  const upsert = (eventName: string, matcher: string | null) => {
    cfg.hooks[eventName] ??= [];
    const block = cfg.hooks[eventName].find(
      (b: any) =>
        (matcher === null ? b.matcher == null : b.matcher === matcher) &&
        Array.isArray(b.hooks),
    );
    const target = block ?? { matcher: matcher ?? undefined, hooks: [] };
    if (!block) cfg.hooks[eventName].push(target);
    const present = target.hooks.some(
      (h: any) => h.type === "command" && h.command === hookCmd,
    );
    if (present) existed++;
    else {
      target.hooks.push({ type: "command", command: hookCmd });
      added++;
    }
  };

  // PostToolUse no-matcher was too aggressive — it fires 10–30x per turn per
  // Claude session, causing SQLite write-lock contention with MCP tool calls
  // and the dashboard. Reverting to claudetalk-only matcher; the other
  // expanded events (UserPromptSubmit, PostToolBatch, SubagentStop) still
  // give Claude many opportunities to see new messages without the per-tool
  // spam.
  dropOurs("PostToolUse", null);

  upsert("SessionStart", null);
  upsert("UserPromptSubmit", null);
  upsert("PostToolUse", "mcp__claudetalk__.*");
  upsert("PostToolBatch", null);
  upsert("SubagentStop", null);
  upsert("Stop", null);

  return { added, existed, removed };
}

function removeHooks(cfg: any): number {
  if (!cfg.hooks) return 0;
  const hookCmd = `${BUN_BIN} run ${HOOK_ENTRY}`;
  let removed = 0;
  for (const ev of Object.keys(cfg.hooks)) {
    const blocks = cfg.hooks[ev];
    if (!Array.isArray(blocks)) continue;
    for (const block of blocks) {
      if (!Array.isArray(block.hooks)) continue;
      const before = block.hooks.length;
      block.hooks = block.hooks.filter(
        (h: any) => !(h.type === "command" && h.command === hookCmd),
      );
      removed += before - block.hooks.length;
    }
    cfg.hooks[ev] = blocks.filter((b: any) => Array.isArray(b.hooks) && b.hooks.length > 0);
    if (cfg.hooks[ev].length === 0) delete cfg.hooks[ev];
  }
  if (Object.keys(cfg.hooks).length === 0) delete cfg.hooks;
  return removed;
}

function describeChange(path: string, result: WriteResult, summary: string): void {
  const verb =
    result === "wrote" ? "✔ wrote" : result === "skipped" ? "› would change" : "= unchanged";
  console.log(`${verb}  ${path}   ${summary}`);
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

  const mcp = readObj(USER_MCP_JSON);
  mcp.mcpServers ??= {};
  mcp.mcpServers.claudetalk = serverEntry();
  const mcpResult = safeWriteJson(USER_MCP_JSON, mcp, opts);
  describeChange(USER_MCP_JSON, mcpResult, `claudetalk → ${BUN_BIN} run ${SERVER_ENTRY}`);

  if (withHooks) {
    const settings = readObj(USER_SETTINGS);
    const { added, existed, removed } = ensureHooks(settings);
    const sResult = safeWriteJson(USER_SETTINGS, settings, opts);
    describeChange(USER_SETTINGS, sResult, `hooks added=${added}, already-present=${existed}, migrated=${removed}`);
  }

  if (opts.dryRun) {
    console.log("\nNothing was written (--dry-run). Re-run without --dry-run to apply.");
  } else {
    console.log("\nRestart any open Claude Code sessions to pick up the new server.");
  }
}

function installProject(withHooks: boolean, opts: InstallOptions): void {
  const here = process.cwd();
  const mcpPath = join(here, ".mcp.json");
  const mcp = readObj(mcpPath);
  mcp.mcpServers ??= {};
  mcp.mcpServers.claudetalk = serverEntry();
  const mcpResult = safeWriteJson(mcpPath, mcp, opts);
  describeChange(mcpPath, mcpResult, `claudetalk → ${BUN_BIN} run ${SERVER_ENTRY}`);

  if (withHooks) {
    const projectSettings = join(here, ".claude", "settings.json");
    const settings = readObj(projectSettings);
    const { added, existed, removed } = ensureHooks(settings);
    const sResult = safeWriteJson(projectSettings, settings, opts);
    describeChange(projectSettings, sResult, `hooks added=${added}, already-present=${existed}, migrated=${removed}`);
  }

  if (opts.dryRun) {
    console.log("\nNothing was written (--dry-run). Re-run without --dry-run to apply.");
  } else {
    console.log(
      "\nRestart this Claude Code session (and acknowledge the new MCP / hooks prompts) to pick it up.",
    );
  }
}

function uninstall(scope: "user" | "project", opts: InstallOptions): void {
  const mcpPath = scope === "user" ? USER_MCP_JSON : join(process.cwd(), ".mcp.json");
  const settingsPath =
    scope === "user" ? USER_SETTINGS : join(process.cwd(), ".claude", "settings.json");

  if (existsSync(mcpPath)) {
    const mcp = readObj(mcpPath);
    if (mcp.mcpServers?.claudetalk) {
      delete mcp.mcpServers.claudetalk;
      if (Object.keys(mcp.mcpServers).length === 0) delete mcp.mcpServers;
      const r = safeWriteJson(mcpPath, mcp, opts);
      describeChange(mcpPath, r, "removed claudetalk MCP entry");
    } else {
      console.log(`(no claudetalk entry in ${mcpPath})`);
    }
  } else {
    console.log(`(no ${mcpPath})`);
  }

  if (existsSync(settingsPath)) {
    const s = readObj(settingsPath);
    const removed = removeHooks(s);
    const r = safeWriteJson(settingsPath, s, opts);
    describeChange(settingsPath, r, `removed ${removed} hook entries`);
  }
}

function whoami(path: string): void {
  const id = pseudonymFor(resolve(path));
  console.log(`pseudonym: ${id.pseudonym}`);
  console.log(`folder:    ${id.path}`);
  console.log(`hash:      ${id.hash}`);
}

function doctor(): void {
  console.log("ClaudeTalk doctor");
  console.log(`  bun:    ${BUN_BIN}`);
  console.log(`  server: ${SERVER_ENTRY}`);
  console.log(`  hook:   ${HOOK_ENTRY}`);
  console.log();

  for (const [label, path] of [
    ["user .claude.json", USER_MCP_JSON],
    ["user settings.json", USER_SETTINGS],
    ["project .mcp.json", join(process.cwd(), ".mcp.json")],
    ["project settings.json", join(process.cwd(), ".claude", "settings.json")],
  ] as const) {
    const exists = existsSync(path);
    const j = exists ? readObj(path) : null;
    const hasServer = j?.mcpServers?.claudetalk ? "✔ MCP" : "  ";
    const hookCmd = `${BUN_BIN} run ${HOOK_ENTRY}`;
    let hookCount = 0;
    if (j?.hooks) {
      for (const ev of Object.keys(j.hooks)) {
        for (const block of j.hooks[ev] ?? []) {
          for (const h of block.hooks ?? []) {
            if (h.command === hookCmd) hookCount++;
          }
        }
      }
    }
    const hookMark = hookCount > 0 ? `✔ ${hookCount} hooks` : "";
    console.log(`  [${exists ? "x" : " "}] ${label.padEnd(22)} ${path}   ${hasServer} ${hookMark}`);
  }
  console.log();

  db(); // open + migrate
  const active = listInstances(60 * 60 * 1000);
  console.log(`Active instances in last hour (${active.length}):`);
  for (const i of active) console.log("  " + fmtInstance(i));
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
  case "help":
  case "--help":
  case "-h":
    help();
    break;
  default:
    console.error(`Unknown command '${cmd}'. Run with 'help' for usage.`);
    process.exit(2);
}
