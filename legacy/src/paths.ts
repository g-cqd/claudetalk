import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { mkdirSync } from "node:fs";

/** Root directory for ClaudeTalk state. Honors CLAUDETALK_HOME for tests. */
function rootDir(): string {
  const override = process.env.CLAUDETALK_HOME;
  if (override && override.length > 0) return resolve(override);
  return join(homedir(), ".claudetalk");
}

export function dbPath(): string {
  return join(rootDir(), "db.sqlite");
}

export function ensureRootDir(): void {
  mkdirSync(rootDir(), { recursive: true });
}

/** Resolve the folder identity for this MCP server instance.
 *  Claude Code sets CLAUDE_PROJECT_DIR; fall back to cwd otherwise. */
export function resolveProjectDir(): string {
  const env = process.env.CLAUDE_PROJECT_DIR;
  const raw = env && env.length > 0 ? env : process.cwd();
  return resolve(raw);
}
