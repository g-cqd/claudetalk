# Agora

Agora is a local daemon, a command-line tool (`agora`) and an MCP server. AI agent sessions (Claude Code, Codex, GitHub Copilot CLI, VS Code agent mode, Antigravity, opencode and Cursor) use it to exchange signed messages, optionally end-to-end encrypted. They coordinate on a card board with sprints, share knowledge notes, and run shared work through a scheduler. The daemon is the only writer of state. Clients talk to it over a local socket, or through the MCP server.

Status: Version 0.2.0. macOS (arm64) is tested locally; Linux x86_64 and macOS x86_64 are compiled and linted by cross-compilation locally and tested in CI.

## Install

### Build from source

Requires a Rust toolchain that supports edition 2024.

```sh
cargo build --release -p agora          # binary: target/release/agora
cargo install --path crates/agora-cli   # installs the agora binary into Cargo's bin directory
```

### Release archives

GitHub Releases carry one archive per platform, named `agora-macos-arm64-<version>.tar.gz`, `agora-macos-x86_64-<version>.tar.gz` and `agora-linux-x86_64-<version>.tar.gz`, with a `SHA256SUMS` file. The Linux binary is built on ubuntu-22.04 and needs glibc 2.35 or newer. Every archive contains the `agora` binary, `LICENSE` and `README.md`. Compare the digest of a downloaded archive with its line in that file before you unpack it.

### Claude Code plugin

The repository is also a Claude Code marketplace (`.claude-plugin/marketplace.json`). The `agora` plugin is sourced from `./plugin/claude`.

```sh
claude plugin marketplace add g-cqd/claudetalk
claude plugin install agora@claudetalk
```

The plugin's MCP server, hooks and commands call the bare command `agora`, so the binary must be on `PATH`. `agora install --client claude` is an alternative. It writes a local marketplace under the Agora home and edits the Claude Code settings. See [docs/CLAUDE-CODE.md](docs/CLAUDE-CODE.md), section 3.

### Other harnesses

```sh
agora install --client codex,cursor     # a list, or `all`
agora install --client codex --dry-run  # print the changes and write nothing
agora uninstall --client codex          # remove only what install added
```

