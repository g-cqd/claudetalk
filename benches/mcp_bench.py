#!/usr/bin/env python3
"""MCP benchmark driver for the legacy Bun server and Agora.

Both servers run as stdio MCP children and are driven by this one file with the
same scenarios and the same timing. Python 3 standard library only.

Scenarios: startup, idle, latency, throughput. The method is in benches/README.md.

Modes:
  run      mcp_bench.py --backend {legacy,agora} --scenario NAME --out FRAGMENT.json
  merge    mcp_bench.py --merge FRAGMENT.json [...] --out RESULTS.json
  summary  mcp_bench.py --summarize RESULTS.json
"""

import argparse
import atexit
import datetime
import json
import math
import os
import platform
import re
import shlex
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass

PROTOCOL = "2025-11-25"
CHANNEL_METHOD = "notifications/claude/channel"
SCHEMA = 1
SCENARIOS = ("startup", "idle", "latency", "throughput")
SETTLE_S = 5.0  # idle: time after setup before the measurement window opens
RECEIVER_SETTLE_S = 2.5  # legacy: the receiver must observe the chat before the first post
RECEIVE_TIMEOUT_S = 15.0  # per message in the latency scenario
RETRY_DEADLINE_S = 120.0  # legacy: a post stops retrying on rate_limited after this long
MAX_INVITES = 64  # legacy groupchat accepts at most 64 invitees
BENCH_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(BENCH_DIR)
NS_PER_MS = 1_000_000
NS_PER_S = 1_000_000_000


class BenchError(RuntimeError):
    """A condition that makes the measurement invalid. The run stops."""


_CLEANUPS = []
ALL_SESSIONS = []


def register_cleanup(fn):
    _CLEANUPS.append(fn)


def run_cleanups():
    """Run registered cleanups newest first, once each. Safe to call again."""
    while _CLEANUPS:
        fn = _CLEANUPS.pop()
        try:
            fn()
        except Exception as exc:  # keep cleaning up the rest
            print(f"cleanup failed: {exc}", file=sys.stderr)


atexit.register(run_cleanups)


def now_utc():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def ns_to_ms(ns):
    return ns / NS_PER_MS


def token():
    return os.urandom(4).hex()


def run_capture(argv, env=None):
    """Stdout of a command, stripped, or None when it fails or prints nothing."""
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=30, check=True, env=env)
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip() or None


def signal_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        pass


@dataclass(frozen=True)
class Reply:
    """A JSON-RPC response with the send and receive timestamps (perf_counter_ns)."""

    msg: dict
    sent: int
    recv: int

    @property
    def rtt_ns(self):
        return self.recv - self.sent

    @property
    def result(self):
        return self.msg.get("result") or {}


def result_text(result):
    return "\n".join(b.get("text", "") for b in result.get("content", []) if b.get("type") == "text")


