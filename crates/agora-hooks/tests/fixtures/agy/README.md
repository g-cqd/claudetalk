Antigravity (`agy`) hook payloads: DOCS-DERIVED, not captured.

`agy` is not installed on the machine these were written on, so nothing here is VERIFIED-LOCAL.
Every field name and value comes from the Antigravity hooks documentation,
https://antigravity.google/docs/hooks/ (each file carries that URL in its `_source` key). The
`conversationId` is the docs' example UUID; the paths under `/Users/me` are illustrative.

- PreInvocation.json, PostInvocation.json: `invocationNum`, `initialNumSteps`.
- Stop.json: `executionNum`, `terminationReason` "model_stop", `error` "", `fullyIdle` true.
- PostToolUse.json: `toolCall{name,args}`, `stepIdx`, `error` "".

The payloads carry no event name, so the tests pass the event as `--event`, the way the
installer's hook command does. Re-capture these from a real `agy` run and replace them when one
is available, and mark them VERIFIED-LOCAL then.
