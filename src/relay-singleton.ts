/**
 * Module-level handle to the active "publisher" used by chat-tools to
 * route a locally-inserted message out to the relay (in the stdio MCP
 * server process) or to the relay's own broadcast pipeline (in the
 * relay process itself, via a loopback).
 *
 * Both shapes satisfy the same `Publisher` interface so chat-tools
 * doesn't care which one it's talking to.
 */

export interface Publisher {
  publishMessage(args: {
    messageId: string;
    chatId: string;
    body: string;
    createdAt: number;
    signature: string;
  }): Promise<number | null>;
  close(): void;
}

let _publisher: Publisher | null = null;

export function setRelayClient(c: Publisher | null): void {
  _publisher = c;
}

export function getRelayClient(): Publisher | null {
  return _publisher;
}
