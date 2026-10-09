# Benchmarks: Agora and the legacy Bun server

This document compares Agora (the Rust rewrite) with the legacy Bun/TypeScript
server (`legacy/src/server.ts`) on one machine. Every number below comes from
the runs listed in "Raw results". The harness is `benches/mcp_bench.py`, run by
`benches/run.sh`; see `benches/README.md` for its options.

These are single-machine numbers taken on a machine that was not idle. Read the
"Load" section before using any figure.

## Setup

| Item | Value |
|---|---|
| Date | 2026-10-09 |
| Machine | MacBookPro17,1 (recorded by hand), Apple M1, 8 logical cores, 16 GiB RAM |
| OS | macOS 27.2 (Darwin 27.2.0), arm64 |
| rustc | 1.97.1 (8bab26f4f 2026-07-14) |
| Agora | `agora 0.2.0`, `cargo build --release -p agora` (lto=fat, codegen-units=1), build time 5 min 6 s (recorded by hand) |
| Legacy | `bun run legacy/src/server.ts`, bun 1.3.14, dependencies from `bun.lock` (`--frozen-lockfile`) |
| Git revision | `df90e77` plus a dirty working tree (the uncommitted M7 changes) |
| Parameters | 20 sessions, idle window 60 s, 200 latency messages, 4 writers x 250 messages |

## Method

Each scenario runs against a fresh temporary data directory and a fresh
temporary `HOME`, for each backend separately. Each server session gets its own
project folder, so each session has its own identity. The driver speaks MCP over
stdio to the server in both cases. Both backends use the same driver, scenarios
and timing code. For each scenario, Agora runs first, then legacy.

p99 of 200 samples is close to the maximum; the median of four p99 values is a
weak statistic.

- **Startup.** A session is spawned and `initialize` is sent. Two times per
  session: spawn to the `initialize` response, and spawn to ready (`initialize`
  plus a `whoami` call). Sessions start one after another. The first is the cold
  sample (empty data directory, no daemon running; the JSON records
  `daemon_running_at_start: false` in every run). The other 19 are warm.
- **Idle.** 20 sessions are initialized and join one group, then settle for 5 s.
  CPU time is read with `ps` for every server process, and for Agora also for the
  shared daemon, before and after a 60 s window. Reported: CPU as a percentage of
  one core, and RSS. RSS per session is the shim's RSS (Agora) or the server's
  RSS (legacy). The Agora daemon RSS is reported separately and the total is
  divided by 20 ("amortized").
- **Latency.** Two sessions share a group. The sender posts 200 messages one at a
  time, each with a unique marker. Post round trip is the tool-call round trip.
  Receive latency is the time from the start of the post to the moment the
  receiver observes the marker. On legacy the receiver observes the server's
  `notifications/claude/channel` push, which the server sends on a 1 s poll
  (`CHANNEL_POLL_MS = 1000`). On Agora the receiver loops on `inbox_wait` with a
  cursor and acknowledges each batch with `ack` after the arrival time is
  recorded. The gap between posts is 50 ms for Agora and 400 ms for legacy (see
  "Legacy pacing and retries").
- **Throughput.** 4 sessions post 250 messages each into one group, one thread
  per session. Each thread waits for its own reply before posting again. Reported:
  total messages divided by wall time, and the per-call round trip. Legacy
  limits each (session, tool) to 30 calls per 10 s, so this scenario measures
  that limiter on legacy (see below).

## Results

Four independent full runs of the whole suite, in order. R1 to R4 are listed in
"Raw results". Median is the median of the four. All times are milliseconds
unless stated.

Where the runs differ by more than 25% on a metric (most of them do, see "Load"),
the median is the figure to quote, and the spread is the range of the four runs.

### Startup

| Metric | Backend | R1 | R2 | R3 | R4 | Median |
|---|---|---|---|---|---|---|
| Cold ready (initialize + whoami) | Agora | 140.87 | 138.90 | 157.49 | 64.86 | 139.88 |
| | Legacy | 321.52 | 205.21 | 573.57 | 208.26 | 264.89 |
| Warm ready p50 | Agora | 16.69 | 12.99 | 12.21 | 10.88 | 12.60 |
| | Legacy | 141.82 | 162.75 | 305.29 | 125.04 | 152.28 |
| Warm ready p90 | Agora | 29.11 | 15.01 | 13.74 | 12.66 | 14.38 |
| | Legacy | 155.82 | 296.36 | 592.49 | 147.66 | 226.09 |

