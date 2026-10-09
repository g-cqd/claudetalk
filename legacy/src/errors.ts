/**
 * Structured error codes for ClaudeTalk MCP tool failures. Every error
 * response gets a stable identifier so callers (other Claudes) can branch
 * programmatically instead of regex-matching on free text.
 *
 * Wire format: the code is prepended to the human-readable message in
 * square brackets — `[unknown_pseudonym] Unknown pseudonym 'X'.`
 * This keeps the MCP CallToolResult shape unchanged (single text content
 * block + isError flag) while giving callers a deterministic prefix to
 * parse.
 */

export const ErrorCode = {
  UNSPECIFIED: "unspecified",
  UNKNOWN_PSEUDONYM: "unknown_pseudonym",
  UNKNOWN_CHAT: "unknown_chat",
  UNKNOWN_ASK: "unknown_ask",
  UNKNOWN_MESSAGE: "unknown_message",
  NOT_MEMBER: "not_member",
  NOT_ADDRESSEE: "not_addressee",
  ALREADY_ANSWERED: "already_answered",
  FORBIDDEN_SELF_ACTION: "forbidden_self_action",
  INVALID_NICKNAME: "invalid_nickname",
  INVALID_REACTION: "invalid_reaction",
  INVALID_REPLY_TARGET: "invalid_reply_target",
  VALIDATION_FAILED: "validation_failed",
  RATE_LIMITED: "rate_limited",
} as const;

export type ErrorCode = (typeof ErrorCode)[keyof typeof ErrorCode];

/** Build the standard MCP error response. Always includes the structured
 *  `[code]` prefix so callers can branch on it. */
export function toolError(
  message: string,
  code: ErrorCode = ErrorCode.UNSPECIFIED,
): {
  content: Array<{ type: "text"; text: string }>;
  isError: true;
} {
  return {
    content: [{ type: "text" as const, text: `[${code}] ${message}` }],
    isError: true,
  };
}

/** Build the standard MCP success response. */
export function toolText(message: string): {
  content: Array<{ type: "text"; text: string }>;
} {
  return { content: [{ type: "text" as const, text: message }] };
}
