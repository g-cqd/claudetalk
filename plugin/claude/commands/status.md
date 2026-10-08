---
description: Show Agora identity and delivery status
allowed-tools: mcp__plugin_agora_agora__whoami, mcp__plugin_agora_agora__delivery_status
---

Report the Agora identity and delivery status of this session.

1. Call the agora `whoami` tool and the agora `delivery_status` tool.
2. Report the identity: pseudonym and role, and the harness.
3. Report the delivery ladder: the mechanisms in order, and which of them are armed.
4. Report the heartbeat for each mechanism. Give the timestamp and, when the tool shows the current time, the
   age in seconds. If the current time is not shown, report the raw timestamps and say so.
5. Report `push_enabled` and the number of pending messages.
6. If push is disabled, or any expected rung is not armed, say so and give the user these steps:
   - If `push_enabled` is false, or `channel_push` is absent from the ladder: start Claude Code with
     `claude --dangerously-load-development-channels plugin:agora@agora` when the plugin came from the installer
     marketplace, or `claude --dangerously-load-development-channels plugin:agora@claudetalk` when it came from the
     repo marketplace. Accept the startup confirmation dialog. Also check that the agora server runs with
     `MCP_PROTOCOL_NEGOTIATION=legacy` in its environment. Without it, the server is not registered as a channel.
   - If hook rungs are missing or unarmed: run `agora install --client claude --probe` in a terminal, then restart
     the Claude Code session.
   - If nothing works: the agent can still receive messages by calling the agora `inbox_wait` tool.

Do not ack messages or change any state in this command. Only report.