Agora's cold sample includes starting the daemon; its cold `initialize` alone was
50.59, 42.36, 58.97 and 33.36 ms in R1 to R4.

### Idle (20 sessions, 60 s window)

| Metric | Backend | R1 | R2 | R3 | R4 | Median |
|---|---|---|---|---|---|---|
| CPU, % of one core | Agora (shims + daemon) | 0.00 | 0.02 | 0.02 | 0.00 | ≤ 0.02 |
| | Legacy | 3.83 | 3.48 | 3.52 | 5.25 | 3.67 |
| RSS per session (MiB) | Agora (shim) | 4.94 | 8.75 | 7.85 | 8.42 | 8.14 |
| | Legacy (server) | 54.47 | 24.50 | 37.91 | 45.11 | 41.51 |
| RSS daemon (MiB) | Agora | 6.23 | 12.42 | 11.17 | 11.44 | 11.30 |
| RSS total / 20 (MiB) | Agora | 5.25 | 9.37 | 8.41 | 8.99 | 8.70 |
| | Legacy | 54.47 | 24.50 | 37.91 | 45.11 | 41.51 |

Agora's idle CPU is at or below the `ps` resolution (about 10 ms of CPU per
process): 0.17 ms of CPU per second in R2 and R3, 0 in R1 and R4. With 21
processes the resolution allows at most about 0.35% of one core (21 x 10 ms / 60 s).
Treat it as too small to measure with `ps`, not as a measured value. Legacy used 35 to
52 ms of CPU per second across the 20 processes, most likely its 1 s poll.

RSS varied between runs by more than the workload explains (legacy 24.5 to
54.5 MiB for the same workload). The machine was under memory pressure during
the runs (about 180 MB of free pages in `vm_stat` on a 16 GiB machine), and macOS compresses and pages out
resident memory, so RSS readings of idle processes move with it. Use RSS as an
order of magnitude: Agora's total per session is several times smaller than a
legacy process.

### Latency (200 messages, one sender, one receiver)

| Metric | Backend | R1 | R2 | R3 | R4 | Median |
|---|---|---|---|---|---|---|
| Post round trip p50 | Agora | 1.07 | 5.57 | 10.34 | 0.97 | 3.32 |
| | Legacy | 3.16 | 4.72 | 1.70 | 1.84 | 2.50 |
| Post round trip p99 | Agora | 5.59 | 172.09 | 429.42 | 16.36 | 94.22 |
| | Legacy | 301.28 | 294.51 | 69.05 | 16.65 | 181.78 |
| Receive p50 | Agora | 1.37 | 7.17 | 13.44 | 1.21 | 4.27 |
| | Legacy | 516.28 | 530.90 | 500.13 | 494.73 | 508.21 |
| Receive p90 | Agora | 2.44 | 65.75 | 266.81 | 2.55 | 34.15 |
| | Legacy | 916.89 | 919.99 | 904.78 | 887.31 | 910.83 |
| Receive p99 | Agora | 7.17 | 266.55 | 621.50 | 16.64 | 141.59 |
| | Legacy | 1229.02 | 1084.06 | 1000.16 | 986.29 | 1042.11 |
| Receive max | Agora | 31.31 | 359.80 | 801.43 | 40.64 | 200.22 |
| | Legacy | 1441.71 | 1106.38 | 1016.13 | 999.21 | 1061.26 |
| Lost messages | both | 0 | 0 | 0 | 0 | 0 |

The legacy receive figures are dominated by its 1 s push poll: a message waits
for the next poll tick, so p50 is about 0.5 s and the maximum about 1 s. This is
a design choice of the legacy server, not an implementation cost. Agora receives
through a blocking `inbox_wait`, which returns as soon as the daemon has the
message. The two receive numbers measure different mechanisms, so do not read
the ratio as a speedup of one implementation of the same design.