class Session:
    """One MCP server child. A reader thread files responses by id and notifications by arrival."""

    def __init__(self, label, argv, env, cwd, log_path):
        self.label = label
        self.log_path = log_path
        self.log = open(log_path, "wb")
        self.state = {}  # backend-private state for this session
        self._cond = threading.Condition()
        self._send_lock = threading.Lock()
        self._responses = {}
        self._next_id = 0
        self._eof = False
        self.notifications = []  # (arrival_ns, method, params), append-only
        self.t_spawn = time.perf_counter_ns()
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log,
            env=env,
            cwd=cwd,
            start_new_session=True,  # own process group, so close() can signal the whole tree
        )
        self._reader = threading.Thread(target=self._read_loop, name=f"reader-{label}", daemon=True)
        self._reader.start()
        ALL_SESSIONS.append(self)
        register_cleanup(self.close)

    def _write(self, obj):
        data = (json.dumps(obj, separators=(",", ":")) + "\n").encode()
        with self._send_lock:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()

    def _read_loop(self):
        for raw in self.proc.stdout:
            arrived = time.perf_counter_ns()
            try:
                msg = json.loads(raw)
            except ValueError:
                continue  # a non-JSON line is not a protocol message
            if not isinstance(msg, dict):
                continue
            if "method" in msg and "id" in msg:  # server-to-client request: answer it, do not hang
                try:
                    self._write({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
                except OSError:
                    pass
                continue
            with self._cond:
                if "method" in msg:
                    self.notifications.append((arrived, msg["method"], msg.get("params") or {}))
                elif "id" in msg:
                    self._responses[msg["id"]] = (arrived, msg)
                self._cond.notify_all()
        with self._cond:
            self._eof = True
            self._cond.notify_all()

    def call(self, method, params=None, timeout=30.0):
        with self._cond:
            self._next_id += 1
            rid = self._next_id
        sent = time.perf_counter_ns()
        try:
            self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        except OSError as exc:
            raise BenchError(f"{self.label}: cannot send {method}: {exc}") from exc
        deadline = time.monotonic() + timeout
        with self._cond:
            while rid not in self._responses:
                if self._eof:
                    raise BenchError(f"{self.label}: server closed stdout during {method}")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise BenchError(f"{self.label}: {method} timed out after {timeout:g} s")
                self._cond.wait(left)
            recv, msg = self._responses.pop(rid)
        return Reply(msg, sent, recv)

    def tool(self, name, arguments, timeout=60.0):
        reply = self.call("tools/call", {"name": name, "arguments": arguments}, timeout)
        if "error" in reply.msg:
            raise BenchError(f"{self.label}: {name} failed: {reply.msg['error']}")
        if reply.result.get("isError"):
            raise BenchError(f"{self.label}: {name} returned an error: {result_text(reply.result)[:300]}")
        return reply

    def initialize(self, timeout=30.0):
        reply = self.call(
            "initialize",
            {"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "mcp-bench", "version": "1"}},
            timeout,
        )
        if "error" in reply.msg:
            raise BenchError(f"{self.label}: initialize failed: {reply.msg['error']}")
        self._write({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return reply

    def wait_until(self, check, timeout):
        """Block until check(notifications) returns non-None; return it, or None on timeout."""
        deadline = time.monotonic() + timeout
        with self._cond:
            while True:
                hit = check(self.notifications)
                if hit is not None:
                    return hit
                if self._eof:
                    raise BenchError(f"{self.label}: server closed stdout while waiting")
                left = deadline - time.monotonic()
                if left <= 0:
                    return None
                self._cond.wait(left)

    def close(self, grace=3.0):
        if self.proc.poll() is None:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
            try:
                self.proc.wait(grace)
            except subprocess.TimeoutExpired:
                signal_group(self.proc.pid, signal.SIGTERM)
                try:
                    self.proc.wait(grace)
                except subprocess.TimeoutExpired:
                    signal_group(self.proc.pid, signal.SIGKILL)
                    self.proc.wait()
        if not self.log.closed:
            self.log.close()


class Context:
    """Per-run paths and the child environment. Children never see the real HOME."""

    def __init__(self, data_home, child_home, workdir):
        self.data_home = data_home
        self.child_home = child_home
        self.workdir = workdir
        self._count = 0

    def new_folder(self):
        self._count += 1
        path = os.path.join(self.workdir, "folders", f"s{self._count:04d}")
        os.makedirs(path)
        return path

    def env(self, **extra):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.child_home,
            "TMPDIR": self.child_home,
            "LANG": os.environ.get("LANG", "C.UTF-8"),
        }
        env.update(extra)
        return env

    def spawn(self, label, argv, env, cwd):
        self._count += 1
        log = os.path.join(self.workdir, "logs", f"{self._count:04d}-{label}.log")
        os.makedirs(os.path.dirname(log), exist_ok=True)
        return Session(label, argv, env, cwd, log)


class LegacyBackend:
    """The Bun server. Identity is the folder; receive is the 1 s channel push."""

    name = "legacy"
    # The server rate-limits each (session, tool) to 30 calls per 10 s
    # (legacy/src/rate-limit.ts). The latency scenario paces posts under that.
    post_gap_s = 0.4

    def __init__(self, ctx, argv):
        self.ctx = ctx
        self.argv = argv
        self.rate_limited_retries = 0
        self._lock = threading.Lock()  # guards rate_limited_retries across writer threads

    def spawn(self):
        folder = self.ctx.new_folder()
        env = self.ctx.env(CLAUDETALK_HOME=self.ctx.data_home, CLAUDE_PROJECT_DIR=folder)
        return self.ctx.spawn("legacy", self.argv, env, folder)

    def identity(self, s):
        text = result_text(s.tool("whoami", {}).result)
        m = re.search(r"You are:\s*(\S+)", text)
        if not m:
            raise BenchError(f"whoami gave no pseudonym: {text[:200]!r}")
        return m.group(1)

    def create_group(self, creator, group, member_ids):
        if len(member_ids) > MAX_INVITES:
            raise BenchError(f"legacy groupchat takes at most {MAX_INVITES} invitees, got {len(member_ids)}")
        creator.tool("groupchat", {"slug": group, "invite": member_ids, "history": 0})

    def post(self, s, group, text):
        # Throughput exceeds the server's rate limit by design. On a
        # rate_limited error, wait and retry; the reply returned is the
        # attempt that succeeded, so the limiter's wait counts in wall time
        # but not in the call round trip.
        deadline = time.monotonic() + RETRY_DEADLINE_S
        while True:
            try:
                return s.tool("groupchat", {"slug": group, "message": text, "history": 0})
            except BenchError as exc:
                if "rate_limited" not in str(exc):
                    raise
                if time.monotonic() >= deadline:
                    raise BenchError(f"{s.label}: still rate limited after {RETRY_DEADLINE_S:g} s") from exc
                with self._lock:
                    self.rate_limited_retries += 1
                time.sleep(0.25)

    def wait_for_message(self, s, group, marker, timeout):
        chat_id = f"group:{group}"

        def hit(notes):
            for arrived, method, params in notes:
                meta = params.get("meta") or {}
                if method == CHANNEL_METHOD and meta.get("chat_id") == chat_id:
                    if marker in str(params.get("content", "")):
                        return arrived
            return None

        return s.wait_until(hit, timeout)

    def daemon_pids(self):
        return []

    def versions(self):
        return {"bun": run_capture(["bun", "--version"], env=self.ctx.env())}


class AgoraBackend:
    """The Rust shim plus its shared daemon. Identity is the folder; receive is inbox_wait."""

    name = "agora"
    post_gap_s = 0.05

    def __init__(self, ctx, argv):
        self.ctx = ctx
        self.argv = argv
        self.bin = argv[0]
        register_cleanup(self.stop_daemon)  # registered first, so it runs last

    def spawn(self):
        folder = self.ctx.new_folder()
        env = self.ctx.env(AGORA_HOME=self.ctx.data_home, AGORA_FOLDER=folder)
        return self.ctx.spawn("agora", self.argv, env, folder)

    def identity(self, s):
        pk = (s.tool("whoami", {}).result.get("structuredContent") or {}).get("pubkey", "")
        if not re.fullmatch(r"[0-9a-f]{64}", pk):
            raise BenchError(f"whoami gave no 64-hex public key: {pk!r}")
        s.state.update(cursor=0, seen=[])
        return pk

    def create_group(self, creator, group, member_ids):
        creator.tool("channel_create", {"id": group, "kind": "group", "members": member_ids, "encrypt": False})

    def post(self, s, group, text):
        return s.tool("chat", {"channel": group, "text": text})

    def wait_for_message(self, s, group, marker, timeout):
        deadline = time.monotonic() + timeout
        seen = s.state["seen"]  # (arrival_ns, channel, text) for every message this session received
        while True:
            for arrived, channel, text in seen:
                if channel == group and marker in text:
                    return arrived
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            wait_s = min(55, max(1, math.ceil(left)))
            reply = s.tool(
                "inbox_wait",
                {"cursor": s.state["cursor"], "timeout_secs": wait_s, "limit": 200},
                timeout=wait_s + 30,
            )
            sc = reply.result.get("structuredContent") or {}
            s.state["cursor"] = sc.get("cursor", s.state["cursor"])
            top = {}
            for m in sc.get("messages", []):
                seen.append((reply.recv, m["channel"], m["text"]))
                top[m["channel"]] = max(top.get(m["channel"], 0), m["seq"])
            for channel, seq in top.items():
                s.tool("ack", {"channel": channel, "seq": seq})

    def daemon_pids(self):
        return find_daemon_pids(self.ctx.data_home)

    def stop_daemon(self):
        """Stop the daemon this run started, and only that one (matched by its exact --home)."""
        run_capture([self.bin, "stop", "--home", self.ctx.data_home], env=self.ctx.env(AGORA_HOME=self.ctx.data_home))
        for pid in find_daemon_pids(self.ctx.data_home):
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
            print(f"warning: daemon pid {pid} survived 'agora stop'; sent SIGTERM", file=sys.stderr)

    def versions(self):
        return {"agora": run_capture([self.bin, "--version"], env=self.ctx.env(AGORA_HOME=self.ctx.data_home))}


def find_daemon_pids(home):
    """PIDs of `agora daemon --home HOME` for exactly this home (no prefix matches)."""
    out = run_capture(["ps", "-axww", "-o", "pid=,command="]) or ""
    pattern = re.compile(r"agora daemon --home " + re.escape(home) + r"(\s|$)")
    pids = []
    for line in out.splitlines():
        m = re.match(r"\s*(\d+)\s+(.*)$", line)
        if m and pattern.search(m.group(2)):
            pids.append(int(m.group(1)))
    return pids


def parse_cputime(text):
    """Seconds from ps cputime/time: [[dd-]hh:]mm:ss[.ss]."""
    days = 0
    if "-" in text:
        d, text = text.split("-", 1)
        days = int(d)
    secs = 0.0
    for part in text.split(":"):
        secs = secs * 60 + float(part)
    return days * 86400 + secs


def ps_snapshot(pids):
    """pid -> (cpu seconds, rss KiB) for the pids that are still alive."""
    if not pids:
        return {}
    out = run_capture(["ps", "-o", "pid=,cputime=,rss=", "-p", ",".join(str(p) for p in pids)]) or ""
    snap = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3:
            snap[int(parts[0])] = (parse_cputime(parts[1]), int(parts[2]))
    return snap


def percentile(sorted_values, p):
    """Nearest-rank percentile of an ascending list."""
    rank = max(1, math.ceil(p / 100 * len(sorted_values)))
    return sorted_values[rank - 1]


def summarize(values):
    if not values:
        return {"n": 0}
    v = sorted(values)
    return {
        "n": len(v),
        "min": round(v[0], 3),
        "mean": round(statistics.fmean(v), 3),
        "p50": round(percentile(v, 50), 3),
        "p90": round(percentile(v, 90), 3),
        "p99": round(percentile(v, 99), 3),
        "max": round(v[-1], 3),
    }


def rounded(values):
    return [round(x, 3) for x in values]


def scenario_startup(be, args):
    daemon_at_start = bool(be.daemon_pids())
    samples = []
    for _ in range(args.sessions):
        s = be.spawn()
        try:
            init = s.initialize()
            be.identity(s)
            ready = time.perf_counter_ns()
        finally:
            s.close()
        samples.append({"initialize_ms": ns_to_ms(init.recv - s.t_spawn), "ready_ms": ns_to_ms(ready - s.t_spawn)})
    init_ms = [x["initialize_ms"] for x in samples]
    ready_ms = [x["ready_ms"] for x in samples]
    return {
        "sessions": len(samples),
        "daemon_running_at_start": daemon_at_start,  # True means the first session is not cold
        "cold": {"initialize_ms": round(init_ms[0], 3), "ready_ms": round(ready_ms[0], 3)},
        "warm": {
            "n": len(samples) - 1,
            "initialize_ms": summarize(init_ms[1:]),
            "ready_ms": summarize(ready_ms[1:]),
        },
        "samples": {"initialize_ms": rounded(init_ms), "ready_ms": rounded(ready_ms)},
    }


def scenario_idle(be, args):
    group = f"bench-idle-{token()}"
    sessions, ids = [], []
    for _ in range(args.sessions):
        s = be.spawn()
        sessions.append(s)
        s.initialize()
        ids.append(be.identity(s))
    be.create_group(sessions[0], group, ids[1:])
    time.sleep(SETTLE_S)
    daemons = be.daemon_pids()
    session_pids = [s.proc.pid for s in sessions]
    pids = session_pids + daemons
    before = ps_snapshot(pids)
    t0 = time.perf_counter_ns()
    time.sleep(args.idle_seconds)
    after = ps_snapshot(pids)
    window = (time.perf_counter_ns() - t0) / NS_PER_S

    def cpu_of(group_pids):
        return sum(after[p][0] - before[p][0] for p in group_pids if p in before and p in after)

    server_cpu = cpu_of(session_pids)
    daemon_cpu = cpu_of(daemons)
    total_cpu = server_cpu + daemon_cpu
    session_rss = [after[p][1] for p in session_pids if p in after]
    daemon_rss = sum(after[p][1] for p in daemons if p in after)
    total_rss_kib = sum(session_rss) + daemon_rss
    mean_rss_kib = statistics.fmean(session_rss) if session_rss else 0.0
    return {
        "sessions": len(sessions),
        "settle_s": SETTLE_S,
        "window_s": round(window, 3),
        "cpu_seconds": {"sessions": round(server_cpu, 3), "daemon": round(daemon_cpu, 3), "total": round(total_cpu, 3)},
        "cpu_percent_of_one_core": round(total_cpu / window * 100, 3),
        "cpu_ms_per_second": round(total_cpu * 1000 / window, 3),
        "rss_mib": {
            "per_session_mean": round(mean_rss_kib / 1024, 3),
            "daemon": round(daemon_rss / 1024, 3),
            "total": round(total_rss_kib / 1024, 3),
            "amortized_per_session": round(total_rss_kib / 1024 / len(sessions), 3),
        },
        "pids_missing_after_window": [p for p in pids if p not in after],
        "daemon_pids": daemons,
    }


def scenario_latency(be, args):
    sender, receiver = be.spawn(), be.spawn()
    sender.initialize()
    receiver.initialize()
    be.identity(sender)
    receiver_id = be.identity(receiver)
    group = f"bench-lat-{token()}"
    be.create_group(sender, group, [receiver_id])
    time.sleep(RECEIVER_SETTLE_S)  # the receiver must observe the chat before the first post
    run = token()
    markers = [f"{run}-m{i:05d}" for i in range(args.messages)]
    starts, arrivals, errors = {}, {}, []

    def receive():
        try:
            for marker in markers:
                arrived = be.wait_for_message(receiver, group, marker, RECEIVE_TIMEOUT_S)
                if arrived is not None:
                    arrivals[marker] = arrived
        except BenchError as exc:
            errors.append(str(exc))

    rx = threading.Thread(target=receive, name="latency-receiver", daemon=True)
    rx.start()
    rtts = []
    for marker in markers:
        reply = be.post(sender, group, f"latency {marker}")
        starts[marker] = reply.sent
        rtts.append(ns_to_ms(reply.rtt_ns))
        time.sleep(be.post_gap_s)
    deadline = time.monotonic() + RECEIVE_TIMEOUT_S
    while rx.is_alive() and time.monotonic() < deadline:
        rx.join(0.5)
    obs = [ns_to_ms(arrivals[m] - starts[m]) for m in markers if m in arrivals]
    return {
        "messages": len(markers),
        "received": len(obs),
        "lost": len(markers) - len(obs),
        "rate_limited_retries": getattr(be, "rate_limited_retries", 0),
        "post_gap_s": be.post_gap_s,
        "post_rtt_ms": summarize(rtts),
        "receive_ms": summarize(obs),
        "samples": {"post_rtt_ms": rounded(rtts), "receive_ms": rounded(obs)},
        "receiver_errors": errors[:3],
    }


def scenario_throughput(be, args):
    writers = [be.spawn() for _ in range(args.writers)]
    ids = []
    for s in writers:
        s.initialize()
        ids.append(be.identity(s))
    group = f"bench-tp-{token()}"
    be.create_group(writers[0], group, ids[1:])
    run = token()
    replies = [[] for _ in writers]
    errors = []
    barrier = threading.Barrier(len(writers))

    def writer(w):
        try:
            barrier.wait(timeout=60)
            for j in range(args.per_writer):
                replies[w].append(be.post(writers[w], group, f"throughput {run} w{w} {j}"))
        except (BenchError, threading.BrokenBarrierError) as exc:
            errors.append(f"writer {w}: {exc}")

    threads = [threading.Thread(target=writer, args=(w,), name=f"writer-{w}", daemon=True) for w in range(len(writers))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if errors:
        raise BenchError("; ".join(errors[:3]))
    flat = [r for rs in replies for r in rs]
    wall_ns = max(r.recv for r in flat) - min(r.sent for r in flat)
    wall_s = wall_ns / NS_PER_S
    rtts = [ns_to_ms(r.rtt_ns) for r in flat]
    return {
        "writers": len(writers),
        "per_writer": args.per_writer,
        "messages": len(flat),
        "wall_s": round(wall_s, 3),
        "messages_per_s": round(len(flat) / wall_s, 3),
        "call_rtt_ms": summarize(rtts),
        "rate_limited_retries": getattr(be, "rate_limited_retries", 0),
        "samples": {"call_rtt_ms": rounded(rtts)},
    }


SCENARIO_FUNCS = {
    "startup": scenario_startup,
    "idle": scenario_idle,
    "latency": scenario_latency,
    "throughput": scenario_throughput,
}


def guard_data_home(env_name):
    """The data home must be a fresh temp directory, never the user's real data."""
    value = os.environ.get(env_name, "")
    if not value:
        raise BenchError(f"{env_name} is unset; point it at a fresh temporary directory (benches/run.sh does this)")
    if not os.path.isabs(value):
        raise BenchError(f"{env_name} must be an absolute path, got {value!r}")
    real = os.path.realpath(value)
    home = os.path.realpath(os.path.expanduser("~"))
    for protected in (os.path.join(home, ".claudetalk"), os.path.join(home, ".agora")):
        if real == protected or real.startswith(protected + os.sep):
            raise BenchError(f"{env_name} points at {protected}; refusing to benchmark against real data")
    if real == home:
        raise BenchError(f"{env_name} is the home directory; refusing")
    if not os.path.isdir(real):
        raise BenchError(f"{env_name} does not exist: {real}")
    return real


def resolve_server_argv(args):
    if args.server_cmd:
        argv = shlex.split(args.server_cmd)
    elif args.backend == "legacy":
        argv = ["bun", "run", os.path.join(REPO_ROOT, "legacy", "src", "server.ts")]
    else:
        argv = [os.path.join(REPO_ROOT, "target", "release", "agora"), "mcp"]
    if not argv:
        raise BenchError("--server-cmd is empty")
    resolved = shutil.which(argv[0])
    if resolved is None:
        raise BenchError(f"server executable not found: {argv[0]}")
    argv[0] = resolved
    if args.backend == "agora" and argv[1:] != ["mcp"]:
        raise BenchError("the agora server command must be '<agora-bin> mcp'")
    if args.backend == "legacy" and (len(argv) < 3 or not os.path.isfile(argv[-1])):
        raise BenchError("the legacy server command must end with the path to legacy/src/server.ts")
    return argv


def machine_info():
    cpu = run_capture(["sysctl", "-n", "machdep.cpu.brand_string"])
    if cpu is None and os.path.exists("/proc/cpuinfo"):
        with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as fh:
            cpu = next((ln.split(":", 1)[1].strip() for ln in fh if ln.startswith("model name")), None)
    mem = run_capture(["sysctl", "-n", "hw.memsize"])
    ram = int(mem) if mem and mem.isdigit() else None
    if ram is None and os.path.exists("/proc/meminfo"):
        with open("/proc/meminfo", encoding="utf-8") as fh:
            kb = next((ln.split()[1] for ln in fh if ln.startswith("MemTotal:")), None)
        ram = int(kb) * 1024 if kb and kb.isdigit() else None
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu_model": cpu,
        "logical_cores": os.cpu_count(),
        "ram_bytes": ram,
        "hostname": socket.gethostname(),
    }


def run_backend(args):
    env_name = "AGORA_HOME" if args.backend == "agora" else "CLAUDETALK_HOME"
    data_home = guard_data_home(env_name)
    argv = resolve_server_argv(args)
    child_home = tempfile.mkdtemp(prefix="agora-bench-home.")
    workdir = tempfile.mkdtemp(prefix="agora-bench-work.")
    register_cleanup(lambda: shutil.rmtree(child_home, ignore_errors=True))
    register_cleanup(lambda: shutil.rmtree(workdir, ignore_errors=True))
    ctx = Context(data_home, child_home, workdir)
    be = AgoraBackend(ctx, argv) if args.backend == "agora" else LegacyBackend(ctx, argv)
    names = list(SCENARIOS) if args.scenario == "all" else [args.scenario]
    started = now_utc()
    results = {}
    try:
        for name in names:
            print(f"[{args.backend}] {name} ...", file=sys.stderr)
            t0 = time.monotonic()
            results[name] = SCENARIO_FUNCS[name](be, args)
            print(f"[{args.backend}] {name} done in {time.monotonic() - t0:.1f} s", file=sys.stderr)
        versions = be.versions()
    except Exception:  # a failed measurement: show the server logs; Ctrl-C and SIGTERM stay quiet
        for s in ALL_SESSIONS[-3:]:
            print(f"--- last lines of {s.log_path} ---", file=sys.stderr)
            try:
                with open(s.log_path, encoding="utf-8", errors="replace") as fh:
                    print("".join(fh.readlines()[-10:]), file=sys.stderr)
            except OSError:
                pass
        raise
    finally:
        run_cleanups()
    git = run_capture(["git", "-C", REPO_ROOT, "rev-parse", "--short", "HEAD"])
    dirty = run_capture(["git", "-C", REPO_ROOT, "status", "--porcelain"]) is not None
    return {
        "schema": SCHEMA,
        "backend": args.backend,
        "server_cmd": shlex.join(argv),
        "machine": machine_info(),
        "tools": {"repo_commit": git, "repo_dirty": dirty, **versions},
        "parameters": {
            "sessions": args.sessions,
            "messages": args.messages,
            "writers": args.writers,
            "per_writer": args.per_writer,
            "idle_seconds": args.idle_seconds,
            "receive_timeout_s": RECEIVE_TIMEOUT_S,
            "protocol_version": PROTOCOL,
        },
        "started_utc": started,
        "finished_utc": now_utc(),
        "scenarios": results,
    }


def write_json(path, doc):
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")


def load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def merge_fragments(paths, out):
    doc = {"schema": SCHEMA, "generated_utc": now_utc(), "runs": {}}
    for path in paths:
        frag = load_json(path)
        if frag.get("schema") != SCHEMA:
            raise BenchError(f"{path}: unsupported schema {frag.get('schema')}")
        doc.setdefault("machine", frag["machine"])
        doc.setdefault("parameters", frag["parameters"])
        run = doc["runs"].setdefault(
            frag["backend"],
            {"server_cmd": frag["server_cmd"], "tools": frag["tools"], "started_utc": frag["started_utc"], "scenarios": {}},
        )
        for name, body in frag["scenarios"].items():
            if name in run["scenarios"]:
                raise BenchError(f"{path}: scenario {name} for {frag['backend']} appears twice")
            run["scenarios"][name] = body
        run["finished_utc"] = frag["finished_utc"]
    write_json(out, doc)


def cell(run, paths):
    """Values at dotted paths inside a run's scenarios, joined with ' / '."""
    out = []
    for path in paths:
        node = run
        for key in path.split("."):
            node = node.get(key) if isinstance(node, dict) else None
        out.append("n/a" if node is None else f"{node:.2f}" if isinstance(node, float) else str(node))
    return " / ".join(out)


SUMMARY_ROWS = [
    ("startup", "cold initialize (ms)", ["scenarios.startup.cold.initialize_ms"]),
    ("startup", "cold ready, initialize + whoami (ms)", ["scenarios.startup.cold.ready_ms"]),
    ("startup", "warm initialize p50 / p90 (ms)", ["scenarios.startup.warm.initialize_ms.p50", "scenarios.startup.warm.initialize_ms.p90"]),
    ("startup", "warm ready p50 / p90 (ms)", ["scenarios.startup.warm.ready_ms.p50", "scenarios.startup.warm.ready_ms.p90"]),
    ("idle", "CPU % of one core", ["scenarios.idle.cpu_percent_of_one_core"]),
    ("idle", "CPU ms per second", ["scenarios.idle.cpu_ms_per_second"]),
    ("idle", "RSS per session mean (MiB)", ["scenarios.idle.rss_mib.per_session_mean"]),
    ("idle", "RSS daemon (MiB)", ["scenarios.idle.rss_mib.daemon"]),
    ("idle", "RSS total / sessions (MiB)", ["scenarios.idle.rss_mib.amortized_per_session"]),
    ("latency", "post RTT p50 / p90 / p99 / max (ms)", [f"scenarios.latency.post_rtt_ms.{k}" for k in ("p50", "p90", "p99", "max")]),
    ("latency", "receive p50 / p90 / p99 / max (ms)", [f"scenarios.latency.receive_ms.{k}" for k in ("p50", "p90", "p99", "max")]),
    ("latency", "lost messages", ["scenarios.latency.lost"]),
    ("throughput", "messages per second", ["scenarios.throughput.messages_per_s"]),
    ("throughput", "wall seconds", ["scenarios.throughput.wall_s"]),
    ("throughput", "call RTT p50 / p99 (ms)", ["scenarios.throughput.call_rtt_ms.p50", "scenarios.throughput.call_rtt_ms.p99"]),
]


def summarize_file(path):
    doc = load_json(path)
    runs = doc["runs"]
    order = [b for b in ("agora", "legacy") if b in runs]
    lines = [
        f"Results {doc['generated_utc']} on {doc['machine']['platform']}, {doc['machine']['cpu_model']}, "
        f"{doc['machine']['logical_cores']} logical cores",
        "",
        "| Scenario | Metric | " + " | ".join(order) + " |",
        "|---|---|" + "---|" * len(order),
    ]
    for scenario, label, paths in SUMMARY_ROWS:
        cells = []
        for b in order:
            run = {"scenarios": runs[b]["scenarios"]}
            # Scenario absent from this run: show n/a rather than a stale value.
            cells.append(cell(run, paths) if scenario in runs[b]["scenarios"] else "n/a")
        lines.append(f"| {scenario} | {label} | " + " | ".join(cells) + " |")
    print("\n".join(lines))


def build_parser():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--backend", choices=("legacy", "agora"))
    p.add_argument("--server-cmd", help="server command, shell-split; default: bun run legacy/src/server.ts, or target/release/agora mcp")
    p.add_argument("--scenario", choices=SCENARIOS + ("all",), default="all")
    p.add_argument("--sessions", type=int, default=20)
    p.add_argument("--messages", type=int, default=200)
    p.add_argument("--writers", type=int, default=4)
    p.add_argument("--per-writer", type=int, default=250)
    p.add_argument("--idle-seconds", type=float, default=60.0)
    p.add_argument("--out", help="output JSON (fragment in run mode, results in merge mode)")
    p.add_argument("--merge", nargs="+", metavar="FRAGMENT", help="merge run fragments into --out")
    p.add_argument("--summarize", metavar="RESULTS", help="print the Markdown summary of a results file")
    return p


def validate(args):
    for name in ("sessions", "messages", "writers", "per_writer"):
        if getattr(args, name) < 1:
            raise BenchError(f"--{name.replace('_', '-')} must be at least 1")
    if args.idle_seconds <= 0:
        raise BenchError("--idle-seconds must be positive")


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        if args.merge:
            if not args.out:
                raise BenchError("--merge needs --out")
            merge_fragments(args.merge, args.out)
            return 0
        if args.summarize:
            summarize_file(args.summarize)
            return 0
        if not args.backend or not args.out:
            raise BenchError("run mode needs --backend and --out")
        validate(args)
        signal.signal(signal.SIGTERM, lambda signum, frame: sys.exit(128 + signum))
        fragment = run_backend(args)
        write_json(args.out, fragment)
        print(f"wrote {args.out}", file=sys.stderr)
        return 0
    except BenchError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("interrupted; spawned servers were closed", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
