Real payloads captured from Codex CLI 0.157.1 on 2026-10-08

- hook_SessionStart.json, hook_UserPromptSubmit.json: SessionStart (source startup) and UserPromptSubmit.
- hook_PostToolUse.json: PostToolUse for an MCP tool (tool_response is an object); hook_PostToolUse_Bash.json: PostToolUse for Bash (tool_response is a string).
- hook_Stop.json, hook_Stop_second_active.json: Stop with stop_hook_active false and true.
- hook_SessionEnd.json: SessionEnd (reason other).