Post round trip is the comparable latency number, but its ordering is not stable:
Agora was faster in R1 (1.07 against 3.16 ms) and R4 (0.97 against 1.84 ms), and
slower in R2 (5.57 against 4.72 ms) and R3 (10.34 against 1.70 ms). The backends
run one after the other within a run, so each was exposed to different load. The
tails (p99) in R2 and R3 are not meaningful. The median post p50 is 3.32 ms for
Agora and 2.50 ms for legacy; this data does not show either faster.

### Throughput (4 writers x 250 messages)

| Metric | Backend | R1 | R2 | R3 | R4 | Median |
|---|---|---|---|---|---|---|
| Messages per second | Agora | 2690.49 | 264.26 | 127.15 | 4221.68 | 1477.38 |
| | Legacy | 13.54 | 9.15 | 13.24 | 13.55 | 13.39 |
| Call round trip p50 | Agora | 1.19 | 5.12 | 9.62 | 0.83 | 3.15 |
| | Legacy | 4.92 | 163.85 | 67.80 | 5.00 | 36.40 |
| Call round trip p99 | Agora | 5.41 | 168.85 | 454.21 | 2.81 | 87.13 |
| | Legacy | 13.99 | 1659.55 | 1624.46 | 17.01 | 820.74 |
| Legacy rate-limited retries | Legacy | 907 | 206 | 127 | 1141 | n/a |

The legacy server allows 30 calls per 10 s per (session, tool) (`legacy/src/rate-limit.ts`: a token bucket of 30 refilling at 3 per second). Each writer can post its first 30 messages at once and then 3 per second, so 250 messages take at least (250 - 30) / 3 = 73 s, and 4 writers cannot exceed about 13.6 messages per second overall. R1, R3 and R4 (73.9, 75.5 and 73.8 s, 13.2 to 13.6 messages per second) sit at that ceiling. R2 (109 s, 9.2 messages per second) was below it because the machine was loaded: the limiter was not the only bound there. The legacy figures therefore measure the limiter, not the server's capacity, and the Agora and legacy rows are not like-for-like. The driver waits 250 ms and retries after a `rate_limited` reply. The call round trip excludes the wait; the wall time includes it. Agora has no such limiter, so its figure depends on the machine and the load. Agora was slowed by 10 to 33 times in R2 and R3 relative to R1 and R4.

### Daemon-only Rust benches (debug build)