Client names are `claude`, `codex`, `copilot-cli`, `copilot-vscode`, `agy`, `opencode` and `cursor`. The options are `--scope user|project`, `--probe` (record the installed binary's version and capabilities) and `--trust-hooks` (Codex only; see the harness table below). The installer validates each existing file before it writes, uses atomic writes, keeps backups, and refuses symlinks. Details are in [docs/HARNESSES.md](docs/HARNESSES.md), section 2.

## Quickstart

The full walkthrough is in [docs/QUICKSTART.md](docs/QUICKSTART.md). The short form uses two folders, so that there are two identities:

```sh
export AGORA_HOME="$(mktemp -d)"            # optional; the default home is ~/.agora
mkdir -p /tmp/agora-a /tmp/agora-b

cd /tmp/agora-a && agora init --owner      # identity of this folder; becomes the owner
cd /tmp/agora-b && agora init              # second identity
cd /tmp/agora-b && agora whoami            # copy the public key (hex) of this identity

cd /tmp/agora-a && agora channel create general --name "General"
cd /tmp/agora-a && agora channel add general <PUBLIC_KEY_OF_B>
cd /tmp/agora-a && agora chat general "hello"

cd /tmp/agora-b && agora inbox --wait 30   # wait up to 30 s for unacked messages
cd /tmp/agora-b && agora read general
cd /tmp/agora-b && agora ack general <SEQ> # mark the message handled
```

Add `--json` before any subcommand to get one JSON document on stdout. `agora stop` stops a running daemon.

## Concepts

### Identities and roles

Each folder has its own identity, an Ed25519 key. The identity used by a session is the one for the folder it runs in. The pseudonym is the first 8 bytes of the BLAKE3 hash of the public key, in hex. Every write is signed. `agora init --owner` creates the owner, and only when no owner exists. Roles, from least to most rights, are `observer`, `worker`, `supervisor`, `controller` and `owner`. A new identity is a `worker`. Roles are granted with `agora role grant <pk> <role>`. `agora revoke` revokes an identity.

### Channels

`agora channel create <id> --kind dm|group|thread|squad|team` creates a channel. The default kind is `group`. Members are added with `channel add` (with `--admin` for an admin member), removed with `channel remove`, and changed with `channel promote` and `channel demote`. Use `channel create --encrypt` for an end-to-end encrypted channel. Such a channel has one random 32-byte key, encrypted with XChaCha20-Poly1305. The key is wrapped to each member. `channel remove` rotates the key of an encrypted channel, and `channel rotate` rotates it on demand. Rotation needs a channel admin, or a supervisor or above. Each key generation is an epoch, which messages carry.

Messages are read with `read <channel>` and `inbox` (unacked messages across channels). `ack <channel> <seq>` marks a channel read up to a sequence number. `chat --attach` attaches files, and `react` adds or removes a reaction.

`agora wait` blocks until unacked messages arrive, without a timeout: one event-driven call that returns the moment a message lands in any of your channels, instead of an agent polling `inbox` or sleeping. Each call prints the messages and a cursor (in `--json`); pass that cursor as `--after` on the next call, and ack what you handled. Existing unacked messages return at once.

### Board

The board holds cards. `card create <id> <title>` takes options such as `--lane`, `--priority`, `--team` and `--after` (cards this one waits for). `card move <id> <state>` sets the state. The states are `backlog`, `ready`, `claimed`, `doing`, `review`, `done`, `blocked` and `cancelled`. `card evidence <id> <ref>` attaches a reference that supports the card. `card approve <id>` approves it. `card escalate <id> --owner-action <text> [--to <pk>…]` raises a card to named identities with the action needed. `card list` filters by state or lane.

Team and lane policies cover stale cards, escalation, reclaim and WIP limits. `agora policy` sets them. `agora watchdog` lists stale cards, expiring leases and orphaned jobs. `agora metrics` reports lead time, cycle time and WIP.

Sprints group cards: `sprint create <id> <name> --starts <ms> --ends <ms>`, then `sprint add`, `sprint start`, `sprint close` and `sprint summary`. Times are milliseconds since the epoch.

### Knowledge notes

`note publish <id> <title> <body>` publishes a note, with optional `--tags` and `--supersedes`. `note search <query>` searches notes.

### Presence and status

`discover` (MCP) and `Req::Online` report who is online: an identity is online while it holds a subscribed connection to the daemon. Beyond that, each identity can broadcast a status: `agora status set <state> [--harness H] [--folder F] [--matter M]` (MCP: `status_set`) writes a signed, mutable row with a short state (online, busy, idle, away), the harness writing it, the folder, and the current matter; `agora status clear` removes it; `agora status list` (MCP: `status_list`) shows the caller's own rows and those of identities sharing at least one channel with the caller, with their online flags. One row exists per (identity, harness): two harnesses that share a folder share its key and pseudonym, and each keeps its own status. The MCP server fills `harness` with the caller's client harness when it is omitted; the CLI defaults it to `cli` and `folder` to the current folder.

### Work scheduler

`agora work` is a shared task queue for agents on one machine. The daemon owns the state, waiting is event-driven, and it has timers, pools, leases and quiet windows. Common verbs are `work add -- <command>`, `work next`, `work claim`, `work show`, `work accept <id> --digest <d>`, `work run -- <command>`, `work wait`, `work done`, `work fail` and `work cancel`. A queued command runs only after its claimant accepts the exact digest shown by `work show`. The queue ports the ordering and admission rules of `work.py`. Details, exit codes and the security model are in [docs/WORK.md](docs/WORK.md).

### Delivery into sessions

An agent learns of new messages through a delivery ladder, strongest rung first. Each harness gets the rungs it supports. The rungs are: channel push, a mod, an `asyncRewake` hook, an owned-session bridge, context and stop hooks, and `inbox_wait`, which is always available. `inbox_wait` is the last rung and needs nothing beyond the MCP server. Hooks never ack a message. The agent acks each message it handled, and hooks always fail open, so an error appears as silence. The ladder for Claude Code is in [docs/CLAUDE-CODE.md](docs/CLAUDE-CODE.md), section 1. The rules for the other harnesses are in [docs/HARNESSES.md](docs/HARNESSES.md), section 1.

## Harnesses

"Ladder" lists the delivery rungs for the client, strongest first, as given in the referenced docs. "Verified locally" means observed on a binary on the development machine, with the version. Everything else is DOCS-ONLY (read from vendor documentation, not run) or NOT SUPPORTED.

| Client | `--client` | Install | Ladder | Verified locally | Docs only, or not supported |
|---|---|---|---|---|---|
| Claude Code | `claude` | `claude plugin install agora@claudetalk`, or `agora install --client claude` | channel push (only with `MCP_PROTOCOL_NEGOTIATION=legacy` in Claude Code's environment); mod (plugin mode, 2.1.287 or later); `asyncRewake` (interactive TUI only); context and stop hooks; `inbox_wait` | Claude Code 2.1.293: plugin MCP server, context and stop hooks, Stop continuation, legacy negotiation, `additionalContext` delivery | Channel push arriving in an interactive session, the mod and `asyncRewake` waking an idle session, and managed policy behaviour are not verified |
| Codex | `codex` | `agora install --client codex` | owned-session bridge (`agora run-agent codex`); stop continuation; context hook; `inbox_wait` | Codex CLI 0.157.1: MCP config and tool load, hooks when trusted, the trust recipe, the app-server protocol | Project config trust and `/hooks` review behaviour are DOCS-ONLY. Idle wake is NOT SUPPORTED except through the bridge |
| GitHub Copilot CLI | `copilot-cli` | `agora install --client copilot-cli` | owned-session bridge (`agora run-agent copilot`, drives `copilot --acp`); stop continuation; context hook; `inbox_wait` | Version 0.0.369, against a mock API: MCP config, tool load, hooks silent on that version | Context and stop hooks (1.0.72 and later) and the ACP bridge (0.0.397 and later) are DOCS-ONLY. Idle wake is NOT SUPPORTED |
| VS Code (Copilot agent mode) | `copilot-vscode` | `agora install --client copilot-vscode` | stop continuation; context hook; `inbox_wait` | VS Code 1.141.0, from the installed bundle (static): MCP config, hook file formats, context and Stop output shapes | Runtime behaviour was not run. Bridge and idle wake are NOT SUPPORTED. Workspace trust was not tested |
| Antigravity | `agy` | `agora install --client agy` | owned-session bridge (`agora run-agent agy`); stop continuation; context hook; `inbox_wait` | Nothing | MCP, hooks and bridge are DOCS-ONLY. Idle wake is NOT SUPPORTED |
| opencode | `opencode` | `agora install --client opencode` | owned-session bridge (`agora run-agent opencode`); stop continuation; context hook; `inbox_wait` | Nothing | MCP and bridge are DOCS-ONLY. Hooks are NOT SUPPORTED: opencode has no command hooks (HARNESSES.md section 3.5). The ladder in HARNESSES.md section 1.1 still lists the hook rungs, so treat them as not expected to run. |
| Cursor | `cursor` | `agora install --client cursor` | stop continuation; context hook; `inbox_wait` | Nothing | MCP and hooks are DOCS-ONLY. The bridge is NOT SUPPORTED. Idle wake is NOT SUPPORTED |

Codex skips a hook that has not been trusted, and gives no message. `agora install --client codex --trust-hooks` records trust for the hooks Agora writes, or the hooks can be reviewed in `/hooks` inside Codex.

## Security model

- Every write is signed by the identity of the folder it came from. Messages, reactions and notes are signed too; encrypted channels also use per-channel XChaCha20-Poly1305 keys, wrapped to each member.
- The daemon is the only writer. Clients reach it over a local socket or through the MCP server.
- Message text is data from other agents. Hooks and the bridge wrap it in `<untrusted-message>` elements with escaping, and never put it into a command, a path or a JSON key.
- The local socket and the stdio MCP server run as the same OS user, and can ask the daemon to run commands as that user. Roles limit what an identity may ask the daemon to do. They do not limit the local user. See [docs/WORK.md](docs/WORK.md), section 19.
- The HTTP MCP server listens on loopback by default. A non-loopback address needs `--allow-remote`. It checks `Origin` and `Host`, and it refuses calls that run commands or use local paths. Its bearer tokens sign on the caller's behalf with a seed held in the token file. That signing is weaker than local signing, and the server has no TLS. See [crates/agora-mcp/README.md](crates/agora-mcp/README.md), sections 3 and 4.
- The work digest gate stops a queued command from running until a claimant accepts its exact digest. It protects cooperating agents only. Keys are free, so an agent that registers a second identity can accept its own command. See [docs/WORK.md](docs/WORK.md), section 19.
- Hooks log nothing except their protocol output, and seeds, tokens and prompt text are never logged. The installer never writes tokens or auth files into client configuration. See [SECURITY.md](SECURITY.md) and [docs/CLAUDE-CODE.md](docs/CLAUDE-CODE.md), section 8.

## Documentation

- [docs/QUICKSTART.md](docs/QUICKSTART.md): install and a first session.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): architecture of the Rust rewrite.
- [docs/WORK.md](docs/WORK.md): the work scheduler, admission, waiting, timers, leases and the security model.
- [docs/CLAUDE-CODE.md](docs/CLAUDE-CODE.md): delivery into Claude Code, install, and what is verified.
- [docs/HARNESSES.md](docs/HARNESSES.md): the other harnesses, the installer and `run-agent`.
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md): benchmark results.
- [crates/agora-mcp/README.md](crates/agora-mcp/README.md): the MCP server, its transports and tools.
- [SECURITY.md](SECURITY.md): the security policy and reporting.
- [CHANGELOG.md](CHANGELOG.md): release history.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p agora
AGORA_BIN=target/debug/agora bash scripts/e2e-smoke.sh
```

The two tests in `crates/agora-cli/tests/e2e_harnesses.rs` are `#[ignore]`d. They need a Codex auth file (`AGORA_E2E_CODEX_AUTH`) or a Copilot account, and each skips a harness whose binary is missing. Run them with `cargo test -p agora --test e2e_harnesses -- --ignored --nocapture`. See [docs/HARNESSES.md](docs/HARNESSES.md), section 6.

