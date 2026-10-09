/**
 * Atomic, mode-preserving, backup-creating JSON file writer.
 *
 * Used by the installer (bin/cli.ts) to mutate ~/.claude.json and
 * ~/.claude/settings.json without ever losing or widening-permissions on the
 * original. Pure logic, no Claude-specific knowledge — testable in isolation.
 */
import { basename, dirname, join } from "node:path";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  renameSync,
  statSync,
  writeFileSync,
} from "node:fs";

export interface WriteOptions {
  /** When true, print the structural diff and write nothing. */
  dryRun: boolean;
  /** When true, copy the current file to `<path>.bak.<ISO>` before replacing. */
  backup: boolean;
}

export type WriteResult = "wrote" | "skipped" | "unchanged";

export interface DiffEntry {
  path: string;
  kind: "added" | "removed" | "modified";
  before?: unknown;
  after?: unknown;
}

/** Size cap for config files we read into memory. ~/.claude.json
 *  legitimately runs to tens of KB on busy users; 4 MiB is comfortably
 *  above realistic config size while preventing an attacker-pre-placed
 *  multi-GB JSON from OOM'ing the installer. (Security audit M7.) */
const READ_JSON_MAX_BYTES = 4_000_000;

function readJsonRaw(path: string): string | null {
  if (!existsSync(path)) return null;
  const st = statSync(path);
  if (st.size > READ_JSON_MAX_BYTES) {
    throw new Error(
      `Refusing to read ${path}: size ${st.size} > max ${READ_JSON_MAX_BYTES}`,
    );
  }
  return readFileSync(path, "utf8");
}

export function readJson(path: string): unknown {
  const raw = readJsonRaw(path);
  if (raw === null) return {};
  try {
    return JSON.parse(raw);
  } catch (e) {
    throw new Error(`Failed to parse ${path}: ${(e as Error).message}`);
  }
}

/** JSON-aware structural diff: walks the two trees and reports added /
 *  removed / modified paths. A naive line-diff on a 6000-line config file is
 *  useless because inserting one key shifts every subsequent line. */
export function structuralDiff(
  before: unknown,
  after: unknown,
  path = "$",
): DiffEntry[] {
  if (before === undefined && after !== undefined) {
    return [{ path, kind: "added", after }];
  }
  if (before !== undefined && after === undefined) {
    return [{ path, kind: "removed", before }];
  }
  const bothObjects =
    before !== null &&
    after !== null &&
    typeof before === "object" &&
    typeof after === "object" &&
    Array.isArray(before) === Array.isArray(after);

  if (!bothObjects) {
    return JSON.stringify(before) === JSON.stringify(after)
      ? []
      : [{ path, kind: "modified", before, after }];
  }

  // Arrays diff coarsely: identical or replaced wholesale.
  if (Array.isArray(before) && Array.isArray(after)) {
    return JSON.stringify(before) === JSON.stringify(after)
      ? []
      : [{ path, kind: "modified", before, after }];
  }

  const out: DiffEntry[] = [];
  const keys = new Set<string>([
    ...Object.keys(before as object),
    ...Object.keys(after as object),
  ]);
  for (const k of [...keys].sort()) {
    const childPath = /^[A-Za-z_][A-Za-z0-9_]*$/.test(k)
      ? `${path}.${k}`
      : `${path}[${JSON.stringify(k)}]`;
    out.push(
      ...structuralDiff(
        (before as Record<string, unknown>)[k],
        (after as Record<string, unknown>)[k],
        childPath,
      ),
    );
  }
  return out;
}

export function formatDiff(label: string, entries: DiffEntry[]): string {
  if (entries.length === 0) return `(${label}: no changes)`;
  const lines: string[] = [`changes in ${label}:`];
  for (const e of entries) {
    if (e.kind === "added") {
      const v = JSON.stringify(e.after, null, 2).split("\n").join("\n      ");
      lines.push(`  + ${e.path}`, `      ${v}`);
    } else if (e.kind === "removed") {
      const v = JSON.stringify(e.before, null, 2).split("\n").join("\n      ");
      lines.push(`  - ${e.path}`, `      ${v}`);
    } else {
      const a = JSON.stringify(e.before, null, 2).split("\n").join("\n        ");
      const b = JSON.stringify(e.after, null, 2).split("\n").join("\n        ");
      lines.push(`  ~ ${e.path}`, `      before: ${a}`, `      after:  ${b}`);
    }
  }
  return lines.join("\n");
}

/** Pure atomic write. Returns "wrote", "skipped" (dry-run with a diff), or
 *  "unchanged" (serialized output identical to current file bytes). */
export function safeWriteJson(
  path: string,
  data: unknown,
  opts: WriteOptions,
  log: (msg: string) => void = console.log,
): WriteResult {
  const serialized = JSON.stringify(data, null, 2) + "\n";
  const before = readJsonRaw(path) ?? "";
  if (before === serialized) return "unchanged";

  if (opts.dryRun) {
    let beforeVal: unknown = {};
    try {
      beforeVal = before === "" ? {} : JSON.parse(before);
    } catch {
      beforeVal = {};
    }
    log(formatDiff(path, structuralDiff(beforeVal, data)));
    return "skipped";
  }

  mkdirSync(dirname(path), { recursive: true });

  let mode = 0o644;
  if (existsSync(path)) {
    // Refuse to follow symlinks. `copyFileSync(path, bak)` follows symlinks
    // by default, so an attacker-pre-placed symlink at ~/.claude.json
    // pointing to a sensitive file (e.g. ~/.ssh/id_rsa) would copy the
    // target's contents into a 0o644 backup that we then create.
    // (Security audit M4.)
    try {
      const lst = lstatSync(path);
      if (lst.isSymbolicLink()) {
        throw new Error(
          `Refusing to write through symbolic link at ${path}. ` +
            `Resolve the symlink or remove it before retrying.`,
        );
      }
    } catch (e: any) {
      if (e?.code === "ENOENT") {
        // raced; file disappeared between existsSync and lstatSync — fine
      } else {
        throw e;
      }
    }
    try {
      mode = statSync(path).mode & 0o777;
    } catch {}
    if (opts.backup) {
      const stamp = new Date().toISOString().replace(/[:.]/g, "-");
      const bak = `${path}.bak.${stamp}`;
      copyFileSync(path, bak);
      try {
        chmodSync(bak, mode);
      } catch {}
      log(`  backup: ${bak}`);
    }
  }

  // Atomic write: write to tmp in the same directory, fsync via write, then
  // rename(2) over the original. A crash before rename leaves the original intact.
  const tmp = join(
    dirname(path),
    `.${basename(path)}.tmp-${process.pid}-${Date.now()}`,
  );
  writeFileSync(tmp, serialized, { encoding: "utf8", mode });
  renameSync(tmp, path);
  return "wrote";
}
