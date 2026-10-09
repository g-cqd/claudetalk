# Cursor hook fixtures (DOCS-DERIVED)

DOCS-DERIVED: Cursor is not installed on the machine that wrote these files, so no payload here
was captured from a real Cursor binary. Each file is built from the hook documentation at
https://cursor.com/docs/hooks (the field names and event names in SPEC 2.6). Every file carries a
`"_source"` key with that URL; the dialect parser ignores unknown keys.

The values (ids, `cursor_version`, durations, `user_email`) are placeholders. `user_email` is a
fake address on purpose. Replace the fixtures with real captures once Cursor is available and
verified (VERIFIED-LOCAL).

- `sessionStart.json`: sessionStart, with `session_id`, `is_background_agent`, `composer_mode`.
- `postToolUse.json`: postToolUse for a built-in tool; `tool_output` is a string.
- `afterMCPExecution.json`: afterMCPExecution for the `agora` server's `inbox` tool;
  `tool_input` and `result_json` are JSON strings.
- `stop.json`: stop with `loop_count` 0 (first continuation).
- `stop_loop1.json`: stop with `loop_count` 1 (the hook already continued once).
- `beforeSubmitPrompt.json`: an event Agora ignores (cannot add context in Cursor).