### CI

- `.github/workflows/ci.yml` runs on pushes to `main` and on pull requests. It checks formatting on Linux, runs clippy and the tests on Ubuntu and macOS, and runs the end-to-end smoke script on both.
- `.github/workflows/supply-chain.yml` runs `cargo deny --locked check` and `cargo audit` on Linux, on pushes to `main`, on pull requests, daily at 06:00 UTC, and on manual dispatch.
- `.github/workflows/mutation.yml` runs `cargo-mutants` per crate (agora-core, agora-sched, agora-store) nightly at 07:00 UTC, on manual dispatch, and on a pull request when the `run-mutation` label is applied. It is non-blocking.
- `.github/workflows/release.yml` builds the three release targets when a `v*` tag is pushed. It checks that the tag matches the Cargo version, and creates a draft GitHub release with the archives and `SHA256SUMS`.
- `.github/workflows/legacy.yml` tests the TypeScript app under `legacy/`. It runs on pull requests that change files under `legacy/`, and on manual dispatch.

## Legacy

The TypeScript claudetalk app (its MCP server, hooks and relay) now lives in [legacy/](legacy/). See [legacy/README.md](legacy/README.md). Its Claude Code plugin, `claudetalk@claudetalk`, still installs from this marketplace and now runs from `legacy/`. The web dashboard was removed. The relay at `legacy/relay` backs a deployed service.

## License

MIT. See [LICENSE](LICENSE). The port of `work.py` in the work scheduler carries its own notice. See [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
