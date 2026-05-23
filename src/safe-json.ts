/**
 * Bounded JSON readers for config files in `~/.claudetalk/` and
 * `~/.claude/`. Prevents a malicious sibling process from writing a
 * multi-GB JSON file that crashes every newly-started MCP server with
 * an OOM during the JSON.parse pass. (Security audit M7.)
 */
import { readFileSync, statSync } from "node:fs";

/** Default max size for any config file we parse — 1 MiB. None of the
 *  config files in ClaudeTalk legitimately need more than a few KB; this
 *  is a defence-in-depth ceiling, not a tight bound. */
const MAX_CONFIG_BYTES = 1_000_000;

export interface ReadJsonOptions {
  /** Override the size cap for this specific call (rare). */
  maxBytes?: number;
}

/** Read + parse a JSON file with a size guard. Returns `null` if the file
 *  doesn't exist, is too large, or contains invalid JSON. Callers treat
 *  null as "absent / unusable" — same semantics as before, just safer. */
export function readJsonBounded<T = unknown>(
  path: string,
  opts: ReadJsonOptions = {},
): T | null {
  try {
    const max = opts.maxBytes ?? MAX_CONFIG_BYTES;
    const st = statSync(path);
    if (st.size > max) {
      console.error(
        `[claudetalk] refusing to parse ${path} — size ${st.size} > max ${max}`,
      );
      return null;
    }
    return JSON.parse(readFileSync(path, "utf8")) as T;
  } catch {
    return null;
  }
}
