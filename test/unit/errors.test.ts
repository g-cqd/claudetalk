import { describe, expect, test } from "bun:test";
import { ErrorCode, toolError, toolText } from "../../src/errors.ts";

describe("toolError", () => {
  test("prepends [code] when explicit", () => {
    const r = toolError("foo", ErrorCode.UNKNOWN_PSEUDONYM);
    expect(r.isError).toBe(true);
    expect(r.content[0]!.text).toBe("[unknown_pseudonym] foo");
  });

  test("defaults to [unspecified] when no code given", () => {
    const r = toolError("bar");
    expect(r.content[0]!.text).toBe("[unspecified] bar");
  });

  test("toolText returns success shape (no isError)", () => {
    const r = toolText("ok");
    expect(r.content[0]!.text).toBe("ok");
    expect((r as any).isError).toBeUndefined();
  });

  test("error codes catalog exposes the expected stable strings", () => {
    expect(ErrorCode.RATE_LIMITED).toBe("rate_limited");
    expect(ErrorCode.UNKNOWN_CHAT).toBe("unknown_chat");
    expect(ErrorCode.NOT_MEMBER).toBe("not_member");
  });
});
