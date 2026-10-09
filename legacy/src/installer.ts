/**
 * Install / uninstall logic for ClaudeTalk's MCP server + hooks. Extracted
 * from bin/cli.ts so the dispatcher stays under the file-size budget as
 * Phase 4 adds new commands.
 *
 * All file mutations go through safeWriteJson (atomic write, backup,
 * mode preservation). Pure logic — bin/cli.ts handles arg parsing and
 * the y/N prompt.
 */
import { existsSync } from "node:fs";
import { join } from "node:path";
import { readJson, safeWriteJson, type WriteOptions, type WriteResult } from "./safe-write.ts";

type JsonObj = Record<string, any>;

const readObj = (path: string): JsonObj => readJson(path) as JsonObj;

export interface InstallContext {
  bunBin: string;
  serverEntry: string;
  hookEntry: string;
  userMcpJson: string;
  userSettings: string;
}

export interface InstallOptions extends WriteOptions {
  promptIfRisky: boolean;
}

export interface InstallSummary {
  path: string;
  result: WriteResult;
  summary: string;
}

function serverEntry(ctx: InstallContext) {
  return {
    type: "stdio" as const,
    command: ctx.bunBin,
    args: ["run", ctx.serverEntry],
  };
}

function ensureHooks(
  cfg: any,
  ctx: InstallContext,
): { added: number; existed: number; removed: number } {
  cfg.hooks ??= {};
  const hookCmd = `${ctx.bunBin} run ${ctx.hookEntry}`;
  let added = 0;
  let existed = 0;
  let removed = 0;

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

  // PostToolUse no-matcher was too aggressive (10–30x/turn). Only fire on
  // claudetalk's own tool calls; UserPromptSubmit / PostToolBatch / Stop /
  // SubagentStop give Claude many other opportunities to see new messages.
  dropOurs("PostToolUse", null);

  upsert("SessionStart", null);
  upsert("UserPromptSubmit", null);
  upsert("PostToolUse", "mcp__claudetalk__.*");
  upsert("PostToolBatch", null);
  upsert("SubagentStop", null);
  upsert("Stop", null);

  return { added, existed, removed };
}

function removeHooks(cfg: any, ctx: InstallContext): number {
  if (!cfg.hooks) return 0;
  const hookCmd = `${ctx.bunBin} run ${ctx.hookEntry}`;
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

export function installUser(
  ctx: InstallContext,
  withHooks: boolean,
  opts: InstallOptions,
): InstallSummary[] {
  const out: InstallSummary[] = [];
  const mcp = readObj(ctx.userMcpJson);
  mcp.mcpServers ??= {};
  mcp.mcpServers.claudetalk = serverEntry(ctx);
  const mcpResult = safeWriteJson(ctx.userMcpJson, mcp, opts);
  out.push({
    path: ctx.userMcpJson,
    result: mcpResult,
    summary: `claudetalk → ${ctx.bunBin} run ${ctx.serverEntry}`,
  });

  if (withHooks) {
    const settings = readObj(ctx.userSettings);
    const { added, existed, removed } = ensureHooks(settings, ctx);
    const sResult = safeWriteJson(ctx.userSettings, settings, opts);
    out.push({
      path: ctx.userSettings,
      result: sResult,
      summary: `hooks added=${added}, already-present=${existed}, migrated=${removed}`,
    });
  }
  return out;
}

export function installProject(
  ctx: InstallContext,
  withHooks: boolean,
  opts: InstallOptions,
): InstallSummary[] {
  const out: InstallSummary[] = [];
  const here = process.cwd();
  const mcpPath = join(here, ".mcp.json");
  const mcp = readObj(mcpPath);
  mcp.mcpServers ??= {};
  mcp.mcpServers.claudetalk = serverEntry(ctx);
  const mcpResult = safeWriteJson(mcpPath, mcp, opts);
  out.push({
    path: mcpPath,
    result: mcpResult,
    summary: `claudetalk → ${ctx.bunBin} run ${ctx.serverEntry}`,
  });

  if (withHooks) {
    const projectSettings = join(here, ".claude", "settings.json");
    const settings = readObj(projectSettings);
    const { added, existed, removed } = ensureHooks(settings, ctx);
    const sResult = safeWriteJson(projectSettings, settings, opts);
    out.push({
      path: projectSettings,
      result: sResult,
      summary: `hooks added=${added}, already-present=${existed}, migrated=${removed}`,
    });
  }
  return out;
}

export function uninstall(
  ctx: InstallContext,
  scope: "user" | "project",
  opts: InstallOptions,
): InstallSummary[] {
  const out: InstallSummary[] = [];
  const mcpPath = scope === "user" ? ctx.userMcpJson : join(process.cwd(), ".mcp.json");
  const settingsPath =
    scope === "user" ? ctx.userSettings : join(process.cwd(), ".claude", "settings.json");

  if (existsSync(mcpPath)) {
    const mcp = readObj(mcpPath);
    if (mcp.mcpServers?.claudetalk) {
      delete mcp.mcpServers.claudetalk;
      if (Object.keys(mcp.mcpServers).length === 0) delete mcp.mcpServers;
      const r = safeWriteJson(mcpPath, mcp, opts);
      out.push({ path: mcpPath, result: r, summary: "removed claudetalk MCP entry" });
    } else {
      out.push({ path: mcpPath, result: "unchanged", summary: "(no claudetalk entry)" });
    }
  } else {
    out.push({ path: mcpPath, result: "unchanged", summary: "(no such file)" });
  }

  if (existsSync(settingsPath)) {
    const s = readObj(settingsPath);
    const removed = removeHooks(s, ctx);
    const r = safeWriteJson(settingsPath, s, opts);
    out.push({
      path: settingsPath,
      result: r,
      summary: `removed ${removed} hook entries`,
    });
  }
  return out;
}
