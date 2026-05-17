import { describe, expect, test } from "bun:test";
import { pseudonymFor } from "../../src/pseudonym.ts";

describe("pseudonymFor", () => {
  test("is deterministic for the same path", () => {
    const a = pseudonymFor("/tmp/alice");
    const b = pseudonymFor("/tmp/alice");
    expect(a.pseudonym).toBe(b.pseudonym);
    expect(a.hash).toBe(b.hash);
  });

  test("returns the absolute path verbatim", () => {
    const id = pseudonymFor("/tmp/whatever");
    expect(id.path).toBe("/tmp/whatever");
  });

  test("produces a 64-character lowercase hex hash", () => {
    const id = pseudonymFor("/x");
    expect(id.hash).toMatch(/^[0-9a-f]{64}$/);
  });

  test("matches the {Adjective}{Animal}-XXX shape", () => {
    const id = pseudonymFor("/x");
    expect(id.pseudonym).toMatch(/^[A-Z][a-z]+[A-Z][a-z]+-[0-9a-f]{3}$/);
  });

  test("different paths almost always produce different pseudonyms", () => {
    const seen = new Set<string>();
    for (let i = 0; i < 500; i++) {
      seen.add(pseudonymFor(`/tmp/folder-${i}`).pseudonym);
    }
    // With ~67 adjectives × ~70 animals × 4096 suffixes, 500 paths essentially
    // never collide. Allow a tiny margin for the birthday paradox.
    expect(seen.size).toBeGreaterThanOrEqual(498);
  });

  test("the suffix is the first 3 hex chars after the byte used for animal index", () => {
    // Smoke check: hash slice 16..19 hex == suffix
    const id = pseudonymFor("/tmp/x");
    const suffix = id.pseudonym.split("-")[1]!;
    expect(suffix).toBe(id.hash.slice(16, 19));
  });

  test("a stable known input has a stable known pseudonym", () => {
    // Locks in the algorithm: if the word lists or hashing scheme changes,
    // this assertion breaks — forcing the maintainer to acknowledge a
    // breaking change in pseudonym derivation.
    const id = pseudonymFor("/tmp/alice-project");
    expect(id.pseudonym).toBe("AzureMole-317");
    expect(id.hash).toBe(
      "2c02cce3ba7758fb317137087d09adf2cf4a7d49d1c77c86ce7fc75f46564286",
    );
  });
});
