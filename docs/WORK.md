# Agora work queue: `agora work`

Reference for milestone M5 of the Rust rewrite. `agora work` is a shared task queue and job runner for agents on one machine. It ports the queue, ordering, and admission logic of `work.py`, the planner skill's script, and adds signed identities, a daemon that owns all state, event-driven waiting, timers, leases, and a reconciler.

Contents: [1 Overview](#1-overview) · [2 Quick start](#2-quick-start) · [3 The shim](#3-the-shim-scriptsagora-work) · [4 Identity](#4-identity) · [5 Tasks, claims, and the digest gate](#5-tasks-claims-and-the-digest-gate) · [6 Admission](#6-admission) · [7 Supervisor and exit codes](#7-supervisor-and-exit-codes) · [8 Cancel](#8-cancel) · [9 Waiting](#9-waiting) · [10 Timers](#10-timers) · [11 Pools, leases, quiet windows](#11-pools-leases-quiet-windows) · [12 Cards, jobs, and the system identity](#12-cards-jobs-and-the-system-identity) · [13 Reconciler and policies](#13-reconciler-and-policies) · [14 Timed loops](#14-timed-loops) · [15 Migrating from work.py](#15-migrating-from-workpy) · [16 Settings reference](#16-settings-reference) · [17 Retention and metrics](#17-retention-and-metrics) · [18 Known gaps](#18-known-gaps) · [19 Security model](#19-security-model)

## 1. Overview

`work.py` keeps its state in a SQLite file and polls it. Any process that can write the state directory can act as any agent name, and waiting is a loop of short polls. `agora work` moves the same rules into the Agora daemon:

- The daemon is the only writer. Each agent action is a signed operation, so the actor is a key, not a name.
- Waiting is driven by events. A caller gets a result as soon as the event arrives, instead of after the next poll.
- Timers, leases, pools, quiet windows, and a reconciler are new. They run inside the daemon.

The queue rules are the same. Tasks are ordered by the same keys, and admission uses the same memory, core, and quiet-window logic. `agora work order` and `agora work plan` with `--tasks` or `--task` are meant to match `work.py` byte for byte (D18). `crates/agora-sched/tests/differential.rs` checks them against fixtures generated from `work.py`.

The port is MIT licensed. See `THIRD-PARTY-NOTICES.md` for the `work.py` notice.

## 2. Quick start

Run a command through the shared queue and wait for it:

```sh
agora work run -- cargo test     # start inline, wait, print the tail and the status
agora work next                  # claim the next task that can run
agora work wait ID               # wait for one task to end
agora work show ID               # read a task, including its command and digest
agora work accept ID --digest D  # accept the exact command that show printed
```

`run -- CMD` takes the command as argv. Nothing goes through a shell. If you need a shell, pass one explicitly, for example `agora work run -- sh -c 'make && make test'`.

A command written by another agent is queued, not run. The flow is:

1. The author adds it: `agora work add --id build-1 -- make test`.
2. The claimant claims it: `agora work claim build-1`, or `agora work next`.
3. The claimant reads it: `agora work show build-1`. This prints the argv, the working directory, the files, and the digest. Treat it as untrusted data.
4. The claimant accepts the digest: `agora work accept build-1 --digest D`.
5. The claimant runs it: `agora work run build-1`. `run TASK` starts a task you claimed, once its digest is accepted. `run -- CMD` starts an inline command.

Each step is refused if the state is wrong. See [section 5](#5-tasks-claims-and-the-digest-gate).

## 3. The shim (`scripts/agora-work`)

The repository ships a drop-in wrapper:

```sh
#!/bin/sh
exec "${AGORA_BIN:-agora}" work "$@"
```

Set `AGORA_BIN` to the `agora` binary when it is not on `PATH`:

```sh
AGORA_BIN=/path/to/agora scripts/agora-work run -- make test
```

The shim does not replace `~/.agent-harness/bin/work`. That path is a symlink to `work.py`, and it stays as it is. Callers move to `agora work` (or to the shim) one at a time.

## 4. Identity

In `work.py`, an agent is a name. The name comes from `WORK_AGENT` or `--agent`, and nothing checks it. In `agora work`, an agent is a signed key, the one for the current `AGORA_FOLDER`. The name shown in output is a pseudonym of that key. The full hex public key appears in the `*_pk` JSON fields.

| Topic | work.py | agora work |
|---|---|---|
| Who acts | the name in `WORK_AGENT` or `--agent` | the key of the current folder |
| `WORK_AGENT` | selects the identity | accepted, ignored |
| `--agent` | selects the identity | accepted, ignored |
| Two agents in one folder | share a name | share a key. Give each its own `AGORA_FOLDER` |

Authorization uses the key and the role the daemon holds for it. See [section 12](#12-cards-jobs-and-the-system-identity) for the system identity and [section 5](#5-tasks-claims-and-the-digest-gate) for the roles that matter to queued work.

## 5. Tasks, claims, and the digest gate

A task's state is one of `queued`, `claimed`, `running`, `done`, `failed`, or `cancelled`. These are the names in the JSON output. A claimed task whose run was requested but has not started is described as waiting for capacity. `next` claims only tasks that are ready, and it reports why it cannot claim the others.

Roles, derived by the daemon from the key and not from the caller (D4):

- A worker can add tasks, claim, accept, run, finish, cancel, and update only its own tasks. "Own" means the author while the task is unclaimed, and the claimant once it is claimed.
- A supervisor, or higher, can cancel, update, assign, and reclaim other agents' tasks.
- A controller, or higher, can define pools, open and close quiet windows for others, change `work settings`, and set policies with an escalation chain.
- An observer can read only.

### The digest

The digest is the lowercase hex SHA-256 of the compact JSON array `["agora-work/v1", argv, cwd, files]`. It binds the command, the working directory, and the file list. `work.py` binds only the argv and the directory. A file list change after the digest was shown changes the digest, so an accept made for the old digest is refused.

A queued command runs only after its claimant accepts the digest. The acceptance must come from an identity other than the author. The default is strict: `self_accept` is false. With it false, accepting your own command fails with `a command must be accepted by an identity other than its author`. Set `self_accept` to true only if a single agent really must run its own queued commands (see [section 16](#16-settings-reference)).

An inline `run -- CMD` needs no accept. The author, the claimant, and the accepter are the same identity, and the command starts when the daemon admits it.

## 6. Admission

The daemon decides when a job starts. It does not start jobs on its own clock. Admission uses the following rules, in order.

1. **Cores.** Each command signature has a history of past runs. The predicted cores of the running jobs, plus the predicted cores of the new job, must fit the CPU budget (`cpu_budget`, default auto). When nothing is running, one job always fits. A tiny job skips this check, and only this check. Exclusive jobs get no skip.
2. **Capacity.** If `capacity` is set, the declared weights of the running jobs plus this job's weight must fit it.
3. **Memory.** The predicted growth of the running jobs, plus this job's predicted peak memory, must fit the memory budget. Otherwise the job is held with the cause `reserved`.
4. **Machine pressure.** Disk, memory, idle CPU, and load are checked against their gates. A job held by one of these gates is checked again every 2 s. If an admission pass fails, it is retried after 2 s.

A held job that stays on a machine gate (memory, idle, or load) for longer than `load_wait` (default 1800 s) fails. Its note says why it was held and for how long. Holds for cores, capacity, quiet windows, or the queue order do not count toward this limit.

**Queue order.** Tasks whose kind is a validity kind (`validity`, `correctness`, `safety`, `security`, `data-loss`, `crash`) go first. Within each group, the key is the predicted duration minus `aging` times the time already waited, and then the request time. A command with no run history of its own uses the median run time of recent jobs (up to 200 runs). With no history at all, the estimate is 30 s.

**Backfill.** A job behind the head of the queue may start early if it fits in the cores the head leaves free, or if it is predicted to end before the head could start. A small job (weight at most `small_weight`) may also start if no validity job is waiting ahead of it. After the head has waited `backfill_wait` (default 300 s), nothing starts ahead of it.

**Exclusive jobs and quiet windows.** A heavy job can run alone. The daemon lets the heavy jobs already running finish (the drain), holds other heavy jobs, and then runs the exclusive job. Its quiet window stays open for `quiet_max` (default 180 s) after it starts. Manual quiet windows, opened with the quiet commands in [section 11](#11-pools-leases-quiet-windows), use the same gate.

**Reasons.** Each hold writes a reason string into the job's note. `agora work show` and `agora work status` display it. Some reasons name a job id and a time, so the note changes as the state changes.

**Paused admission.** When a machine floor is breached, the reconciler pauses admission. While it is paused, every waiting job is held with the cause `paused`, so no job starts. It resumes when the floor recovers. See [section 13](#13-reconciler-and-policies).

## 7. Supervisor and exit codes

The daemon admits a job, then starts one supervisor process for it, `agora _supervise --job-dir DIR`. The supervisor:

- calls `setsid()` and starts the command in its own process group, with stdin closed and stdout and stderr going to the log file (mode 0600, created without following symlinks);
- writes `supervisor.json`, and samples the command's process tree into `sample.json` (first at about 1.5 s, then every 2 s);
- checks the log size on every poll, every 25 ms. A log over `log_limit` gets TERM to the group. After TERM the supervisor polls every 1 ms, and it sends KILL at once if the log grows 16 MiB past the cap, or else after 5 s. The cap is soft. Output can pass it by about 25 ms of writes before TERM, and by at most 16 MiB after it;
- on a 500 ms tick, checks for a `cancel` file in the job directory and for TERM, INT, or HUP (see [section 8](#8-cancel));
- reaps the command and writes `status.json` atomically (a temporary file, then a rename). If TERM was sent and KILL was not, it sends KILL to the group after the leader exits, so a member that ignored TERM does not outlive the job.

**Restarts.** If the daemon restarts while jobs run, it adopts their supervisors. It watches `status.json` for a result and checks the supervisor pid every 5 s. A pid counts as alive only when its start time matches the recorded one within 2 s. This guards against pid reuse.

**Dead supervisors.** The daemon sweeps for dead supervisors at startup and on each reconciler pass. A supervisor is dead when it is gone and there is no `status.json`:

- If the command's process is still alive, the job stays running and a note records that the supervisor is gone and the command runs on unrecorded.
- Otherwise the job fails with status None and the note `the job ended without a recorded status`, with the supervisor's pid.

### Exit codes

The `agora work wait` and `agora work run` commands exit with these codes. The exit code is the value of the status, masked to 0 through 255.

| Code | Meaning | From |
|---|---|---|
| 0 | Success. The job exited 0. | job |
| 1 | Refusal: wrong state, not accepted, or digest mismatch. A job that failed with no recorded status also exits 1. | CLI and job |
| 2 | Input error: unknown task id, bad name, bad flag | CLI |
| 124 | The waiter timed out (`--timeout`, default 570 s). The job keeps running. | CLI |
| 126 | The command was found but could not run: EACCES, EPERM, ENOEXEC, or EISDIR | job status |
| 127 | The command was not found: ENOENT | job status |
| 128+N | The job was killed by signal N, for example 143 for TERM | job status |
| 130 | Ctrl-C at the waiter. The job keeps running. | CLI |
| other | The job's own exit code | job |

A job that exits 124 by itself is not confused with a waiter timeout. The JSON result has `timed_out`, which is true only for the waiter. A job whose directory has gone away is finished as failed with status None and a note of the form `its directory X is gone`, so `wait` exits 1.

The program is located before the QoS wrapper is added, so 126 and 127 are reported for the command itself and are not masked by the wrapper.

Error texts follow Rust `io::Error` wording, not Python's. The meanings above are the same as `work.py`.

## 8. Cancel

`agora work cancel ID` works on queued, claimed, waiting, and running tasks. `work.py` cancels only tasks that are waiting for capacity. For a running task it can only send TERM to the process group by hand.

For a running task:

1. The daemon writes a `cancel` file in the job directory.
2. It sends SIGTERM to the verified supervisor pid.
3. The supervisor sends TERM to the command's group, and KILL after 5 s if the group is still there.
4. The job ends with its recorded status (for example 143) and the state `cancelled`. The note reads `cancelled by <pseudonym>`, plus `: <note>` if you gave one.

If the supervisor has already gone but the command is still running, the daemon checks the command's start time against the process. If it matches, the daemon sends TERM to the command's group, and KILL after 5 s if the group is still there. The job then ends `cancelled`.

A worker can cancel its own tasks. A supervisor, or higher, can cancel any task.

## 9. Waiting

`agora work wait` and `agora work run` wait on an event, not on a poll. The daemon subscribes to the event bus before it checks the condition, so an event that arrives during the check is not lost.

- A single server call returns after the condition holds, or after `min(timeout, 55 s)`. It returns a cursor. The CLI passes the cursor to the next call and repeats until its own `--timeout` (default 570 s). Then it exits 124. The job keeps running.
- The conditions are: task ids (any or all), a scope snapshot, cards reaching a state, a channel after a sequence number, notices, a pid, a file, a file that disappears, and a regex match in a log file.
- A notices wait with no `after` (`--after` on the CLI, `after` in MCP) waits for new notices only. The daemon treats `after < 0` as "the latest notice at call time", so a notice you already have does not return at once. To wait for notices after a given sequence number, pass `--after N` with `--notices` on the CLI, or `after` in MCP.
- Task, card, message, and notice conditions use the event bus.
- A pid condition polls liveness every 250 ms. There is no portable event source for process exit that the code can use without `unsafe`.
- File, gone, and log conditions use a file watcher on the parent directory. A healthy watcher also re-checks the condition at most every 60 s, as a safety net that bounds the effect of a missed event. If the watcher cannot be created, the wait polls every 1 s instead.
- **Relative paths.** A relative path given to `--file`, `--gone`, or `--log` (or to the `file`, `gone`, or `log_path` fields over MCP) is joined to the caller's current directory before the request reaches the daemon. For the CLI that is the shell's directory. For the stdio MCP server it is the directory the server runs in.
- **Resume hints.** A CLI wait that times out prints how to resume it. A task wait prints `agora work wait` with the ids of the tasks still open. A pid, file, gone, or log wait prints `agora work wait` with its targets (a log wait shows a `PATTERN` placeholder, because the pattern is not echoed). Every other condition (cards, messages, notices, and `--all`) prints `run the same wait again to keep waiting`. Over MCP, the command form is replaced by `work_wait` and the returned cursor.

**Log matching.** The regex is the Rust `regex` crate, not `grep -E`. It has no backreferences and no lookaround. Character classes are Unicode by default. It runs on complete lines and on the trailing partial line, from offset 0 at the start.

Over HTTP, the MCP tools refuse the calls that run commands or use paths on this machine. The list is in [section 19](#19-security-model). These need a local stdio connection, as filesystem paths do in the MCP server.

## 10. Timers

Timers are persisted. A timer can fire at a time, every so many seconds, or on a cron schedule.

```sh
agora work schedule add --at 2026-10-09T06:00:00Z -- make nightly
agora work schedule add --every 3600 --id hourly-check -- make check
agora work schedule add --cron '0 6 * * 1-5' -- make morning
agora work schedule list [--owner PK]
agora work schedule rm ID
```

| Kind | Rule |
|---|---|
| `at` | UTC milliseconds, or an RFC 3339 time ending in `Z`. The timer disables itself after it fires. |
| `every` | Seconds, at least 60. |
| `cron` | Five fields, UTC only. Names `jan` to `dec` and `sun` to `sat`, `*`, lists, ranges, and steps. If both the day-of-month and day-of-week fields are restricted, a day matches when either one does (the Vixie rule). A field that starts with `*`, including `*/n`, counts as unrestricted. So `0 0 */2 * mon` runs on odd days that are also Mondays. The search looks eight 366-day years ahead, so a schedule that never matches, such as `0 0 30 2 *`, never fires. |

UTC has no daylight saving time, so the cron schedule never skips or repeats an hour. Local-time schedules are not supported.

A fire enqueues the timer's task template as a new task. The author is the timer's owner. The task id is `<timer-id>-<n>`, and its origin is `timer:<id>`.

A fire is recorded at its scheduled instant, not at the moment the daemon woke, so a late wake does not move the schedule. If the daemon was down when a timer was due, the missed fires coalesce into one fire at startup. The next fire is computed from the time of that startup.

Each identity can have at most `timer_quota` enabled timers (default 32). An `every` timer must be at least 60 s.

## 11. Pools, leases, quiet windows

These are new in `agora work`. `work.py` has none of them.

- **Pools** are named resources with a capacity, for example a database that only three jobs may use at once. A controller defines a pool with `agora pool define NAME --capacity N`, and lists pools with `agora pool list`.
- **Leases** take an amount from a pool, or hold a path. A lease has a time to live, `lease_ttl` (default 900 s). A worker can renew and release its own leases. A supervisor, or higher, can release anyone's. A lease past its expiry is released by the reconciler (rule R1).
- **Quiet windows** are manual exclusive windows. Open one with `agora quiet open --for-secs N` (or `--until TIME`), close it with `agora quiet close LEASE`, and list open windows with `agora quiet status`. A quiet window keeps heavy jobs from starting for its length. The reconciler closes a window once its expected end plus `quiet_max` has passed (rule R2).

## 12. Cards, jobs, and the system identity

### Cards and jobs (D7)

A task can carry a card id, set by `add --card` or `run --card`. The card must exist, and the actor must own it or be a supervisor or higher. When the job ends, the daemon writes a `card.job_sync` event in the same transaction as the `job.finish` event. Only that system edge may change the card.

| Job ends with | Card transition | Owner fields | Evidence |
|---|---|---|---|
| status 0 (done) | Doing to Review. A Claimed card goes to Doing first. | unchanged | `job:<task-id> status=0 log=<path>` |
| non-zero status (failed) | to Blocked | `needs_owner = 1`; `owner_action` is `job <id> failed with status <s>: <note>; inspect <log> and re-run or cancel` | `job:<task-id> status=<s> log=<path>` (`<s>` is `none` when there is no status) |
| cancelled | Doing, Claimed, or Review to Ready | owner cleared | none |
| any state where the edge is illegal, for example Done or Cancelled | none. The sync is skipped, and a `reconcile.notice` tells the card owner why. | unchanged | none |

The card sync does nothing else. The daemon tests that `card.job_sync` cannot make any other transition.

### The system identity (D6)

The daemon holds a key of its own, stored at `$AGORA_HOME/system.seed`: 32 bytes, mode 0600, created when the daemon binds, opened without following symlinks. It is registered as a system identity with the observer role.

It can sign only these operation kinds:

- `job.start`, `job.finish`, `job.note`
- `card.job_sync`
- `timer.fire`
- `reconcile.expire`, `reconcile.notice`, `reconcile.reclaim`, `reconcile.blocker`, `reconcile.admission`, `reconcile.quiet_close`

Agent operations refuse the system identity. The system path refuses any non-system identity, and any kind not in this list. Both checks are covered by the authorization tests.

## 13. Reconciler and policies

The reconciler runs every `reconcile_interval` (default 30 s). It does not run on work-change events: the burst kick in the design (D12) was dropped, so it is interval-driven only. Each pass takes a snapshot, computes the actions with a pure function, and applies each action as a system operation. A pass emits at most 64 actions.

Each action has a key. The daemon records the keys it has applied, so an action is not repeated. A notice whose key includes the card's `updated_ms` is sent again only when the card has changed since.

| Rule | Looks at | Action | Defaults | Key |
|---|---|---|---|---|
| R1 | Leases past their expiry | Release the lease | `lease_ttl` 900 s | `expire:<id>:<expires_ms>` (O-36) |
| R2 | Quiet windows past expected end plus `quiet_max` | Close the window | `quiet_max` 180 s | `quiet:<id>:<expected_end_ms>` (O-36; 0 when unset) |
| R3 | Cards in claimed or doing, with an owner, that have been idle | Notice the owner at `stale_after`. Notice the escalation chain at `stale_after` plus `escalate_after`. Reclaim the card at `reclaim_after`, if reclaim is on. | 1800 s, 900 s (so the escalation comes at 2700 s), 3600 s, reclaim on | `stale1:<card>:<updated_ms>`, `stale2:<card>:<updated_ms>:<target>`, `reclaim:<card>:<updated_ms>`, `stale3:<card>:<updated_ms>` |
| R4 | A team and lane with more claimed and doing cards than its `wip_limit` | Notice each supervisor | no limit | `wip:<team>/<lane>:<count>:<hour>:<target>` (O-2) |
| R5 | Free disk, free memory, or idle CPU below its floor (checked only when both sides are known) | Pause admission and open a blocker card. Resume and close the card when all floors are met again. | disk floor 5 GiB; memory and idle floors off | `pause:` and `resume:` keyed to 10 minute buckets; `blocker:<bucket>`; `unblock:<card>` (O-1) |
| R6 | An active sprint past its end | Notice each supervisor to roll the sprint over | `sprint_rollover` on | `sprint:<id>:<target>` (O-2) |

Notes on the rules:

- **R3 reclaim** returns the card to Ready and clears the owner and approvals. It makes the card available again. The owner gets a notice with the kind `reclaimed`.
- **R5 blocker cards** are created as `blocker-<n>`, in the `ops` lane, in state blocked, with `needs_owner` set. The reconciler keeps the id in its state. When the floors recover, the card is cancelled with evidence `resolved at <time>`.
- **O-1.** If the floors are met and a blocker card is still open, the reconciler closes it even when admission was never paused. This clears a stale blocker.
- **R5 rate limit.** Pause and resume decisions are keyed by 10-minute buckets, so a breach that returns within the same bucket after a resume is re-paused at the next bucket.
- **O-2.** The keys for R4 and R6 include the target's public key. A supervisor added later still receives the notice. The SPEC's shorter key format is replaced by this one.
- **O-3.** The `owner_action` text is one string.

### Policies

Policies are set per team and lane. The fields are `stale_after_s`, `escalate_after_s`, `reclaim_after_s`, `reclaim`, `require_review`, `wip_limit`, `escalation_chain` (up to 16 public keys, in notification order), and `sprint_rollover`. Settings apply in layers, from most general to most specific: all teams and lanes, then the team, then the lane, then the team and lane. A later layer overrides an earlier one. Set a policy with `agora policy set TEAM LANE`, where `*` means any, and the flags `--stale-after`, `--escalate-after`, `--reclaim-after`, `--wip-limit`, and `--escalation-chain` (among others). `agora policy list` shows them.

When a lane has no escalation chain, the escalation notice goes to every supervisor.

## 14. Timed loops

These are the only timed loops in the work code. Anything else waits on an event, or on a timeout that ends a call. This list follows rule 0.7 of the build spec.

| Loop | Interval | Where | Purpose |
|---|---|---|---|
| Supervisor poll | 25 ms (1 ms once TERM has been sent) | supervisor | the command's exit, and the log cap, checked on each poll |
| Supervisor tick | 500 ms | supervisor | cancel file, TERM/INT/HUP flags, KILL after the grace period, and a process-tree sample every fourth tick |
| Supervisor sampling | first at about 1.5 s, then every 2 s | supervisor | `sample.json`: CPU and memory of the command's process tree |
| Admission re-check | 2 s | daemon | only while a job is held by an idle, memory, pressure, or load gate |
| Admission retry | 2 s | daemon | after an admission pass that fails |
| Pid liveness probe | 250 ms | daemon, `wait --pid` only | the wait on a process |
| Watcher safety re-check | 60 s | daemon, file, gone, and log waits | re-checks the condition while the watcher is healthy, bounding the effect of a missed event |
| Watcher fallback | 1 s | daemon, file, gone, and log waits | only when the file watcher cannot be created |
| Adopted supervisor check | 5 s | daemon, after a restart | liveness of supervisors the daemon did not start |
| Reconciler | 30 s by default (`reconcile_interval`) | daemon | rules R1 to R6; no event kick |
| Timer wake | until the next due fire | daemon (`Clock::sleep_until`) | fires timers; a sleep, not a loop |

Fixed timeouts, which end a call and do not repeat:

| Timeout | Value | Where |
|---|---|---|
| TERM to KILL grace | 5 s | log cap, cancel, stragglers, and orphaned commands |
| Job start wait | 30 s | the daemon waits this long for a new job's supervisor to start the job |
| Awaiter server cap | 55 s | each wait call |
| CLI wait timeout | 570 s (`--timeout`) | `run` and `wait` |

Tests replace the clock and the machine probe, so no test depends on wall time or host load.

## 15. Migrating from work.py

### Verb and flag mapping

| work.py | agora work | Notes |
|---|---|---|
| `work add [-- CMD]` | `agora work add [-- CMD]` | Same flags. `--agent` is ignored. |
| `work scope set/show` | `agora work scope set/show` | Same. |
| `work order` | `agora work order` | Byte-compatible with `--tasks` and `--task`. |
| `work plan --agents N` | `agora work plan --agents N` | `N` is at most 256 (see [section 18](#18-known-gaps)). |
| `work next` | `agora work next` | Also claims the first ready task. |
| `work claim ID` | `agora work claim ID` | Same. |
| `work accept ID --digest D` | `agora work accept ID --digest D` | The digest now binds the file list. |
| `work show ID` | `agora work show ID` | The command is framed as untrusted data. |
| `work run [TASK] [-- CMD]` | `agora work run [TASK] [-- CMD]` | Adds `--card`. `--timeout` and `--tail` are on `run` and `wait`. |
| `work wait …` | `agora work wait …` | `--interval` is ignored. |
| `work done/fail/cancel ID` | `agora work done/fail/cancel ID` | `cancel` also works on running tasks. |
| `work settings` | `agora work settings` | Adds the agora keys (section 16). |
| `work status` | `agora work status` | Same. |
| `work log` | `agora work log` | Same. |
| (none) | `agora work update ID`, `agora work reclaim`, `agora work schedule …` | New. `work.py` has no update, reclaim, or schedule verb. |
| (none) | `agora pool`, `agora quiet`, `agora policy`, `agora watchdog`, `agora metrics`, `agora notices` | New. Top-level, not under `work`. |

### What differs

| Topic | work.py | agora work |
|---|---|---|
| Identity | A name from `WORK_AGENT` or `--agent`. Not checked. | A signed key per folder. `WORK_AGENT` and `--agent` are ignored. |
| Digest | Binds argv and cwd. | Binds argv, cwd, and the file list. |
| Self-accept | An author can accept its own command. | Refused by default (`self_accept` is false). |
| Cancel of running jobs | Not available through the tool. | `agora work cancel` stops the job's process group. |
| Waiting | Polls at 0.05 s rising to 0.5 s, and sweeps every 2 s. | Event-driven. Each call returns after at most 55 s with a cursor. The CLI stops at its `--timeout` (570 s) with exit 124. |
| Log regex | `grep -E`, 60 s timeout. | The `regex` crate. No backreferences or lookaround. Unicode classes. |
| Timers | None. | `at`, `every`, and `cron`, in UTC. |
| Pools, leases, quiet windows | Automatic quiet windows only. | Pools, leases, and manual quiet windows, plus the automatic ones. |
| Admission gates | Machine gates fail a job after `load_wait`. | Also paused admission when a floor is breached (section 13). Disk, memory, and idle floors are settings. |
| `--interval` | Honored. | Accepted and ignored. |
| `--agent` | Honored. | Accepted and ignored. |
| `order` and `plan` with `--tasks` or `--task` | Reference output. | Byte-compatible (D18). |
| Agent count for `plan` | Not capped. | At most 256. Above that the CLI exits 2: `'N' is more than 256` with `--tasks` or `--task`, and `agents must be between 1 and 256` on the queue path. Plans are also capped at 2000 tasks (section 19). |
| Environment of a job | The claimant's full environment, plus `PWD`. Not stored. | The environment is scrubbed (section 19) and capped at 256 KiB. `PWD` is added. |
| Stuck claims | No timeout. Only the same name can close a claim. | `agora work reclaim` (supervisor or higher), and reclaim of stale cards by rule R3. |
| Retention | `keep_days`, pruned at most hourly. | Same setting. The reconciler deletes old job directories and events (section 17). |
| Error texts | Python wording. | Rust `io::Error` wording (D18 allows this). |
| Shell | Never used. | Never used. |

## 16. Settings reference

`agora work settings` shows the values. With flags, it changes them. Each `work.py` setting keeps its name and default. Rows marked "agora" are new. Flag spellings are the kebab-case form of the field names.

| Setting | Flag | Default | Notes |
|---|---|---|---|
| `cpu_budget` | `--cpu-budget CORES` | auto | A positive number, or `auto`. `off` is rejected. |
| `capacity` | `--capacity` | auto | Declared-weight cap. `auto` and `off` both mean no cap. |
| `memory_budget` | `--memory-budget SIZE` | auto | A size such as `14G` or `14336M`, or `auto` or `off`. `off` stores 0. |
| `idle_min` | `--idle-min PERCENT` | 10 | From 0 up to under 100. |
| `memory_limit` | `--memory-limit` | warning | `warning`, `critical`, or `off`. |
| `load_factor` | `--load-factor` | 1.5 | Positive. |
| `load_wait` | `--load-wait` | 1800 s | Positive. A held job fails after this long on a machine gate. |
| `log_limit` | `--log-limit BYTES` | 268435456 | At least 1. Soft cap (section 7). |
| `keep_days` | `--keep-days` | 14 | Positive. Retention (section 17). |
| `backfill_wait` | `--backfill-wait` | 300 s | Positive. |
| `small_weight` | `--small-weight` | 1 | Positive. |
| `aging` | `--aging` | 1 | At least 0. |
| `job_qos` | `--job-qos` | utility | `utility`, `default`, or `background`. Exclusive jobs run at default. |
| `quiet_drain` | `--quiet-drain` | 60 s | At least 0. |
| `quiet_max` | `--quiet-max` | 180 s | At least 0. |
| `self_accept` (agora) | `--self-accept` | false | Lets an author accept its own command (section 5). |
| `timer_quota` (agora) | `--timer-quota` | 32 | Enabled timers per identity. |
| `disk_floor` (agora) | `--disk-floor` | 5 GiB (5242880 KiB) | Reconciler floor for free disk (R5). |
| `memory_floor` (agora) | `--memory-floor` | off | Reconciler floor for available memory (R5). |
| `idle_floor` (agora) | `--idle-floor` | off | Reconciler floor for idle CPU percent (R5). |
| `reconcile_interval` (agora) | `--reconcile-interval` | 30 s | Reconciler period. |
| `lease_ttl` (agora) | `--lease-ttl` | 900 s | Default lease time to live. |
| `run_role` (agora) | `--run-role` | worker | The lowest role that may run or accept a command: `worker`, `supervisor`, `controller`, or `owner`. Section 19. |
| `max_waiting` (agora) | `--max-waiting` | 64 | Jobs one identity may hold claimed, requested, or waiting to start. Section 19. |

## 17. Retention and metrics

**Retention (D17).** The reconciler removes the directories and logs of tasks that ended more than `keep_days` ago. It removes only regular files, and it rebuilds each path from the task id, which must be a valid name. It also removes event rows of kinds `work.*`, `job.*`, `lease.*`, `timer.*`, `quiet.*`, `pool.*`, `policy.*`, and `reconcile.*` older than that window, along with leases released before it and reconcile marks and notices older than it.

**Metrics.** `agora metrics [--team T] [--lane L] [--since-ms MS]` reports flow metrics for a team or lane: lead and cycle time, WIP, blocked time, and queue waits. The per-job CPU time is read from the kernel's accounting for the supervisor's children, through `getrusage` (O-7). If that call fails, the value falls back to the samples taken during the run. This matters for short jobs: a job under about 1.5 s was never sampled, so without this it had no CPU time and was left out of the prediction history.

## 18. Known gaps

- **QoS on Linux (O-5).** The Linux branches of the QoS wrapper (`nice`, and `ionice` for background) are not compiled on the build host. They are a follow-up. On macOS the wrapper is `/usr/sbin/taskpolicy -c <qos>`, applied when `taskpolicy` is executable. Otherwise no wrapper is used.
- **Process tree sampling (O-6).** The sampler counts the root process and its descendants. It does not sum the whole process group.
- **Agent cap (O-11).** Plans are capped at 256 agents, to bound the cost of one plan.
- **Log cap.** The log cap is soft (section 7).
- **Scope quirks.** Tasks without a scope are hidden by any scope filter. File claims are checked across all scopes.
- **Case.** File comparisons are case-sensitive, even on case-insensitive macOS file systems.
- **Waiter.** A waiter that is stopped with Ctrl-C leaves its job running. `wait --all` is a snapshot.
- **Spawn errors.** ENOENT, and any spawn error outside the 126 group (EACCES, EPERM, ENOEXEC, EISDIR), is reported as 127, as D8 specifies.
- **Pid waits.** A pid wait polls every 250 ms. It is not event-driven.

## 19. Security model

### Trust boundary

The daemon, the supervisors it starts, the `agora` CLI, and the stdio MCP server all run as the same OS user. Anything that can reach the daemon's unix socket, or drive the stdio MCP server, can ask the daemon to run commands as that user. The socket and the stdio MCP server are therefore a code-execution interface. Roles limit what an identity may ask the daemon to do. They do not limit the local user.

The HTTP MCP server refuses the calls that run a command or use a path on this machine. They are `work_run`; `work_add` with a command, a working directory, or files; `work_accept` of a task that has a command; `work_wait` with `pid`, `file`, `gone`, or `log_path`; `pool_lease` with a path; and `work_schedule` with a command. Each is refused with a usage error whose message ends in `is not available over HTTP` (D16). For HTTP peers, the log tail is also removed from wait results, in both the JSON and the text, because a remote peer does not read job logs.

### The digest gate

The digest (D5) binds the argv, the working directory, and the file list. The daemon canonicalises the working directory when the task is added, and the directory must exist, so the digest covers the real path. A queued command runs only after its claimant accepts that exact digest. The author cannot accept its own command unless `self_accept` is on, and it is off by default.

When the job runs, the store checks that the claimant is the identity that accepted the command, and that the author is not running its own queued command while `self_accept` is off. The supervisor checks before it starts the command that the working directory still resolves to the path the digest covers. If it does not, the job does not start.

The gate protects cooperating agents. Keys are free (see F1), so an agent that registers a second identity can accept its own command. The digest does not bind the contents of the program or of the files it reads (see F3).

### Settings that limit running: `run_role` and `max_waiting`

- `run_role` (default `worker`) is the lowest role that may run or accept a command. Requests below it are refused with `Forbidden`.
- `max_waiting` (default 64) is the number of jobs one identity may hold claimed, requested, or waiting to start. A request beyond it is refused.

For sandboxed agents, either of these is a fix:

1. Set `run_role` to `supervisor`, and grant the supervisor role only to identities that should run commands. A sandboxed worker then cannot run anything.
2. Set `open_registration` to `false` in `$AGORA_HOME/config.json`. The default is `true`. New identities then need an explicit grant before they can act.

### Log tails

The daemon's task wait returns the tail of a task's log only to the task's author, its claimant, or an identity with the supervisor role or higher. Everyone else gets an empty tail. The CLI does not take the tail from the daemon. For a task that is final or whose wait timed out, it reads the log file itself, as the same OS user, so this rule does not limit it, and it escapes the lines with `printable`. The MCP server does not read logs. It frames the tail lines as untrusted data in its text, and its structured output keeps them under the untrusted-data notice. Over HTTP the MCP server removes the tail from both. A log wait (section 9) returns the last lines of the file it watches to its caller. It is refused over HTTP.

### Environment

A job's environment is the claimant's environment after scrubbing (D14). The scrub drops:

- names that contain, case-insensitively, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `PASSPHRASE`, `CREDENTIAL`, `API_KEY`, `APIKEY`, `ACCESS_KEY`, `PRIVATE`, `COOKIE`, `SESSION`, or `WEBHOOK`;
- names that end in `_KEY`, `_DSN`, `_AUTH`, or `_PAT`;
- names that start with `AGORA_`, `DYLD_`, or `LD_`;
- `SSH_AUTH_SOCK`, `GPG_AGENT_INFO`, and `DATABASE_URL`.

The scrub keeps at most 512 variables, in sorted order, with each value at most 32 KiB. Names and values together may not exceed 256 KiB. The daemon refuses a larger environment. The environment is not stored in the database and is not part of the digest.

### Caps

| Limit | Value | What happens above it |
|---|---|---|
| Agents in a plan | 256 | Refused (CLI exit 2, daemon, MCP) |
| Tasks in a plan | 2000 | Refused. The daemon refuses a queue of more than 2000 queued tasks in the scope. |
| Best-response rounds in `assign` | 10 000 | Stops, and returns the choices reached |
| Targets in one wait | 256 | Refused |
| Log regex | 1 MiB compiled size and 1 MiB DFA size | Refused with the regex error |
| Lease time to live | 7 days | Refused |
| Active leases per holder | 64 | Refused |
| Weight of a task | at least 0.1 | Refused: `weight must be at least 0.1` |
| Path depth of claims and path leases | at least 2 components, unless the actor is a supervisor or higher | Refused: `<path> is too broad to claim`. `/` and `/x` are too broad. |
| Escalation owner action | 1000 characters | Refused |
| Escalation targets | 16, each an active non-system identity | Refused |
| Waiting jobs per identity | `max_waiting`, default 64 | Refused |

### Signalling

The daemon signals a process only when it can verify it. A pid is signalled only with a recorded start time that matches the live process within 2 s, which guards against pid reuse. The orphaned-command path requires that start time. Process groups below 2 are refused, so the daemon never signals group 0 or 1.

### Known residual risks

Each residual has one sentence on what it is and one on what helps today.

- **F1, Sybil keys.** Keys are free, and while `open_registration` is `true` anyone who can reach the socket can register new identities, so roles and the digest gate protect only cooperating agents. Set `open_registration` to `false` and grant roles explicitly.
- **F3, content changes after accept.** The digest binds the argv, the working directory, and the file list, not the bytes of the program or of the files it reads, so content can change after accept. Read the program and the files it uses before you accept, and do not accept a command whose inputs another agent can write.
- **F7, self-declared priority.** The kind and priority of a task are declared by its author, so an author can claim a validity kind to move up the queue. A supervisor can reprioritise the task with `agora work update`.
- **F8, claim squatting.** A claimant can hold a task it never runs, because claims have no time to live yet, and automatic reclaim of claimed-but-never-run tasks is a follow-up. A supervisor can free the task with `agora work reclaim`.
- **F9, daemon-side path waits.** The `pid`, `file`, `gone`, and `log` waits run in the daemon, so they are not limited by an MCP host's sandbox. These waits are refused over HTTP. Keep sandboxed agents off the stdio MCP server, or do not use these conditions.
- **F11, filesystem probes in the writer.** Placement probes read the filesystem while the single writer holds the database, so a hung path can stall work operations. Keep queued working directories off network or otherwise hung mounts.
- **F12, inherited descriptors.** The supervisor does not close descriptors it inherited before it starts the command, so the command can use them. Start the daemon without sensitive files open.
- **F13, `supervisor.json` is writable by the job.** The job runs as the same user and can rewrite `supervisor.json`, so the file is not evidence of what happened. The job already has the same user's rights, so this grants no extra privilege. Do not treat the file as an audit record.
- **F14, check-then-spawn window.** The working-directory check and the spawn are not atomic, so a directory can be swapped between them. The realpath check narrows the window. Keep working directories out of paths that other users or agents can rename.
- **F15, `setsid` descendants.** A descendant that calls `setsid` leaves the process group, so a cancel does not reach it. Check for survivors with `ps` after a cancel.
- **F17, refused operations do not use up their nonce.** A refused signed operation can be replayed within its 5-minute window, and the replay succeeds if the state has since changed to allow it. Signed operations are accepted for five minutes only.
