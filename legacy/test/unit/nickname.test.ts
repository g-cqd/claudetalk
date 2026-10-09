import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { addChatMember, ensureChat, groupChatId, resetDb } from "../../src/db.ts";
import {
  castGroupNicknameVote,
  displayBoth,
  displayName,
  NicknameError,
  setPersonalNickname,
  validateNickname,
} from "../../src/nickname.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
  resetDb();
});
afterEach(() => {
  resetDb();
  home.cleanup();
});

describe("validateNickname", () => {
  test("accepts simple identifiers", () => {
    for (const ok of ["bob", "Alice", "a", "x_1", "lead-dev", "A".repeat(30)]) {
      expect(validateNickname(ok)).toBe(ok);
    }
  });

  test("trims whitespace", () => {
    expect(validateNickname("  bob  ")).toBe("bob");
  });

  test.each([
    "",
    "  ",
    "1abc", // starts with digit
    "-x", // starts with dash
    "bob bob", // space inside
    "bob!", // punctuation
    "A".repeat(31), // too long
  ])("rejects bad input %p", (bad) => {
    expect(() => validateNickname(bad)).toThrow(NicknameError);
  });

  test("rejects strings shaped like a pseudonym", () => {
    expect(() => validateNickname("SwiftFox-a3f")).toThrow(/cannot look like a pseudonym/);
    expect(() => validateNickname("AmberCrow-5ad")).toThrow(NicknameError);
  });
});

describe("displayName resolution", () => {
  test("falls back to the pseudonym when nothing is set", () => {
    expect(displayName("V", "T")).toBe("T");
    expect(displayName("V", "T", null)).toBe("T");
  });

  test("returns the target as-is when viewer === target (never alias self)", () => {
    setPersonalNickname("V", "V", "self-name-attempt");
    expect(displayName("V", "V")).toBe("V");
  });

  test("personal nickname takes precedence over no chat context", () => {
    setPersonalNickname("V", "T", "bob");
    expect(displayName("V", "T")).toBe("bob");
  });

  test("personal nickname WINS over an active group nickname", () => {
    const cid = groupChatId("g1");
    ensureChat(cid, "group", null);
    addChatMember(cid, "V");
    addChatMember(cid, "T");
    castGroupNicknameVote(cid, "T", "V", "in-group");
    castGroupNicknameVote(cid, "T", "T", "in-group"); // target ratified → active
    setPersonalNickname("V", "T", "private-name");
    expect(displayName("V", "T", cid)).toBe("private-name");
  });

  test("group nickname only activates when ≥2 votes AND target voted", () => {
    const cid = groupChatId("g2");
    ensureChat(cid, "group", null);
    addChatMember(cid, "V");
    addChatMember(cid, "T");
    addChatMember(cid, "U");

    // Only one voter: not active
    castGroupNicknameVote(cid, "T", "V", "x");
    expect(displayName("V", "T", cid)).toBe("T");

    // Two voters but target hasn't voted: not active
    castGroupNicknameVote(cid, "T", "U", "x");
    expect(displayName("V", "T", cid)).toBe("T");

    // Target ratifies: active
    castGroupNicknameVote(cid, "T", "T", "x");
    expect(displayName("V", "T", cid)).toBe("x");
  });

  test("if multiple nicknames tie ≥2+target, the most recently voted wins", () => {
    const cid = groupChatId("g3");
    ensureChat(cid, "group", null);
    addChatMember(cid, "V");
    addChatMember(cid, "T");
    addChatMember(cid, "U");
    castGroupNicknameVote(cid, "T", "T", "first");
    castGroupNicknameVote(cid, "T", "U", "first"); // 'first' active
    expect(displayName("V", "T", cid)).toBe("first");
    Bun.sleepSync(5);
    castGroupNicknameVote(cid, "T", "T", "second");
    castGroupNicknameVote(cid, "T", "V", "second"); // 'second' now active and newer
    expect(displayName("V", "T", cid)).toBe("second");
  });

  test("group nickname does NOT leak outside its chat", () => {
    const cid = groupChatId("inside");
    const other = groupChatId("outside");
    ensureChat(cid, "group", null);
    ensureChat(other, "group", null);
    addChatMember(cid, "V");
    addChatMember(cid, "T");
    addChatMember(other, "V");
    addChatMember(other, "T");
    castGroupNicknameVote(cid, "T", "V", "in-cid");
    castGroupNicknameVote(cid, "T", "T", "in-cid");
    expect(displayName("V", "T", cid)).toBe("in-cid");
    expect(displayName("V", "T", other)).toBe("T");
    expect(displayName("V", "T", null)).toBe("T");
  });
});

describe("displayBoth", () => {
  test("returns just the pseudonym when no nickname is set", () => {
    expect(displayBoth("V", "T")).toBe("T");
  });
  test("returns 'name (pseudonym)' when a personal nickname exists", () => {
    setPersonalNickname("V", "T", "bob");
    expect(displayBoth("V", "T")).toBe("bob (T)");
  });
});
