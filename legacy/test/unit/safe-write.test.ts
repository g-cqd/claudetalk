import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  formatDiff,
  readJson,
  safeWriteJson,
  structuralDiff,
} from "../../src/safe-write.ts";

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "claudetalk-safewrite-"));
});
afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("structuralDiff", () => {
  test("identical objects → no entries", () => {
    expect(structuralDiff({ a: 1 }, { a: 1 })).toEqual([]);
  });

  test("added leaf key", () => {
    const d = structuralDiff({ a: 1 }, { a: 1, b: 2 });
    expect(d.length).toBe(1);
    expect(d[0]).toMatchObject({ path: "$.b", kind: "added", after: 2 });
  });

  test("removed nested key", () => {
    const d = structuralDiff({ a: { b: 2, c: 3 } }, { a: { b: 2 } });
    expect(d.length).toBe(1);
    expect(d[0]).toMatchObject({ path: "$.a.c", kind: "removed", before: 3 });
  });

  test("modified leaf", () => {
    const d = structuralDiff({ a: 1 }, { a: 2 });
    expect(d[0]).toMatchObject({ kind: "modified", before: 1, after: 2 });
  });

  test("arrays are diffed wholesale (no positional drift)", () => {
    const d = structuralDiff({ list: [1, 2] }, { list: [1, 2, 3] });
    expect(d.length).toBe(1);
    expect(d[0]!.kind).toBe("modified");
  });

  test("non-identifier keys are escaped in the path", () => {
    const d = structuralDiff({}, { "weird-key": 1, "ok_key": 2 });
    const paths = d.map((e) => e.path).sort();
    expect(paths).toEqual(['$.ok_key', '$["weird-key"]']);
  });
});

describe("formatDiff", () => {
  test("collapses to '(label: no changes)' when there are no entries", () => {
    expect(formatDiff("/file.json", [])).toBe("(/file.json: no changes)");
  });
  test("prints + / - / ~ markers", () => {
    const txt = formatDiff("/x.json", [
      { path: "$.a", kind: "added", after: 1 },
      { path: "$.b", kind: "removed", before: 2 },
      { path: "$.c", kind: "modified", before: 3, after: 4 },
    ]);
    expect(txt).toContain("+ $.a");
    expect(txt).toContain("- $.b");
    expect(txt).toContain("~ $.c");
  });
});

describe("safeWriteJson", () => {
  test("writes atomically when the file does not exist", () => {
    const path = join(dir, "new.json");
    const result = safeWriteJson(path, { x: 1 }, { dryRun: false, backup: true }, () => {});
    expect(result).toBe("wrote");
    expect(JSON.parse(readFileSync(path, "utf8"))).toEqual({ x: 1 });
  });

  test("returns 'unchanged' when serialized output matches", () => {
    const path = join(dir, "stable.json");
    writeFileSync(path, JSON.stringify({ x: 1 }, null, 2) + "\n");
    const result = safeWriteJson(path, { x: 1 }, { dryRun: false, backup: true });
    expect(result).toBe("unchanged");
  });

  test("dry-run prints a diff and writes NOTHING", () => {
    const path = join(dir, "dry.json");
    writeFileSync(path, JSON.stringify({ x: 1 }, null, 2) + "\n");
    const before = readFileSync(path, "utf8");
    const logs: string[] = [];
    const result = safeWriteJson(
      path,
      { x: 1, y: 2 },
      { dryRun: true, backup: true },
      (m) => logs.push(m),
    );
    expect(result).toBe("skipped");
    expect(readFileSync(path, "utf8")).toBe(before);
    expect(logs.join("\n")).toContain("$.y");
  });

  test("creates a timestamped backup before overwriting", () => {
    const path = join(dir, "backed.json");
    writeFileSync(path, JSON.stringify({ x: 1 }, null, 2) + "\n");
    safeWriteJson(path, { x: 2 }, { dryRun: false, backup: true }, () => {});
    const baks = require("node:fs")
      .readdirSync(dir)
      .filter((f: string) => f.startsWith("backed.json.bak."));
    expect(baks.length).toBe(1);
    expect(JSON.parse(readFileSync(join(dir, baks[0]), "utf8"))).toEqual({ x: 1 });
  });

  test("preserves file mode (0o600 stays 0o600)", () => {
    const path = join(dir, "mode.json");
    writeFileSync(path, JSON.stringify({ x: 1 }) + "\n");
    chmodSync(path, 0o600);
    safeWriteJson(path, { x: 2 }, { dryRun: false, backup: false }, () => {});
    expect(statSync(path).mode & 0o777).toBe(0o600);
  });

  test("readJson returns {} for missing files and throws on malformed JSON", () => {
    expect(readJson(join(dir, "does-not-exist.json"))).toEqual({});
    const bad = join(dir, "bad.json");
    writeFileSync(bad, "{not valid json,");
    expect(() => readJson(bad)).toThrow(/Failed to parse/);
  });

  test("--no-backup skips the .bak sidecar", () => {
    const path = join(dir, "nobak.json");
    writeFileSync(path, JSON.stringify({ x: 1 }, null, 2) + "\n");
    safeWriteJson(path, { x: 2 }, { dryRun: false, backup: false }, () => {});
    const baks = require("node:fs")
      .readdirSync(dir)
      .filter((f: string) => f.startsWith("nobak.json.bak."));
    expect(baks).toEqual([]);
  });
});
