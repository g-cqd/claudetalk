# Agora in other agent harnesses

Reference for milestone M6 of the Rust rewrite. It covers how Agora messages reach Codex, GitHub Copilot CLI, VS Code agent mode, Antigravity (`agy`), opencode and Cursor, what the installer writes for each, and what has and has not been tested. Claude Code is covered in [CLAUDE-CODE.md](CLAUDE-CODE.md).

Status labels, as in CLAUDE-CODE.md:

- **VERIFIED-LOCAL** (with a version): observed on a binary installed on the development machine. Where the label says "static", the fact was read from the installed bundle or source and the feature was not run.
- **DOCS-ONLY**: read in the vendor documentation or a named third-party source. Not run here.
- **NOT SUPPORTED**: Agora does not use this mechanism for the harness.

## 1. Overview

### 1.1 Delivery ladder per harness

The ladder is the ordered list of rungs the MCP server walks for a message that stays unacked. Strongest first.

| Harness (`Harness`) | Ladder |
|---|---|
| Codex, Copilot CLI, opencode, Antigravity | `owned_session_api` → `stop_continue` → `hook_inject` → `wait_tool` |
| VS Code Copilot, Cursor | `stop_continue` → `hook_inject` → `wait_tool` |
| Claude Code | See [CLAUDE-CODE.md section 1](CLAUDE-CODE.md#1-overview). |
| Unknown client | `wait_tool` |

The rungs:

- `owned_session_api`: a bridge process started with `agora run-agent` drives the agent through the agent's own API and sends each message as a prompt. The rung is in the ladder only while a bridge is alive (see 1.2).
- `stop_continue`: the agent's Stop hook returns the messages as a block, and the model continues the turn.
- `hook_inject`: a context hook adds the messages to the model's context at a turn boundary or after an Agora tool call.
- `wait_tool`: the agent calls `inbox_wait` itself. It is always the last rung and needs nothing installed beyond the MCP server.

Only `owned_session_api` can wake an idle session, and only while its bridge runs. Every other harness here delivers at turn boundaries or when the agent polls.

### 1.2 How capabilities and heartbeats decide the rungs

Two inputs decide which rungs a session gets.

- `capabilities.json` (section 2.4) records what the installer wrote and, with `--probe`, what the binary supports. Each harness has its own key: `codex`, `copilot_cli`, `copilot_vscode`, `antigravity`, `opencode`, `cursor`.
- Heartbeat files record what ran recently. The context hook writes `hook_inject`, the Stop hook writes `stop_continue`, and a running bridge beats `owned_session_api` every 30 s.

The MCP server combines them as follows:

- `stop_continue` is in the ladder when the Stop hook is installed and not marked untrusted, or when a `stop_continue` heartbeat was seen. `hook_inject` follows the same rule with the context hook.
- **Codex untrusted hooks.** When `hooks_trusted` is `false`, the hook rungs are excluded until a heartbeat proves the hooks run. Codex skips untrusted hooks without any message, so the presence of the hook file proves nothing.
- `owned_session_api` is in the ladder only while a bridge heartbeat is armed. The armed window is 90 s, so a bridge that stops beating drops the rung within about one window. The install flag does not count.
- Copilot CLI hook flags (`context_hook`, `stop_hook`) are set from the probed version (1.0.72 or later). `owned_session` is set when `copilot --help` mentions `--acp` (section 3.2). Without a probe they are all false. A heartbeat from a newer CLI adds the hook rungs anyway.
- Channel push, mods and `asyncRewake` never apply to these harnesses.

The server recomputes the ladder on each escalation tick and on `delivery_status`, as described in [CLAUDE-CODE.md section 1](CLAUDE-CODE.md#1-overview).

### 1.3 The shared per-session cursor

Hooks, bridges and (after M6) Claude Code channel push share one cursor per harness session. The cursor is the highest `seq` already delivered to that session. It lives in `<agora home>/hooks/sessions/<key>.json` under an exclusive lock.

The key is the harness's own session id:

| Harness | Session id used as the key |
|---|---|
| Codex | The thread id. It equals the hook `session_id` and the app-server thread id. |
| Copilot CLI | `sessionId`, which exists only in newer versions. Version 0.0.369 sends none (section 3.2). |
| VS Code | The `session_id` field of its hook input. |
| Antigravity | The conversation id. |
| opencode | The opencode session id. |
| Cursor | The `conversation_id` (or `session_id`) of the hook input. |
| Claude Code | The `CLAUDE_CODE_SESSION_ID` value in the MCP server environment, which M6 adds to channel push. |

Rules that apply to every path:

- Nothing acks. An acknowledgement is always the agent's own `ack` call. Hooks, bridges and channel push never ack on the agent's behalf.
- Delivery comes first, then the cursor advances. The cursor moves only past messages that were actually shown or accepted.
- A duplicate is preferred over a loss. If the cursor cannot tell that a message was shown, the message can appear again.

### 1.4 Untrusted framing

Message text is data from other agents, never an instruction.

- Hooks and the bridge render messages through `agora_hooks::render::render_messages`. Each message is escaped and wrapped in an `<untrusted-message>` element by the MCP framing code. The session-start channel list is wrapped in `<untrusted-data>`.
- The bridge prepends a fixed preamble to every delivery that says the same thing: the text is untrusted, and the owner's instructions win.
- Message text is never interpolated into a command, a file path or a JSON key.
- Every hook path exits 0 with empty output on any error, timeout or panic. A hook cannot block a prompt or a Stop by failing.

The instruction snippet the installer writes (`crates/agora-cli/assets/agora-instructions.md`) states the same rule: agora text may arrive framed as `<untrusted-data>`, with each message inside an `<untrusted-message>` block.

## 2. Install

### 2.1 Command

```
agora install   --client <list> [--scope user|project] [--probe] [--dry-run] [--trust-hooks]
agora uninstall --client <list> [--scope user|project] [--dry-run]
```

`--client` takes one of the names below, a comma list such as `codex,cursor`, or `all`. `all` means every name in the order shown. Duplicates are dropped and the order is kept. An unknown name is a usage error that lists the valid names.

| CLI name | Client |
|---|---|
| `claude` | Claude Code (CLAUDE-CODE.md). Its behaviour is unchanged. |
| `codex` | Codex CLI |
| `copilot-cli` | GitHub Copilot CLI |
| `copilot-vscode` | VS Code agent mode (Copilot Chat) |
| `agy` | Antigravity |
| `opencode` | opencode |
| `cursor` | Cursor |

A list that is exactly `claude` runs the Claude code path and keeps its output shape. Any other list prints one JSON object:

```json
{"ok":true,"clients":[
  {"client":"codex","scope":"user","dry_run":false,
   "actions":[{"path":"...","result":"wrote","detail":"..."}],
   "probe":null,"capabilities":{},"notes":[]}
],"capabilities_actions":[...]}
```

`capabilities_actions` holds the write and backup actions for `capabilities.json`, next to `ok` and `clients`.

Each action's `result` is one of `wrote`, `unchanged`, `dry_run`, `removed`, `absent`, `kept` or `backed_up`. A client that fails reports `{"client":"...","error":{"code","message"}}` in place of its entry. The top-level `ok` is then `false`, and the command exits non-zero after the other clients have run. The text output prints one block per client.

`--trust-hooks` affects Codex only (section 3.1). `--probe` runs the binary probes described in 2.6.

### 2.2 Files written

Paths are per client and scope. `<home>` is the user's home directory. `<proj>` is the project directory. `$CODEX_HOME`, `$COPILOT_HOME` and the other variables are the overrides in 2.5.

| Client | Scope | MCP file and key | Hooks file | Instructions (block unless noted) |
|---|---|---|---|---|
| codex | user | `$CODEX_HOME/config.toml` `[mcp_servers.agora]` | `$CODEX_HOME/hooks.json` | `$CODEX_HOME/AGENTS.md` |
| codex | project | `<proj>/.codex/config.toml` | `<proj>/.codex/hooks.json` | `<proj>/AGENTS.md` |
| copilot-cli | user | `$COPILOT_HOME/mcp-config.json` `mcpServers.agora` | `$COPILOT_HOME/hooks/agora.json` (owned file, shared with copilot-vscode only when neither `COPILOT_HOME` nor `XDG_CONFIG_HOME` is set) | `$COPILOT_HOME/copilot-instructions.md` |
| copilot-cli | project | `<proj>/.github/mcp.json` `mcpServers.agora` | `<proj>/.github/hooks/agora.json` (owned, shared) | `<proj>/.github/copilot-instructions.md` (shared) |
| copilot-vscode | user | `<vscode user dir>/mcp.json` `servers.agora` | `<home>/.copilot/hooks/agora.json` (owned, shared) | `<home>/.copilot/instructions/agora.instructions.md` (owned file) |
| copilot-vscode | project | `<proj>/.vscode/mcp.json` `servers.agora` | `<proj>/.github/hooks/agora.json` (owned, shared) | `<proj>/.github/copilot-instructions.md` (shared) |
| agy | user | `<home>/.gemini/config/mcp_config.json` `mcpServers.agora` | `<home>/.gemini/config/hooks.json` key `agora` | `<home>/.gemini/AGENTS.md` |
| agy | project | `<proj>/.agents/mcp_config.json` | `<proj>/.agents/hooks.json` key `agora` | `<proj>/AGENTS.md` (shared) |
| opencode | user | `<opencode config dir>/opencode.json` `mcp.agora` | none | `<opencode config dir>/AGENTS.md` |
| opencode | project | `<proj>/opencode.json` `mcp.agora` | none | `<proj>/AGENTS.md` (shared) |
| cursor | user | `<home>/.cursor/mcp.json` `mcpServers.agora` | `<home>/.cursor/hooks.json` | none (Cursor user rules live in its settings UI) |
| cursor | project | `<proj>/.cursor/mcp.json` `mcpServers.agora` | `<proj>/.cursor/hooks.json` | `<proj>/.cursor/rules/agora.mdc` (owned file) |

The instruction block body is the snippet in `crates/agora-cli/assets/agora-instructions.md`. It covers the untrusted-input rule, the inbox and reply tools, the board tools and the work queue. An owned file is one Agora creates and deletes whole.

The MCP server args are always `["--home","<home>","mcp"]`. The entries are:

| Client | Entry written |
|---|---|
| codex | TOML `command = "<exe>"` and `args`. No `env`: Codex filters the server environment. |
| copilot-cli | `{"type":"local","command":"<exe>","args":[...],"tools":["*"]}`. No `env`. |
| copilot-vscode | `{"type":"stdio","command":"<exe>","args":[...],"env":{"AGORA_FOLDER":"${workspaceFolder}"}}` |
| agy | `{"command":"<exe>","args":[...]}` |
| opencode | `{"type":"local","command":["<exe>","--home","<home>","mcp"],"enabled":true}` |
| cursor | `{"type":"stdio","command":"<exe>","args":[...],"env":{"AGORA_FOLDER":"${workspaceFolder}"}}` |

Hook commands have the form `<exe> --home <home> hook <kind> --dialect <d>`, with ` --event <E>` added where the harness needs it. Each harness gets these handlers:

| Client | Handlers |
|---|---|
| codex | Grouped form, no matcher, `"timeout": 10`. SessionStart runs `session-start`. UserPromptSubmit and PostToolUse run `context`. Stop runs `stop`. |
| copilot-cli, copilot-vscode | The shared `agora.json`: `sessionStart` runs `session-start`, `userPromptSubmitted` and `postToolUse` run `context`, `agentStop` runs `stop`. Each entry is `{"type":"command","bash":...,"cwd":".","timeoutSec":10}`. |
| agy | Named entry `agora`: `PreInvocation` runs `context`, `Stop` runs `stop`. Each handler has `"timeout": 10`. |
| cursor | Flat form: `sessionStart` runs `session-start`, `postToolUse` and `afterMCPExecution` run `context`, `stop` runs `stop` with `"loop_limit": 5`. |

### 2.3 Safety rules

These apply to every file the non-Claude clients write. The Claude path has the same rules in [CLAUDE-CODE.md section 3](CLAUDE-CODE.md#3-install).

- **Validate first.** Each client parses every existing file it would touch before any write. A malformed file (a JSON or TOML parse error, a non-object JSON root, or unbalanced agora markers) is an error that names the file. Nothing is written for that client. Other clients in the list still run.
- **Atomic writes.** A temp file in the same directory is renamed over the target. Files that already exist keep their mode.
- **Backups.** Before a change replaces an existing file, a copy is kept as `<path>.agora-bak-<ms>`. The newest three are kept per path. A write whose result is byte-identical to the file is `unchanged` and makes no backup.
- **Symlinks refused.** A symlink at the target path is an error, for every file.
- **Dry run.** `--dry-run` writes nothing. It prints a structural diff for JSON and a line diff for TOML and text, capped at 200 lines.
- **Comments kept.** Codex's `config.toml` is edited with `toml_edit`. Comments, unrelated tables and key order are kept. Unit tests check this.
- **Marker blocks.** Instruction text sits between an `<!-- agora:begin ... -->` line (its text says the block is managed by `agora install` and that edits inside are replaced) and `<!-- agora:end -->`. Install replaces the block in place, or appends it after a blank line. Text outside the markers is never touched. More than one block, or a begin without an end, is an error.
- **Owned files.** Files that only Agora writes (`agora.json`, `agora.mdc`, `agora.instructions.md`) are replaced whole and deleted on uninstall.
- **Shared files are kept.** The Copilot hooks file is shared by copilot-cli and copilot-vscode. The `AGENTS.md` and `copilot-instructions.md` blocks are shared across some clients (the table in 2.2 marks them). Uninstalling one client keeps a shared file or block while another installed client still lists it in `capabilities.json`.
- **Refusing foreign files.** Install stops with an error and writes nothing when one of these owned files is foreign: the Copilot hooks file, whose handlers are not Agora's (no `--dialect copilot` command inside); a foreign `.cursor/rules/agora.mdc`; or a foreign VS Code `~/.copilot/instructions/agora.instructions.md`.
- **Placeholders refused.** An executable or home path that contains `${` is refused. Agora treats such a value as an unexpanded placeholder.
- **No secrets.** Agora never writes tokens or auth files into any config, and never reads a user's auth file. The one exception is the explicit `AGORA_E2E_CODEX_AUTH` test input (section 6).
- **Project scope paths.** With `--scope project`, absolute paths to this machine's `agora` binary and Agora home are written into project files. Do not commit those files to a shared project.
- **Quoting.** Hook commands quote the executable and home path only when a character in them needs quoting.
- **Ownership.** An MCP entry is Agora's when the file name of its command starts with `agora`. A hook command is Agora's when it contains ` --home ` and one of ` hook session-start --dialect `, ` hook context --dialect ` or ` hook stop --dialect `.
- **JSON key order.** JSON files are rewritten with sorted keys, so the key order of a hand-edited file changes on the first install. The backup keeps the original file.
- **Updating an entry.** An existing `agora` JSON entry is replaced whole on update, so keys added to it by hand are lost (the backup keeps them). A TOML install keeps user-added keys in our table.
- **Trust.** A later install without `--trust-hooks` for another scope never downgrades recorded trust. Re-installing the same scope without `--trust-hooks` records the hooks as untrusted. For example, after the agora binary moved, the old trust hashes no longer match, and Codex skips the hooks until they are re-trusted.

### 2.4 `capabilities.json`

`$AGORA_HOME/capabilities.json` holds one entry per harness, under the keys listed in 1.2. Each entry records the scopes installed, whether MCP is configured, whether the context and Stop hooks are installed, `hooks_trusted` (Codex only), whether the binary supports an owned session, every file this install wrote, and notes. Keys for Claude Code are unchanged. Unknown keys are preserved.

Uninstall removes a scope from the entry, and removes the entry when no scope is left.

### 2.5 Environment overrides

| Variable | Used for | Default when unset or invalid |
|---|---|---|
| `CODEX_HOME` | Codex config, hooks and AGENTS.md | `<home>/.codex` (must be a non-empty absolute path) |
| `COPILOT_HOME` | Copilot CLI config and hooks | `$XDG_CONFIG_HOME/.copilot` if `XDG_CONFIG_HOME` is set, else `<home>/.copilot` |
| `XDG_CONFIG_HOME` | VS Code user dir (non-macOS), the opencode config dir, and the Copilot config dir when `COPILOT_HOME` is unset | `<home>/.config` |
| `AGORA_VSCODE_USER_DIR` | VS Code user dir, overriding the platform default | macOS: `<home>/Library/Application Support/Code/User` |
| `AGORA_CODEX_BIN`, `AGORA_COPILOT_BIN`, `AGORA_CODE_BIN`, `AGORA_AGY_BIN`, `AGORA_OPENCODE_BIN`, `AGORA_CURSOR_BIN` | Binary used by `--probe` | A `PATH` search for `codex`, `copilot`, `code`, `agy`, `opencode`, then `cursor-agent` and `agent` |

`AGORA_CODE_BIN` is the `code` binary used to probe copilot-vscode. These variables only choose which binary a probe runs.

### 2.6 Probes

`--probe` finds the binary, runs `--version` with a 10 s limit, and records the first line of the output (cut at 100 characters). It then checks whether the binary supports an owned session:

- codex: `codex app-server --help` succeeds.
- copilot-cli: `copilot --help` mentions `--acp`.
- opencode: `opencode serve --help` succeeds.
- agy: `agy --help` mentions `--conversation`.
- copilot-vscode and cursor: false.

A probe never starts a session and never touches configuration.

## 3. Per client

### 3.1 Codex

Codex CLI 0.157.1 is the verified version. Its MCP and hook behaviour was captured from the binary (fixtures, VERIFIED-LOCAL). The app-server protocol was checked live.

| Capability | Status | Notes |
|---|---|---|
| MCP config | VERIFIED-LOCAL, 0.157.1 | `[mcp_servers.agora]` in `$CODEX_HOME/config.toml` (user) or `<proj>/.codex/config.toml` (project). The project file is loaded only for trusted projects (DOCS-ONLY). The server's working directory is the session root, so the identity folder is the session folder. |
| Tool load | VERIFIED-LOCAL, 0.157.1 | MCP `clientInfo.name` is `codex-mcp-client`. |
| Hook context injection | VERIFIED-LOCAL, 0.157.1 | `additionalContext` reaches the model on SessionStart, UserPromptSubmit and PostToolUse. Only for trusted hooks (below). |
| Stop-continue | VERIFIED-LOCAL, 0.157.1 | A Stop `{"decision":"block","reason":...}` continues the turn. The next Stop carries `stop_hook_active: true`, and the stop hook is silent then. |
| Idle wake | NOT SUPPORTED | Only the bridge wakes an idle thread. |
| Owned-session bridge | VERIFIED-LOCAL, 0.157.1 | `agora run-agent codex` drives `codex app-server`. The protocol was checked live; the bridge itself was tested against mock servers. |

**Hook trust.** Codex runs a hook only after it is trusted, and skips untrusted hooks without a message, in both `exec` and the TUI. There are two ways to trust them:

- Open `/hooks` in Codex and review the Agora hooks.
- Run `agora install --client codex --trust-hooks`. This writes a `[hooks.state."<key>"] trusted_hash = "sha256:..."` entry for each Agora handler into the **user** `$CODEX_HOME/config.toml`, whatever the scope. The key is the canonical path of `hooks.json`, the event label, and the group and handler indexes. The hash recipe was checked against Codex's own `hooks/list` output for 12 events (VERIFIED-LOCAL).

Without trust, the install reports `hooks_trusted: false` with the note "Codex skips new hooks until trusted". The hook rungs then stay out of the ladder until a heartbeat appears (1.2). The trusted hash covers the command text, so an install from a different binary path changes the hash and needs trust again. Uninstall removes Agora's trust entries before it removes the handlers.

**Approvals.** Under `approval_policy = never` (exec and the app-server bridge), MCP tool calls fail unless the server entry sets `default_tools_approval_mode = "approve"`. The installer does not write that key. The bridge sets it on each thread it starts, and `--no-approve-agora-tools` turns that off.

Other notes:

- Agora does not use Codex's `notify` setting, so it never replaces a user's `notify`.
- Codex combines AGENTS.md files up to 32 KiB (`project_doc_max_bytes`, VERIFIED-LOCAL).
- MCP notifications never reach the model in Codex.

### 3.2 GitHub Copilot CLI

Copilot CLI 0.0.369 was tested against a mock Copilot API. The test account has no Copilot entitlement, so no real model turn ran. Facts for later versions are DOCS-ONLY.

| Capability | Status | Notes |
|---|---|---|
| MCP config | VERIFIED-LOCAL, 0.0.369 (mock API) | `$COPILOT_HOME/mcp-config.json`, key `mcpServers.agora`. The entry needs `"tools":["*"]`. It has no `env`, because the spawned server treats env values as names of parent variables. The server's working directory is the directory Copilot was launched from, so launch it from the project root. Version 0.0.369 does not read project MCP files. Copilot CLI 1.0.61 and later read `.github/mcp.json` (DOCS-ONLY). |
| Tool load | VERIFIED-LOCAL, 0.0.369 (mock API) | `clientInfo.name` is `github-copilot-developer`. Tools appear as `agora-<tool>`, for example `agora-inbox`. The E2E smoke runs `-p` with `--allow-all-tools`. The narrower `--allow-tool` form was not tested. |
| Hook context injection | NOT SUPPORTED on 0.0.369 | Hooks run from `.github/hooks`, but Copilot ignores all of their output, and the payloads carry no session id. The copilot dialect detects that and stays silent. From 1.0.72, `additionalContext` on sessionStart and postToolUse is DOCS-ONLY. |
| Stop-continue | NOT SUPPORTED on 0.0.369 | From 1.0.72, `agentStop` with `{"decision":"block","reason":...}` is DOCS-ONLY. The block count is capped at 8. |
| Idle wake | NOT SUPPORTED | |
| Owned-session bridge | DOCS-ONLY (`copilot --acp --stdio`, 0.0.397 and later; `session/load` 0.0.410 and later) | Bridge tested against a mock ACP server only. Version 0.0.369 has no `--acp`. |

Notes:

- **Hook files.** Version 0.0.369 reads only `<git root>/.github/hooks/**/*.json`. The user-level `~/.copilot/hooks/` directory is read from 0.0.422 (DOCS-ONLY). So on 0.0.369, the copilot-cli user-scope hooks file is not read at all.
- **One file for both Copilot clients.** The installer writes the same bytes to the shared hooks file for copilot-cli and copilot-vscode, so the second install is `unchanged`. The file uses the Copilot CLI format, with camelCase event keys and `bash` commands:

  ```json
  {"version":1,"hooks":{
    "sessionStart":[{"type":"command","bash":"<session-start cmd>","cwd":".","timeoutSec":10}],
    "userPromptSubmitted":[{"type":"command","bash":"<context cmd>","cwd":".","timeoutSec":10}],
    "postToolUse":[{"type":"command","bash":"<context cmd>","cwd":".","timeoutSec":10}],
    "agentStop":[{"type":"command","bash":"<stop cmd>","cwd":".","timeoutSec":10}]}}
  ```

- **Silence on 0.0.369.** A payload without a non-empty `sessionId` is treated as a legacy invocation. Its event is ignored: no hook runs, no heartbeat is written and the cursor does not move.
- **Capability flags.** `context_hook` and `stop_hook` are true when the probed version is 1.0.72 or later. `owned_session` is true when `copilot --help` mentions `--acp` (0.0.397 and later), whatever the version. Without `--probe` they are all false.
- **Delivery on 0.0.369.** MCP tools and the `inbox_wait` floor only. That is the baseline for every client.
- **Config directory.** The installer uses `COPILOT_HOME` if set (non-empty, absolute), else `$XDG_CONFIG_HOME/.copilot` if `XDG_CONFIG_HOME` is set, else `~/.copilot`. Copilot CLI's own lookup was not tested here.
- **Instructions.** Copilot reads `copilot-instructions.md` in the config directory, merged with `.github/copilot-instructions.md`, then `AGENTS.md`.

### 3.3 VS Code Copilot (agent mode)

VS Code 1.141.0 with Copilot Chat 0.69.0. These facts were read from the installed bundle (static). They were not checked in a live chat session.

| Capability | Status | Notes |
|---|---|---|
| MCP config | VERIFIED-LOCAL, 1.141.0 (static, bundle) | Workspace `.vscode/mcp.json`, or the user `mcp.json` (macOS `~/Library/Application Support/Code/User/mcp.json`). The root key is `servers`. The entry sets `env.AGORA_FOLDER` to `${workspaceFolder}`, so the identity is the workspace. Agora ignores a value that still contains `${`. Expansion of that value in `env` was not exercised live. |
| Tool load | VERIFIED-LOCAL, 1.141.0 (static, bundle) | `clientInfo.name` is `Visual Studio Code`. The Insiders build reports `Visual Studio Code - Insiders`. The protocol version is 2025-11-25. |
| Hook context injection | VERIFIED-LOCAL, 1.141.0 (static, bundle) | Hooks are a preview behind the `chat.useHooks` setting (on by default; policy minimum 1.109). `hookSpecificOutput.additionalContext` is read on SessionStart, UserPromptSubmit, PostToolUse and SubagentStart. |
| Stop-continue | VERIFIED-LOCAL, 1.141.0 (static, bundle) | Only `{"hookSpecificOutput":{"hookEventName":"Stop","decision":"block","reason":...}}` continues. The code ignores a top-level `decision`, so the renderer writes both shapes. |
| Idle wake | NOT SUPPORTED | `code chat -m agent "<prompt>"` opens a window and is not used. |
| Owned-session bridge | NOT SUPPORTED | |

Notes:

- **Hook files read.** VS Code reads every `*.json` in `.github/hooks/` (project) and in `~/.copilot/hooks/` (user). The user path is always the literal home directory, never `COPILOT_HOME`. The installer writes the shared file to both places. Copilot CLI's config directory is `COPILOT_HOME`, else `$XDG_CONFIG_HOME/.copilot`, else `~/.copilot`. The two clients share one user hooks file only when neither `COPILOT_HOME` nor `XDG_CONFIG_HOME` is set. Otherwise they end up with different user hook files.
- **Input format.** VS Code sends its own "Local" JSON schema on stdin whatever the file format: snake_case fields, PascalCase event names and `session_id`. The dialect parses that schema.
- **Working directory of hooks.** The `cwd` of a hook entry resolves against the workspace root. Its default is the home directory. The shared file sets `"cwd": "."` for that reason.
- **Instructions.** Project: `.github/copilot-instructions.md` (block). User: `~/.copilot/instructions/agora.instructions.md`, an owned file with `applyTo: "**"`.
- **Workspace trust.** This was not tested. It is a first check if hooks are silent in a new workspace (section 7).

### 3.4 Antigravity (`agy`)

DOCS-ONLY throughout. Antigravity is not installed on the development machine. Sources are the Antigravity documentation (`antigravity.google/docs/` mcp, hooks, rules and cli/headless pages) and a third-party issue for the `clientInfo` value (`github.com/openSUSE/mtui/issues/591`).

| Capability | Status | Notes |
|---|---|---|
| MCP config | DOCS-ONLY | User `~/.gemini/config/mcp_config.json`, project `<proj>/.agents/mcp_config.json`, root `mcpServers`. The client sends `server/discover` first and falls back to `initialize` on error -32601. The MCP server must not exit on methods it does not know before `initialize`. `clientInfo.name` is `antigravity-client` (third-party report). |
| Tool load | DOCS-ONLY | Not run. |
| Hook context injection | DOCS-ONLY | `PreInvocation` and `PostInvocation` return `{"injectSteps":[{"ephemeralMessage":...}]}`. The dialect maps both to the post-tool-batch context path. |
| Stop-continue | DOCS-ONLY | `Stop` returns `{"decision":"continue","reason":...}`, which re-enters the loop. |
| Idle wake | NOT SUPPORTED | |
| Owned-session bridge | DOCS-ONLY | `agy -p <text> --conversation <id> --output-format json`. `run-agent agy` sends one delivery at a time, with a 10-minute default timeout. Bridge tests use a fake `agy` script, not the real binary. |

Notes:

- Hooks go in `~/.gemini/config/hooks.json` (or `<proj>/.agents/hooks.json`) under the key `agora`. Other named entries are not touched.
- The event name is not reliably in the payload, so the installer passes `--event` on the command line.
- The probe checks for `--conversation` in `agy --help`.

### 3.5 opencode

DOCS-ONLY throughout. opencode is not installed on the development machine. Sources are the opencode documentation (config, mcp-servers, server and rules pages) and the source file `packages/opencode/src/mcp/index.ts` in `anomalyco/opencode`.

| Capability | Status | Notes |
|---|---|---|
| MCP config | DOCS-ONLY | `mcp.agora` in `opencode.json`. User: `$XDG_CONFIG_HOME/opencode/opencode.json`, or `~/.config/opencode/` if unset. Project: `<proj>/opencode.json`. The `command` value is an array. `clientInfo.name` is `opencode`. The installer edits `.json` only. If only `opencode.jsonc` exists, it refuses and prints the entry to add by hand. |
| Tool load | DOCS-ONLY | Not run. |
| Hook context injection | NOT SUPPORTED | opencode has no command hooks. Its plugins are TypeScript, and M6 does not ship one. |
| Stop-continue | NOT SUPPORTED | Same reason. |
| Idle wake | NOT SUPPORTED | Only through the bridge. |
| Owned-session bridge | DOCS-ONLY | `opencode serve` on loopback (default `http://127.0.0.1:4096`). The bridge creates a session, reads the `GET /event` stream, waits for the session to go idle, then posts `prompt_async`. It accepts Basic auth from an environment variable. A non-loopback URL is refused. |

Instructions: `AGENTS.md` in the opencode config directory (user) and `<proj>/AGENTS.md` (project).

### 3.6 Cursor

DOCS-ONLY throughout, with secondary sources. Sources are the Cursor documentation (hooks, context/mcp, context/rules and cli pages), and two third-party pages for the `clientInfo` value (`forum.cursor.com/t/77248`, `canimcp.dev`). Cursor is not installed on the development machine.

| Capability | Status | Notes |
|---|---|---|
| MCP config | DOCS-ONLY | `~/.cursor/mcp.json` or `<proj>/.cursor/mcp.json`, key `mcpServers.agora`, with `env.AGORA_FOLDER` set to `${workspaceFolder}`. `clientInfo.name` is `cursor-vscode` (third-party report). |
| Tool load | DOCS-ONLY | Not run. |
| Hook context injection | DOCS-ONLY | sessionStart and postToolUse return `{"additional_context":...}`. `beforeSubmitPrompt` cannot add context, so Cursor has no prompt-time rung. |
| Stop-continue | DOCS-ONLY | `stop` returns `{"followup_message":...}`, which is submitted automatically. Cursor's `loop_limit` (installed as 5) caps the loop. |
| Idle wake | NOT SUPPORTED | |
| Owned-session bridge | NOT SUPPORTED | `cursor-agent -p --resume` is not bridged in M6. |

Notes:

- `afterMCPExecution` is used only to absorb inbox reads: the dialect maps an inbox tool call to a post-tool event that prints nothing. Other MCP calls map to `cursor:afterMCPExecution` and are ignored.
- The loop counter is per conversation, so the dialect always sends `stop_hook_active` as false. Loops are limited by the Agora block counter and Cursor's `loop_limit`.
- User rules live in Cursor's settings UI, and the installer writes none. The project rule `<proj>/.cursor/rules/agora.mdc` has `alwaysApply: true`.
- Stdin may start with a UTF-8 byte-order mark on Windows. The dialect strips it.

### 3.7 Known limits across harnesses

These limits come from the decisions recorded for M6 (items RB-4 and RB-5) and from the sections above.

1. **One duplicate after an inbox read.** The absorb step moves the cursor to the highest integer `seq` in an inbox tool's result. It trusts only agora's own inbox tools: `mcp__agora__inbox` and `mcp__agora__inbox_wait`, `mcp__plugin_agora_agora__…` (the plugin's tools), Copilot's `agora-inbox`, VS Code's `mcp_agora_inbox`, and the `agora` server in Cursor. Copilot, VS Code, Cursor and Codex return MCP results as text, and the step finds no integer `seq` there. The cursor does not move, so the next hook may show the same messages once. A duplicate is accepted in preference to a loss.
2. **Block counter reset.** For Cursor, agy and Copilot CLI, the Stop block counter resets only on a Stop with an empty inbox or on SessionStart. These dialects map no prompt event that could reset it. Elsewhere, UserPromptSubmit also resets the counter.
3. **No notification to the model.** In none of these harnesses does an MCP notification reach the model. Channel push is a Claude Code feature.
4. **Idle wake is the bridge only.** No other path here wakes an idle session.
5. **Bridge and hook can deliver twice in one turn.** A `run-agent` bridge and a hook can both deliver the same batch within one turn. For agy with `PreInvocation` hooks installed this always happens. For Codex and ACP it is a race. A duplicate is accepted in preference to a loss.
6. **Copilot ACP late error.** If the ACP prompt fails after the 5 s accept window, the bridge logs a warning with the stop reason. The messages stay unacked, so a later `inbox_wait` (or a hook, where one runs) delivers them.
7. **agy refusals.** The agy bridge counts a delivery as accepted only when the result status is `success`. Any other status is a refusal, and refused batches are offered again with a backoff. The backoff starts at 5 s, doubles to 300 s, and resets on acceptance.
8. **agy process exposure.** `agy -p` receives the message text in its argument list, so local users can read it with `ps`. Background processes that agy leaves after a clean exit are not killed by the bridge.
9. **Copilot permissions.** Copilot ACP rejects every permission request unless `--allow-tools` is given. Without it, the agent cannot reply through Agora tools that need permission.

## 4. `agora run-agent`

`agora run-agent` starts an owned-session bridge next to an agent it launches. The bridge long-polls the inbox for the agent's identity and turns each batch of messages into a prompt in that agent. It is opt-in. The command itself is the consent.

```
agora run-agent codex    [--cwd DIR] [--thread ID] [--prompt TEXT] [--model M] [--sandbox read-only|workspace-write|danger-full-access] [--approval never|on-request|untrusted] [--no-approve-agora-tools] [--bin PATH]
agora run-agent copilot  [--cwd DIR] [--session ID] [--prompt TEXT] [--allow-tools] [--bin PATH]
agora run-agent opencode [--url URL] [--session ID] [--cwd DIR] [--password-env VAR] [--prompt TEXT]
agora run-agent agy      --conversation ID [--cwd DIR] [--bin PATH]
```

Behaviour common to all four:

- `--cwd` defaults to the current directory, canonicalized. It is also the identity folder.
- Once the session id is known, the command prints one JSON line: `{"harness":...,"session":...,"folder":...}`. Logs go to stderr only.
- The bridge runs until the agent exits or Ctrl-C (SIGINT or SIGTERM). It exits 0 on a clean end.
- It beats `owned_session_api` every 30 s while it runs. The beat is what puts the rung in the agent's ladder (1.2). Nothing is written to `capabilities.json`.
- It needs an existing identity for the folder. Run `agora whoami` in that folder, or start the agent once with Agora installed. The bridge never creates the identity.
- It never acks. After the agent accepts a prompt, the bridge advances the session cursor (1.3).
- Approval requests are declined. Codex approval requests get `decline`, and Codex elicitation requests get `decline`. Copilot ACP permission requests are rejected unless `--allow-tools` is given.

Per harness:

- **codex** starts or resumes a thread on `codex app-server`. A delivery during an active turn is sent as `turn/steer`. Otherwise it starts a new turn. `--sandbox` defaults to `workspace-write` and `--approval` to `never`. On thread start and resume it sends the untrusted-inbound rule as `developerInstructions`.
- **copilot** (DOCS-ONLY) runs `copilot --acp --stdio`. ACP has no steer, so a delivery that arrives while a prompt is running waits. Waiting deliveries are merged into the next prompt. Without `--allow-tools`, ACP rejects every permission request, so the agent cannot reply through Agora tools that need permission (section 3.7, item 9).
- **opencode** (DOCS-ONLY) works only with a loopback server. A non-loopback `--url` is refused. `--password-env` names the variable that holds the Basic auth password. The password is never logged.
- **agy** (DOCS-ONLY) needs `--conversation`. It runs one `agy -p` call per delivery, with the arguments passed directly and no shell.

## 5. Verified locally vs docs-only

| Client | Evidence | VERIFIED-LOCAL | DOCS-ONLY | NOT SUPPORTED or not tested |
|---|---|---|---|---|
| Claude Code | [CLAUDE-CODE.md section 9](CLAUDE-CODE.md#9-verified-locally-vs-docs-only) | As listed there (2.1.293) | Channels, mods, `asyncRewake` behaviour | Channel push and wake in the interactive TUI, not yet verified |
| Codex | Captured fixtures (0.157.1), protocol checked live, bridge with mocks | MCP config, tool load, context and Stop hooks (when trusted), trust recipe, app-server protocol | Project config trust, `/hooks` review behaviour | Idle wake |
| Copilot CLI | Mock Copilot API (0.0.369) | MCP config, tool load, silence on 0.0.369 | Context and Stop (1.0.72 and later), ACP bridge (0.0.397 and later), user hooks dir (0.0.422 and later) | Hook output on 0.0.369; idle wake |
| VS Code Copilot | Installed bundle (1.141.0), static | MCP config, `servers` key, client name, hook file formats, context, Stop shape | Runtime behaviour (not run) | Idle wake; bridge; workspace trust |
| Antigravity (`agy`) | Documentation and a third-party issue | Nothing | MCP, hooks, bridge | Idle wake |
| opencode | Documentation and source file | Nothing | MCP, bridge | Hooks; idle wake |
| Cursor | Documentation and third-party pages | Nothing | MCP, hooks | Bridge; idle wake |

## 6. Running the real end-to-end smoke

The smoke test is ignored by default, so a normal `cargo test` skips it. Run it with:

```
cargo test -p agora --test e2e_harnesses -- --ignored --nocapture
```

The test file is `crates/agora-cli/tests/e2e_harnesses.rs` (two `#[ignore]`d tests).

Inputs:

- **Codex.** Set `AGORA_E2E_CODEX_AUTH=/path/to/auth.json`. The test copies that file into a temporary `CODEX_HOME`. It never uses your real `~/.codex`.
- **Copilot.** Needs an account with a Copilot entitlement. Authentication comes from the keychain or `GH_TOKEN`. The test copies no auth file. If `copilot -p` fails on authentication, the Copilot half is skipped with a message.
- If a binary is missing, its half of the test is skipped with a message.

What the test does:

1. Installs into a temporary `HOME`, `CODEX_HOME` and `COPILOT_HOME`.
2. Runs `codex exec --skip-git-repo-check -C <proj> "Call the agora whoami tool and print the pseudonym"`. It also runs `copilot -p ... --allow-all-tools`.
3. Asserts that each output mentions the pseudonym. That shows the Agora tools loaded and answered.
4. Asserts that the hook heartbeat files exist.

## 7. Troubleshooting

### Hooks are silent

Hooks fail open, so an error looks like silence. Run one by hand, with the dialect for your harness. This example uses Codex's payload shape:

```
echo '{"session_id":"test","cwd":"/path/to/project","hook_event_name":"UserPromptSubmit","prompt":"x"}' \
  | AGORA_FOLDER=/path/to/project agora hook context --dialect codex
```

Unacked messages produce one JSON line of hook output. Silence has these causes:

- The daemon is not running. Hooks never start it. The MCP server does, so start it by running any `agora` command that uses the daemon.
- No machine seed exists. Hooks never create it. Run any `agora` command once.
- The identity folder differs from the one the MCP server used (see "Identity folder" below).
- The cursor already passed the message, because it was shown in this session. Run `/clear` in Claude Code, or delete `<agora home>/hooks/sessions/<key>.json`.
- The dialect ignored the event. Unknown events, subagent events and Copilot 0.0.369 payloads are all ignored by design.
- The stdin is over 256 KiB, or is not a JSON object.

### Codex hooks do not run

- Run `/hooks` in Codex and check that the Agora hooks are listed and reviewed. An untrusted hook is skipped without any message.
- Or re-run `agora install --client codex --trust-hooks`.
- Check `hooks_trusted` for `codex` in `capabilities.json`. `false` means the hooks are not trusted yet.
- If the binary moved, the trust hash no longer matches. Re-run the install with `--trust-hooks` from the new path.
- Once the hooks have run, a heartbeat file (`hook_inject` or `stop_continue`) shows that they work, even when `hooks_trusted` is `false`.

### Copilot hooks do nothing on a new version

- Check `copilot --version`. On 0.0.369 hook output is ignored by design, so only MCP tools and `inbox_wait` deliver messages.
- Context and Stop output need 1.0.72 or later (DOCS-ONLY). Run `agora install --client copilot-cli --probe` to record the version.
- The user-level hooks directory is read from 0.0.422 (DOCS-ONLY). Use the project scope on older versions.
- Launch Copilot from the project root. Its MCP server and its identity come from the launch directory.

### VS Code hooks do nothing

- Check the `chat.useHooks` setting. It is a preview setting and is on by default from policy minimum 1.109.
- Check that the file is in `.github/hooks/agora.json` (project) or `~/.copilot/hooks/agora.json` (user).
- Check that the workspace is trusted. This was not tested (section 3.3).
- Check the MCP entry in `.vscode/mcp.json` or the user `mcp.json`. The key must be `servers`, not `mcpServers`.

### Identity folder mismatch

The MCP server and the hooks must resolve the same folder to act as the same identity. Compare the key prefix from `whoami` in the session with the identity line that session-start shows. If they differ:

- Codex and Copilot CLI derive the folder from the launch directory and their MCP entries carry no `env`. Start both from the project root.
- VS Code and Cursor set `AGORA_FOLDER` to `${workspaceFolder}`. If the value is not expanded, Agora ignores it and falls back to the process working directory. Check the MCP entry and the directory the client launches the server from.

### Duplicate messages

A duplicate in these harnesses has a known cause, listed in 3.7 item 1: it follows an inbox read whose result has no integer `seq`. Acking each handled message stops repeats.

### Reading `capabilities.json` and delivery status

`$AGORA_HOME/capabilities.json` has one entry per harness, under the keys in 1.2. The fields that decide the ladder are `hooks_trusted`, `context_hook`, `stop_hook` and `owned_session`. `files` lists the paths each install wrote, and `probed_at_ms` is 0 when no probe ran.

`agora whoami` and `delivery_status` show what the server uses:

- `ladder` is the rung list for this session.
- `armed` lists the rungs whose heartbeat is inside its window now.
- `heartbeats` maps each mechanism name (for example `hook_inject`, `stop_continue`, `owned_session_api`) to its last beat in milliseconds.
- `pending` shows each unacked message and how many rungs it has tried.

A rung missing from `ladder` while its hook is installed usually means the Codex trust case or a Copilot version below its flags. Check the entry in `capabilities.json` first.

### Uninstall and shared files

```
agora uninstall --client <list> [--scope user|project] [--dry-run]
```

Uninstall removes only what the install added. A shared file or block is kept while another installed client still lists it, and that action is reported as `kept` with the other client named in the detail. Backups (`.agora-bak-<ms>`) remain next to each changed file. Delete them by hand if you do not need them. A second uninstall reports `unchanged` or `absent`.
