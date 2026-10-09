# MCP benchmark: legacy Bun server vs Agora

`benches/mcp_bench.py` drives two MCP servers over stdio with the same
driver, the same scenarios and the same timing code:

- the legacy Bun server, `legacy/src/server.ts`, started as `bun run <path>`;
- Agora, started as `agora mcp`.

`benches/run.sh` runs every scenario for both backends, writes one JSON
results file and prints a Markdown summary. The driver is Python 3 standard
library only. Nothing is built by `run.sh`.

## What is measured

Each scenario runs against a fresh temporary data directory and a fresh
temporary `HOME`. Every server session gets its own folder, so each session
has its own identity.

| Scenario | What it measures |
|---|---|
| `startup` | Each session is spawned, then `initialize` is sent. Two numbers per session: the time from spawn to the `initialize` response, and the time to a ready session (`initialize` plus a `whoami` call). The first session is reported as cold. The other sessions are warm, with p50 and p90. Sessions run one after another. |
| `idle` | N sessions (default 20) are initialized, join one group, and settle for 5 s. Then CPU time and RSS are read with `ps` for the server processes and, for Agora, the shared daemon. CPU is read before and after a window of `--idle-seconds`. Reported: CPU as a percentage of one core, CPU milliseconds per second, mean RSS per session, daemon RSS, and total RSS divided by the number of sessions. |
| `latency` | Two sessions share a group. The sender posts `--messages` messages one at a time, each with a unique marker, with a gap between posts of 50 ms for Agora and 400 ms for legacy (to stay under the legacy rate limit). For each message the driver records the post tool-call round trip, and the time from post start to the moment the receiver observes the marker. Reported: p50, p90, p99 and max of both, and the count of lost messages. |
| `throughput` | `--writers` sessions post `--per-writer` messages each into one group, one thread per session. Each thread waits for its own response before the next post. Reported: total messages divided by wall time, and the per-call round-trip p50 and p99. On legacy, a `rate_limited` reply is retried after 250 ms (up to 120 s) and counted in `rate_limited_retries`. |

### How receipt is detected

- **Legacy.** The server pushes `notifications/claude/channel` to every session
  on a 1 s poll (`CHANNEL_POLL_MS = 1000`). The driver waits for a notification
  whose `meta.chat_id` is the group and whose `content` contains the marker. A
  receiver must observe the chat before the first post. The cursor starts at
  the chat's max sequence at first observation, so the driver waits 2.5 s after
  the group is created.
- **Agora.** The receiver loops on `inbox_wait` with a cursor. Each batch is
  acknowledged with `ack` after the arrival time is recorded. The driver does
  not use channel push for Agora.

### Machine and tool information

Each results file records the platform, CPU model, logical core count, RAM,
hostname, `bun --version`, `agora --version`, the repository commit, and whether
the working tree was dirty. It also records the parameters and the start and
finish times. Raw samples are included next to the summaries.

## How to run

Full run. Build the release binary first:

```sh
cargo build --release -p agora
bash benches/run.sh
```

Smoke run with tiny parameters (4 sessions, 20 messages, 20 per writer, 5 s
idle window). It takes well under a minute:

```sh
bash benches/run.sh --quick --agora-bin target/debug/agora
```

Options:

- `--agora-bin PATH`: the agora binary. Default `target/release/agora`.
- `--legacy-dir DIR`: the legacy tree. Default `legacy`.
- `--quick`: smoke-run parameters.

### The legacy backend needs installed dependencies

`run.sh` runs the legacy backend only when `bun` is on `PATH` and
`legacy/node_modules` exists. Otherwise it prints a skip message and runs
Agora alone. To install the dependencies hermetically:

```sh
cd legacy && HOME=$(mktemp -d) BUN_INSTALL_CACHE_DIR=/tmp/agora-bench-bun-cache bun install --frozen-lockfile
```

### Running the driver directly

`run.sh` sets the data-directory variables for you. The driver does not run
without them:

