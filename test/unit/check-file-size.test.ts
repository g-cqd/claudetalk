import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
  checkFileSizes,
  countLines,
  formatReport,
  loadBudget,
} from "../../scripts/check-file-size.ts";

function makeTree(layout: Record<string, string>): string {
  const root = mkdtempSync(join(tmpdir(), "claudetalk-fsize-"));
  for (const [rel, content] of Object.entries(layout)) {
    const abs = join(root, rel);
    mkdirSync(join(abs, ".."), { recursive: true });
    writeFileSync(abs, content);
  }
  return root;
}

describe("countLines", () => {
  test("empty file → 0", () => {
    const root = mkdtempSync(join(tmpdir(), "claudetalk-cnt-"));
    const f = join(root, "empty.ts");
    writeFileSync(f, "");
    expect(countLines(f)).toBe(0);
    rmSync(root, { recursive: true, force: true });
  });

  test("trailing newline → counts last line once (wc -l semantics)", () => {
    const root = mkdtempSync(join(tmpdir(), "claudetalk-cnt-"));
    const f = join(root, "x.ts");
    writeFileSync(f, "a\nb\nc\n");
    expect(countLines(f)).toBe(3);
    rmSync(root, { recursive: true, force: true });
  });

  test("no trailing newline → still counts the last line", () => {
    const root = mkdtempSync(join(tmpdir(), "claudetalk-cnt-"));
    const f = join(root, "x.ts");
    writeFileSync(f, "a\nb\nc");
    expect(countLines(f)).toBe(3);
    rmSync(root, { recursive: true, force: true });
  });
});

describe("loadBudget", () => {
  test("parses valid budget", () => {
    const root = mkdtempSync(join(tmpdir(), "claudetalk-bud-"));
    const path = join(root, ".file-size-budget.json");
    writeFileSync(
      path,
      JSON.stringify({
        max_lines: 100,
        soft_target: 80,
        exempt: [{ path: "src/old.ts", lines: 200 }],
      }),
    );
    const b = loadBudget(path);
    expect(b.maxLines).toBe(100);
    expect(b.softTarget).toBe(80);
    expect(b.exempt.get("src/old.ts")).toBe(200);
    rmSync(root, { recursive: true, force: true });
  });

  test("throws on missing max_lines", () => {
    const root = mkdtempSync(join(tmpdir(), "claudetalk-bud-"));
    const path = join(root, ".file-size-budget.json");
    writeFileSync(path, JSON.stringify({}));
    expect(() => loadBudget(path)).toThrow(/max_lines/);
    rmSync(root, { recursive: true, force: true });
  });
});

describe("checkFileSizes", () => {
  test("passes when every file is under the ceiling", () => {
    const root = makeTree({
      ".file-size-budget.json": JSON.stringify({ max_lines: 10, soft_target: 5, exempt: [] }),
      "src/a.ts": "a\nb\nc\n",
    });
    const r = checkFileSizes({ rootDir: root, sourceRoots: ["src"] });
    expect(r.violations).toEqual([]);
    expect(r.warnings).toEqual([]);
    rmSync(root, { recursive: true, force: true });
  });

  test("warns when above soft target but under ceiling", () => {
    const root = makeTree({
      ".file-size-budget.json": JSON.stringify({ max_lines: 10, soft_target: 2, exempt: [] }),
      "src/a.ts": "a\nb\nc\nd\n",
    });
    const r = checkFileSizes({ rootDir: root, sourceRoots: ["src"] });
    expect(r.warnings.length).toBe(1);
    expect(r.violations).toEqual([]);
    rmSync(root, { recursive: true, force: true });
  });

  test("violates when above hard ceiling", () => {
    const root = makeTree({
      ".file-size-budget.json": JSON.stringify({ max_lines: 2, exempt: [] }),
      "src/a.ts": "a\nb\nc\nd\n",
    });
    const r = checkFileSizes({ rootDir: root, sourceRoots: ["src"] });
    expect(r.violations.length).toBe(1);
    expect(r.violations[0]!.lines).toBe(4);
    rmSync(root, { recursive: true, force: true });
  });

  test("exempt baseline blocks growth past baseline", () => {
    const root = makeTree({
      ".file-size-budget.json": JSON.stringify({
        max_lines: 1000,
        soft_target: 500,
        exempt: [{ path: "src/big.ts", lines: 3 }],
      }),
      "src/big.ts": "a\nb\nc\nd\n", // 4 lines, over baseline of 3
    });
    const r = checkFileSizes({ rootDir: root, sourceRoots: ["src"] });
    expect(r.violations).toEqual([]);
    expect(r.grownExempt.length).toBe(1);
    expect(r.grownExempt[0]!.baseline).toBe(3);
    expect(r.grownExempt[0]!.lines).toBe(4);
    rmSync(root, { recursive: true, force: true });
  });

  test("formatReport contains hint lines for each failure category", () => {
    const txt = formatReport({
      violations: [{ path: "a.ts", lines: 5, max: 2 }],
      warnings: [{ path: "b.ts", lines: 4 }],
      grownExempt: [{ path: "c.ts", lines: 9, baseline: 5 }],
      maxLines: 2,
      softTarget: 3,
    });
    expect(txt).toContain("exceed the 2-line ceiling");
    expect(txt).toContain("above soft target");
    expect(txt).toContain("grew past their baseline");
  });
});
