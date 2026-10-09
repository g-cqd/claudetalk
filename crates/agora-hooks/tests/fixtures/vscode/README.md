VS Code agent-mode hook fixtures: DOCS-DERIVED, NOT CAPTURED

These payloads are built from the documented VS Code "Local" hook schema
(code.visualstudio.com/docs/agents/reference/hooks-reference) and the field names found in the
installed VS Code 1.141.0 / Copilot Chat 0.69.0 bundle (`extension.js`, `executeHook`). No real
payload was captured from a running VS Code: they show the documented shapes, with realistic
values (a uuid session, `/Users/me/proj` as cwd, ISO timestamps).

- SessionStart.json: `source` is `"new"` (the VS Code value that `dialect::vscode` maps to
  `"startup"`), plus `model` and `agent_type`.
- UserPromptSubmit.json: `prompt`.
- PostToolUse.json: `tool_name`, `tool_input`, `tool_response` as an object (MCP-style content
  array), `tool_use_id`.
- Stop.json / Stop_active.json: `stop_hook_active` false and true.
- SubagentStop.json: `agent_id`, `agent_type`, `stop_hook_active`.

UNVERIFIED: the MCP tool name VS Code uses in `tool_name`. `mcp_agora_chat` is a plausible
placeholder, not an observed name. Agora's inbox detection (`context::is_inbox_tool`) matches the
Claude/Codex form `mcp__<server>__<tool>`, so a real VS Code name must be captured before that
matching is relied on for this dialect.

Each file carries a top-level `_source` string naming the origin. Agora's parser ignores it.