- `AGORA_HOME` for the agora backend;
- `CLAUDETALK_HOME` for the legacy backend.

Both must be absolute paths to an existing directory. The driver refuses a
value equal to or inside `~/.agora` or `~/.claudetalk`, and it refuses the
home directory itself.

```sh
AGORA_HOME="$(mktemp -d /tmp/agora-bench.XXXX)" python3 -I benches/mcp_bench.py \
  --backend agora --server-cmd "target/release/agora mcp" \
  --scenario all --sessions 20 --messages 200 --writers 4 --per-writer 250 \
  --idle-seconds 60 --out /tmp/agora-bench-agora.json
```

The driver's other modes:

- `--merge FRAGMENT... --out RESULTS.json` combines run fragments;
- `--summarize RESULTS.json` prints the Markdown table.

### Output

`run.sh` writes `benches/results/<UTC timestamp>-<hostname>.json` and prints
the summary. The `benches/results/` directory is not tracked by git. Keep it
out of commits, or add it to `.gitignore` in a separate change.

### Cleanup

Each run stops every server it spawned and the Agora daemon it started. The
stop is matched on the exact `--home` path, so other daemons are not touched.
A `SIGINT` or `SIGTERM` during a run triggers the same cleanup. `run.sh` removes
its temporary directories on exit.

## Caveats

- **Legacy receive latency is a polling interval.** The legacy push runs on a
  1 s poll by design, so receive latency is dominated by that interval. Agora
  receives through a blocking `inbox_wait`, so its receive latency reflects the
  daemon's delivery path. The two receive numbers measure different mechanisms,
  not the same mechanism at different speeds.
- **The Agora daemon is shared.** Several shims talk to one daemon. RSS is
  therefore reported per shim, with the daemon separately, and the total
  divided by the number of sessions. The per-session figure alone understates
  the cost of the daemon.
- **Durability settings.** Both servers use SQLite in WAL mode with
  `PRAGMA synchronous=NORMAL` (`legacy/src/db.ts` lines 94-95;
  `crates/agora-store/src/lib.rs`). They differ in other settings: legacy sets
  `busy_timeout=500` and `wal_autocheckpoint=256`; Agora sets
  `busy_timeout=5000` and keeps SQLite's default autocheckpoint.
- **CPU time resolution is coarse.** `ps` reports CPU time to about 10 ms per
  process. An idle process can read as zero over a short window. Use the
  default 60 s window for CPU numbers.
- **Process tree.** Only the spawned server process and, for Agora, the daemon
  are measured. Child processes that a server starts are not included.
- **Cold start.** The cold sample is valid only when the data directory is
  empty and no daemon is running. `run.sh` gives each run a fresh data
  directory. The output records `daemon_running_at_start` so an invalid cold
  sample is visible. The Agora shim starts its link to the daemon in a
  background task after its first request (`initialize`), so the daemon start
  overlaps the `initialize` response. Whether the first tool call waits for the
  daemon was not checked (UNVERIFIED). The `ready` figure is the end-to-end cold
  number either way.
- **Load.** Results depend on machine load. Run on an idle machine, and do not
  compare runs made under different load.
- **Legacy rate limit.** The legacy server allows 30 calls per 10 s per
  (session, tool). The latency scenario paces legacy posts 0.4 s apart. In the
  throughput scenario the legacy backend retries after `rate_limited`, so the
  legacy throughput figure is the limiter's ceiling (about 12 to 13 messages per
  second for 4 writers). Results from a full run are in `docs/BENCHMARKS.md`.

## Daemon-only Rust benches

The daemon has two ignored benchmarks in `crates/agora-daemon/tests/bench.rs`.
They measure the daemon alone, without the MCP shim:

```sh
cargo test -p agora-daemon --test bench -- --ignored --nocapture
```

- `idle_cpu_with_20_idle_connections`: daemon CPU with 20 idle connections.
- `message_latency_p50_p99`: message latency, p50 and p99, through the daemon.

Use them to separate daemon cost from shim cost when the MCP numbers look
unexpected.
