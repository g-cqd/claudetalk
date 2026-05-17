import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  _resetAuditLogForTests,
  flushNow,
  insertToolCall,
  instrumentServer,
  listToolCalls,
} from "../../src/audit-log.ts";
import { resetDb } from "../../src/db.ts";
import { isolatedHome } from "../helpers/tmp.ts";

/** Drain the in-memory audit queue synchronously so reads see fresh rows. */
const flushAudit = () => {
  flushNow();
  return Promise.resolve();
};

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
  resetDb();
});

afterEach(async () => {
  // Drain pending audit writes BEFORE the env var is restored so that any
  // queued rows land in the isolated DB, not the real ~/.claudetalk DB.
  await flushAudit();
  _resetAuditLogForTests();
  resetDb();
  home.cleanup();
});

describe("insertToolCall / listToolCalls", () => {
  test("round-trips a row", () => {
    insertToolCall({
      pseudonym: "X",
      tool: "whoami",
      args_json: "{}",
      result_summary: "You are: X",
      is_error: false,
      error: null,
      started_at: 1000,
      duration_ms: 12,
    });
    const rows = listToolCalls();
    expect(rows.length).toBe(1);
    expect(rows[0]).toMatchObject({
      pseudonym: "X",
      tool: "whoami",
      args_json: "{}",
      result_summary: "You are: X",
      is_error: 0,
      duration_ms: 12,
    });
  });

  test("filters by pseudonym, tool, and sinceId", () => {
    for (const t of ["a", "b", "a", "c"]) {
      insertToolCall({
        pseudonym: t === "c" ? "OTHER" : "ME",
        tool: t,
        args_json: null,
        result_summary: null,
        is_error: false,
        error: null,
        started_at: Date.now(),
        duration_ms: 0,
      });
    }
    expect(listToolCalls({ pseudonym: "ME" }).length).toBe(3);
    expect(listToolCalls({ pseudonym: "OTHER" }).map((r) => r.tool)).toEqual(["c"]);
    expect(listToolCalls({ tool: "a" }).length).toBe(2);

    const all = listToolCalls();
    expect(listToolCalls({ sinceId: all[1]!.id }).map((r) => r.id)).toEqual([
      all[2]!.id,
      all[3]!.id,
    ]);
  });

  test("limit caps the result count", () => {
    for (let i = 0; i < 5; i++) {
      insertToolCall({
        pseudonym: "X",
        tool: "t",
        args_json: null,
        result_summary: null,
        is_error: false,
        error: null,
        started_at: Date.now(),
        duration_ms: 0,
      });
    }
    expect(listToolCalls({ limit: 3 }).length).toBe(3);
  });

  test("records errors as is_error=1 with the error message", () => {
    insertToolCall({
      pseudonym: "X",
      tool: "bad",
      args_json: null,
      result_summary: null,
      is_error: true,
      error: "boom",
      started_at: Date.now(),
      duration_ms: 1,
    });
    const rows = listToolCalls();
    expect(rows[0]!.is_error).toBe(1);
    expect(rows[0]!.error).toBe("boom");
  });
});

describe("instrumentServer", () => {
  function fakeServer() {
    const handlers = new Map<string, any>();
    return {
      registered: handlers,
      registerTool(name: string, _schema: any, handler: any) {
        handlers.set(name, handler);
      },
    };
  }

  test("wraps subsequent registerTool calls so each call is logged", async () => {
    const srv = fakeServer();
    instrumentServer(srv as any, "ALICE");

    srv.registerTool("greet", {}, async ({ who }: { who: string }) => ({
      content: [{ type: "text", text: `hi ${who}` }],
    }));

    const handler = srv.registered.get("greet");
    const res = await handler({ who: "bob" });
    expect(res.content[0].text).toBe("hi bob");

    await flushAudit();
    const logs = listToolCalls();
    expect(logs.length).toBe(1);
    expect(logs[0]).toMatchObject({
      pseudonym: "ALICE",
      tool: "greet",
      is_error: 0,
    });
    expect(logs[0]!.args_json).toContain('"who":"bob"');
    expect(logs[0]!.result_summary).toBe("hi bob");
    expect(logs[0]!.duration_ms).toBeGreaterThanOrEqual(0);
  });

  test("logs is_error=1 when the handler returns { isError: true }", async () => {
    const srv = fakeServer();
    instrumentServer(srv as any, "X");
    srv.registerTool("fail", {}, async () => ({
      content: [{ type: "text", text: "nope" }],
      isError: true,
    }));
    await srv.registered.get("fail")({});
    await flushAudit();
    const rows = listToolCalls();
    expect(rows[0]!.is_error).toBe(1);
  });

  test("logs is_error=1 with error message when handler throws", async () => {
    const srv = fakeServer();
    instrumentServer(srv as any, "X");
    srv.registerTool("boom", {}, async () => {
      throw new Error("kaboom");
    });
    let threw = false;
    try {
      await srv.registered.get("boom")({});
    } catch (e) {
      threw = true;
      expect((e as Error).message).toBe("kaboom");
    }
    expect(threw).toBe(true);
    await flushAudit();
    const rows = listToolCalls();
    expect(rows[0]!.is_error).toBe(1);
    expect(rows[0]!.error).toBe("kaboom");
  });

  test("truncates oversized args and result summaries", async () => {
    const srv = fakeServer();
    instrumentServer(srv as any, "X");
    const bigArg = "x".repeat(5_000);
    const bigText = "y".repeat(5_000);
    srv.registerTool("big", {}, async () => ({
      content: [{ type: "text", text: bigText }],
    }));
    await srv.registered.get("big")({ blob: bigArg });
    await flushAudit();
    const rows = listToolCalls();
    expect(rows[0]!.args_json!.length).toBeLessThanOrEqual(1_000);
    expect(rows[0]!.args_json!.endsWith("...")).toBe(true);
    expect(rows[0]!.result_summary!.length).toBeLessThanOrEqual(2_000);
    expect(rows[0]!.result_summary!.endsWith("...")).toBe(true);
  });
});
