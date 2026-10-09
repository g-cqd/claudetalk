# Copilot CLI hook fixtures

Two kinds of payload live here. The file name and the `_source` key tell them apart.

## REAL: Copilot CLI 0.0.369 (VERIFIED-LOCAL)

Captured from the installed binary with a mock Copilot API (the account has no Copilot
entitlement, so there was no real model turn). Copied byte-for-byte from
`/tmp/agora-m6/fixtures/copilot/`. They carry no `_source` key and no `sessionId`.

- `hook_sessionStart.json`, `hook_postToolUse.json`, `hook_userPromptSubmitted.json`,
  `hook_sessionEnd.json`, `hook_preToolUse.json`, `hook_errorOccurred.json`

The CLI does not put the event name in the payload. The event comes from the hook command
(`--event`), so the tests pass it in. 0.0.369 ignores all hook output. A payload with no
`sessionId` therefore maps to `copilot:<event>`, which nothing runs for (SPEC 2.2, LEGACY RULE).

`hook_config_example.json` (the probe's hook config) is not a payload and is not used by the tests.

## DOCS-DERIVED: newer Copilot CLI (not captured)

Built from the GitHub docs, <https://docs.github.com/en/copilot/reference/hooks-reference>, for
Copilot CLI >= 1.0.72. Each file carries a `_source` key. They are not captured payloads:

- `newer_sessionStart.json`: `sessionId`, `source`, `initialPrompt`.
- `newer_postToolUse.json`: `toolName` `agora-inbox`, and `toolResult` with `resultType` and
  `textResultForLlm`.
- `newer_agentStop.json`: `transcriptPath`, `stopReason`, `stop_hook_active` false.
- `newer_agentStop_active.json`: the same with `stop_hook_active` true.

The docs say `stop_hook_active` stays snake_case even in the camelCase format. The parser also
accepts `stopHookActive` as an alias. The tests cover that alias with an inline payload.
