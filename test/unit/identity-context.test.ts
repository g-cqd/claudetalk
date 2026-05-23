/**
 * Phase N1b-tools-4: AsyncLocalStorage-scoped identity. Proxies the
 * static `me` passed to register* functions so handler bodies pick up
 * the per-request identity when one is set (HTTP MCP path on the
 * relay), or fall back to static (stdio MCP).
 */
import { expect, test } from "bun:test";
import { dynamicIdentity, identityContext } from "../../src/identity-context.ts";
import type { Identity } from "../../src/pseudonym.ts";

const STATIC: Identity = {
  pseudonym: "StaticStag-001",
  path: "/static",
  hash: "deadbeef",
};

const REQ: Identity = {
  pseudonym: "DynamicDolphin-fff",
  path: "/dynamic",
  hash: "f00dface",
};

test("dynamicIdentity returns the static fallback when ALS is empty", () => {
  const proxy = dynamicIdentity(STATIC);
  expect(proxy.pseudonym).toBe("StaticStag-001");
  expect(proxy.path).toBe("/static");
  expect(proxy.hash).toBe("deadbeef");
});

test("dynamicIdentity returns the ALS-scoped identity inside .run()", () => {
  const proxy = dynamicIdentity(STATIC);
  identityContext.run(REQ, () => {
    expect(proxy.pseudonym).toBe("DynamicDolphin-fff");
    expect(proxy.path).toBe("/dynamic");
    expect(proxy.hash).toBe("f00dface");
  });
});

test("dynamicIdentity restores fallback after .run() exits", () => {
  const proxy = dynamicIdentity(STATIC);
  identityContext.run(REQ, () => {
    expect(proxy.pseudonym).toBe("DynamicDolphin-fff");
  });
  expect(proxy.pseudonym).toBe("StaticStag-001");
});

test("dynamicIdentity isolates concurrent ALS scopes", async () => {
  const proxy = dynamicIdentity(STATIC);
  const a: Identity = { pseudonym: "A", path: "/a", hash: "a" };
  const b: Identity = { pseudonym: "B", path: "/b", hash: "b" };
  const results = await Promise.all([
    identityContext.run(a, async () => {
      await Bun.sleep(5);
      return proxy.pseudonym;
    }),
    identityContext.run(b, async () => {
      await Bun.sleep(5);
      return proxy.pseudonym;
    }),
  ]);
  expect(results).toEqual(["A", "B"]);
});

test("dynamicIdentity survives await boundaries inside a single .run()", async () => {
  const proxy = dynamicIdentity(STATIC);
  await identityContext.run(REQ, async () => {
    expect(proxy.pseudonym).toBe("DynamicDolphin-fff");
    await Bun.sleep(10);
    expect(proxy.pseudonym).toBe("DynamicDolphin-fff"); // ALS persists through await
  });
});
