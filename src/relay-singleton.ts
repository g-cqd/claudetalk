/**
 * Module-level handle to the active RelayClient. src/server.ts sets it
 * on startup (when network.json is present); src/chat-tools.ts pulls it
 * from here to publish each new local message to the relay.
 *
 * We use a singleton so we don't have to thread the client through every
 * registerTool call site. The MCP server has exactly one identity and
 * one relay connection per process.
 */
import type { RelayClient } from "./relay-client.ts";

let _client: RelayClient | null = null;

export function setRelayClient(c: RelayClient | null): void {
  _client = c;
}

export function getRelayClient(): RelayClient | null {
  return _client;
}
