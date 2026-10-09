/**
 * AsyncLocalStorage-scoped per-request identity, used by the HTTP MCP
 * path on the relay so a single shared McpServer can serve tool calls
 * from many bearer-token identities. Stdio MCP (one process per Claude
 * Code session) doesn't need ALS — it captures `me` in a closure and
 * the fallback wins.
 *
 * Tool handlers SHOULD call `currentIdentity(me)` instead of using
 * `me` directly so they work in both deployment shapes without
 * branching at every call site.
 */
import { AsyncLocalStorage } from "node:async_hooks";
import type { Identity } from "./pseudonym.ts";

export const identityContext = new AsyncLocalStorage<Identity>();

/** Return the identity scoped to the current async context if any
 *  (HTTP MCP request handler sets this), else the static fallback
 *  (stdio MCP server uses its registration-time `me`). */
function currentIdentity(staticFallback: Identity): Identity {
  return identityContext.getStore() ?? staticFallback;
}

/** Wrap a static Identity in a Proxy so every property read resolves
 *  via `currentIdentity(staticFallback)`. Lets the existing tool
 *  handlers keep their `me.pseudonym` / `me.path` / `me.keyPair`
 *  accesses unchanged while transparently picking up the ALS-scoped
 *  identity when the HTTP MCP path sets one. Stdio MCP gets the
 *  static one (ALS empty); zero behavioural change. */
export function dynamicIdentity(staticFallback: Identity): Identity {
  return new Proxy(staticFallback, {
    get(_target, prop) {
      const id = currentIdentity(staticFallback);
      return id[prop as keyof Identity];
    },
  }) as Identity;
}