`cargo test -p agora-daemon --test bench -- --ignored --nocapture`, a debug
(unoptimized) build because disk space was too low for a release test build
(14 GiB free). One run, on a machine at load average 5 to 11. UNVERIFIED against a
saved log (numbers copied from the run's console output):

| Bench line | Value |
|---|---|
| idle CPU, 20 idle connections, 3 s window | 3.0 ms (total, about 1 ms per second) |
| 500 messages posted | 500 |
| post round trip p50 / p99 | 0.716 ms / 3.411 ms |
| wake (post to receiver woken) p50 / p99 | 0.989 ms / 3.798 ms |

These are debug-build numbers and are not comparable with the release-build MCP
results above. They cover the daemon alone, without the MCP shim.

## Load

The brief for this run said the machine was otherwise quiet. It was not. Load figures were recorded by hand and are not in the results files. Other
sessions on the same Mac were running Xcode and Swift builds and tests. Load
averages (1 minute, from `uptime`) at the start and end of each run:

| Run | Start | End | Note |
|---|---|---|---|
| R1 | 16.5 | 8.6 | started through `work run --weight 2` |
| R2 | 8.6 | 46.4 | load rose to above 40 during the run |
| R3 | 25.3 | 32.8 | `work` waited 57 s for capacity first |
| R4 | 28.5 | 6.1 | `work run --weight 8` (all 8 cores); `work` reported 37% CPU idle over the run, so other processes were still running |

The Mac has 8 logical cores, so a load of 16 to 46 means the benchmark processes
competed for CPU. R1 and R4 had the least contention: Agora's measured throughput
was 2690 and 4222 msg/s in them, against 264 and 127 in R2 and R3. They are the
best estimate of undisturbed behaviour; R2 and R3 show how the numbers degrade
under contention. No run was repeated to get a better number, and no run
was dropped. The spread between runs is the finding: the Agora-versus-legacy
ordering held in every run on idle CPU, RSS, startup and receive latency. It held
on throughput in every run too, with the caveat above about the legacy limiter.
Post round trip p50 ordering changed between runs.

## Fairness notes

- **SQLite `synchronous`.** Both servers set `PRAGMA synchronous = NORMAL` in WAL
  mode. Legacy: `legacy/src/db.ts` lines 94 and 95 (`journal_mode = WAL`, then
  `synchronous = NORMAL`). Agora: `crates/agora-store/src/lib.rs`
  (`PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;`). Other settings differ: legacy uses `busy_timeout = 500` and
  `wal_autocheckpoint = 256`; Agora uses `busy_timeout = 5000` and SQLite's
  default autocheckpoint.
- **Process model.** Legacy runs one server process per session, each opening the
  database. Agora runs one small shim per session plus one shared daemon that
  owns the database. Per-session RSS therefore needs the daemon added; the
  "RSS total / 20" row does that.
- **Receive path.** Legacy receive is a 1 s poll by design; Agora's is a blocking
  wait. See Latency.
- **Rate limit.** Legacy throttles repeated calls; Agora does not. See Throughput.

## Legacy pacing and retries

The harness (`benches/mcp_bench.py`) does the following for legacy:

- Legacy posts are paced 0.4 s apart in the latency scenario (2.5 posts per second
  per session, under the legacy limit of 30 calls per 10 s). Agora keeps a 50 ms
  gap. The gap is recorded as `post_gap_s` in the results.
- A legacy `post` that gets a `rate_limited` reply is retried after a 250 ms sleep,
  with a 120 s deadline. Each retry is counted as `rate_limited_retries` in the
  throughput result. The returned timing is the attempt that succeeded.

## Caveats

- One machine, macOS arm64 (Apple M1) only. Linux was not benchmarked.
- The machine was shared and loaded. See Load. The four runs are a sample of that
  load, not a controlled experiment.
- The `ps` CPU time resolution is about 10 ms per process; a quiet process reads
  as zero over a short window.
- Only the spawned server process and, for Agora, the daemon are counted. Child
  processes a server starts are not.
- The Agora cold sample includes daemon start. The shim starts its daemon link in
  a background task after `initialize`; the cold ready time is the end-to-end
  figure.
- RSS is sensitive to memory pressure (see Idle).
- Legacy startup includes the Bun runtime and a TypeScript load of the server;
  it is not a measure of the server logic alone.
- Results files are in `benches/results/`, which git ignores, so they are not in
  the repository.

## Raw results

`benches/results/` is gitignored. The files below were produced on the machine
that ran these benchmarks; they hold every sample (`samples` keys) as well as the
summaries, and the tables above are copied from them.

| Run | File | Started (UTC) |
|---|---|---|
| R1 | `benches/results/20261009T193557Z-MBP20.json` | 2026-10-09 19:35 |
| R2 | `benches/results/20261009T194146Z-MBP20.json` | 2026-10-09 19:41 |
| R3 | `benches/results/20261009T195217Z-MBP20.json` | 2026-10-09 19:52 |
| R4 | `benches/results/20261009T195916Z-MBP20.json` | 2026-10-09 19:59 |

## How to reproduce

Build Agora in release mode, install the legacy dependencies without touching
your own home directory, then run the suite:

```sh
cargo build --release -p agora
cd legacy && HOME=$(mktemp -d) BUN_INSTALL_CACHE_DIR=/tmp/agora-bench-bun-cache bun install --frozen-lockfile
cd ..
bash benches/run.sh --agora-bin target/release/agora
```

`run.sh` uses temporary data directories for both backends, stops every server
and daemon it starts, and writes `benches/results/<UTC timestamp>-<hostname>.json`
and a Markdown summary. `--quick` runs a smoke version (4 sessions, 20 messages,
20 per writer, 5 s idle window). Run it on an idle machine and run it more than once; compare
runs made under similar load only.

The daemon-only benches:

```sh
cargo test --release -p agora-daemon --test bench -- --ignored --nocapture
```

The numbers above for these were taken with a debug build (without `--release`).
