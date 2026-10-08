#!/usr/bin/env python3
"""Generate Python-oracle fixtures for agora-sched admission, queue order, head-start and prediction (R1-18).

python3 -I gen_admission.py --out DIR [--work-dir DIR]

Writes DIR/<n>.json, one case per file, with kind:
  blocker     Supervisor.blocker (decide_one): running set, waiting line, candidate, budgets, signals, drain rows
  order       waiting_order (queue order of a waiting line, with aging)
  head_start  head_start_estimate (earliest start of the head of the line)
  predict     predict (history levels, exclusive typical use, no-history fallback), run against sqlite

The expected values come from work.py itself, run in-process. Its SQL runs against a real in-memory sqlite database.
Time (time.time), host memory (total_memory) and the CPU count (host_cpu_capacity) are injected, so nothing reads the
clock or the machine. A fixed seed makes the output deterministic: running twice gives byte-identical files.
Not covered by this oracle (work.py has no pure function for them): the tick-level load-wait failure, paused and
manual quiet windows (Rust-only M5 additions), and the waiting-line filter that the supervisor applies before blocker.
"""
import argparse
import json
import os
import random
import sqlite3
import sys
import types

DEFAULT_WORK_DIR = "/Users/gc/Developer/skills/skills/planner/scripts"
SEED = 20260710
NOW = 100000.0
BLOCKER_CASES, ORDER_CASES, HEAD_CASES, PREDICT_CASES = 80, 8, 8, 24
WEIGHTS = [0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0]
CPUS = [1.0, 2.0, 4.0, 8.0, 12.0, 16.0]
KINDS = ["Work", "work", "validity", "Security", "crash", "docs", "correctness", " crash", "refactor"]
DURATIONS = [None, 30.0, 60.0, 600.0, 600.0, 45.5]
REQUESTS = [0, 50, 100, 300, 600]
COLS = ["id", "weight", "requested", "started", "ended", "quiet_until", "cpu_time", "rss_peak", "status",
        "sig_full", "sig_head", "sig_prog", "sig_tool"]
LEVELS = ("full", "head", "prog", "tool")
POOLS = {"full": ["F1", "F2"], "head": ["H1", "H2"], "prog": ["P1"], "tool": ["T1"]}


def load_work(work_dir):
    sys.path.insert(0, work_dir)
    import work  # noqa: E402  (imported from the planner skill, not from this directory)
    return work


WORK = None  # set in main()
TOTAL_KIB = [None]  # what total_memory() reports to work.memory_budget
HOST_CPUS = [1.0]  # what host_cpu_capacity() reports to predict()


def float_text(value):
    """The exact decimal text of a float result, so the Rust side parses the same double."""
    return None if value is None else repr(float(value))


def make_db(rows):
    """A real sqlite tasks table with the columns the admission and prediction SQL reads."""
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.execute("CREATE TABLE tasks (id TEXT PRIMARY KEY, weight REAL, requested REAL, started REAL, ended REAL, "
                 "quiet_until REAL, cpu_time REAL, rss_peak INTEGER, status INTEGER, sig_full TEXT, sig_head TEXT, "
                 "sig_prog TEXT, sig_tool TEXT)")
    for row in rows:
        values = [row.get(col) for col in COLS]
        conn.execute(f"INSERT INTO tasks ({', '.join(COLS)}) VALUES ({', '.join('?' * len(COLS))})", values)
    return conn


def make_settings(**overrides):
    values = dict(capacity=None, load_factor=1.5, load_wait=1800.0, log_limit=268435456, keep_days=14.0,
                  pruned=None, idle_min=10.0, memory_limit="warning", backfill_wait=300.0, small_weight=1.0,
                  cpu_budget=None, memory_budget=None, aging=1.0, job_qos="utility", quiet_drain=60.0,
                  quiet_max=180.0)
    values.update(overrides)
    return WORK.Settings(**values)


def pick_weight(rng, cpus):
    return rng.choice([weight for weight in WEIGHTS if weight <= cpus])


def running_row(rng, idx, cpus, light=False):
    """A running job. `light` jobs take little CPU and have memory history, so the cores and capacity gates pass and
    the machine's pressure and the memory reservation decide."""
    if light:
        weight = rng.choice([w for w in (0.5, 1.0, 2.0) if w <= cpus])
        return {
            "id": f"r{idx}", "weight": weight, "pred_cores": round(rng.uniform(0.1, 0.5), 2),
            "pred_rss_p90": rng.choice([None, rng.randint(50_000, 3_000_000)]),
            "pred_duration_p90": rng.choice([None, round(rng.uniform(5, 900), 1)]),
            "rss": rng.choice([None, rng.randint(10_000, 2_000_000)]),
            "started": rng.choice([None, NOW - rng.randint(0, 1000), NOW - rng.randint(0, 1000)]),
            "quiet_until": None,
        }
    weight = pick_weight(rng, cpus)
    exclusive = weight >= cpus and cpus >= 2
    quiet = NOW + rng.randint(-100, 200) if exclusive and rng.random() < 0.6 else None
    return {
        "id": f"r{idx}", "weight": weight,
        "pred_cores": rng.choice([None, round(rng.uniform(0.1, weight), 2)]),
        "pred_rss_p90": rng.choice([None, rng.randint(50_000, 3_000_000)]),
        "pred_duration_p90": rng.choice([None, round(rng.uniform(5, 900), 1)]),
        "rss": rng.choice([None, rng.randint(10_000, 2_000_000)]),
        "started": rng.choice([None, NOW - rng.randint(0, 1000)]),
        "quiet_until": quiet,
    }


def waiting_row(rng, ident, seq, cpus, weight=None):
    weight = pick_weight(rng, cpus) if weight is None else weight
    return {
        "id": ident, "kind": rng.choice(KINDS), "seq": seq,
        "requested": NOW - rng.randint(0, 700), "weight": weight,
        "pred_cores": rng.choice([None, round(rng.uniform(0.1, weight), 2)]),
        "pred_duration": rng.choice([None, round(rng.uniform(5, 900), 1)]),
        "pred_duration_p90": rng.choice([None, round(rng.uniform(5, 1200), 1)]),
        "pred_rss_p90": rng.choice([None, rng.randint(50_000, 3_000_000)]),
    }


def settings_fields(rng, cpus):
    """Settings drawn from the ranges the admission tests exercise; memory_budget None is Auto, 0 is Off."""
    return {
        "cpu_budget": rng.choice([None, 2.0, 3.0, 4.0, 5.0, 6.0, 7.5, 8.0, 10.0, 12.0, 16.0]),
        "capacity": rng.choice([None, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0]),
        "memory_budget": rng.choice([None, 0, rng.randint(600_000, 12_000_000)]),
        "idle_min": rng.choice([5.0, 10.0, 15.0, 20.0]),
        "memory_limit": rng.choice(["warning", "critical", "off"]),
        "load_factor": rng.choice([1.0, 1.5, 2.0]),
        "load_wait": 1800.0,
        "backfill_wait": rng.choice([60.0, 300.0]),
        "small_weight": rng.choice([0.5, 1.0, 2.0]),
        "aging": rng.choice([0.0, 0.5, 1.0]),
        "quiet_drain": rng.choice([30.0, 60.0]),
        "quiet_max": rng.choice([120.0, 180.0]),
    }


def signals_fields(rng):
    if rng.random() < 0.1:
        return None
    return {
        "idle_pct": rng.choice([None, round(rng.uniform(0, 100), 1)]),
        "memory_level": rng.choice([None, "normal", "warning", "critical"]),
        "load1": rng.choice([None, round(rng.uniform(0, 20), 2)]),
        "load_gate": rng.random() < 0.5,
        "memory_used_kib": rng.choice([None, rng.randint(0, 12_000_000)]),
    }


def blocker_case(rng, idx):
    # A third of the cases are machine-pressure cases: CPU room everywhere, light running jobs, signals that look
    # unhealthy, so that idle, load and memory decide. The rest draw from the full ranges.
    pressure_case = rng.random() < 0.35
    cpus = rng.choice([4.0, 8.0, 16.0] if pressure_case else CPUS)
    settings_values = settings_fields(rng, cpus)
    total = rng.choice([None, 1_500_000, 3_000_000, 8_388_608, 16_777_216])
    signals = signals_fields(rng)
    if pressure_case:
        settings_values["cpu_budget"] = 16.0
        settings_values["capacity"] = None
        if signals is not None:
            signals["idle_pct"] = round(rng.uniform(0, 25), 1)
            signals["memory_used_kib"] = rng.randint(400_000, 12_000_000)
            signals["memory_level"] = rng.choice(["normal", "warning", "critical"])
            signals["load1"] = round(rng.uniform(0, 30), 2)
    running = [running_row(rng, i, cpus, light=pressure_case) for i in range(rng.randint(0, 4))]
    candidate_exclusive = cpus >= 2 and rng.random() < 0.2
    waiting = [waiting_row(rng, f"w{j}", j, cpus) for j in range(rng.randint(0, 5))]
    cand_seq = rng.randint(0, len(waiting))
    candidate = waiting_row(rng, "cand", cand_seq, cpus, cpus if candidate_exclusive else None)
    if pressure_case and not candidate_exclusive:
        candidate["weight"] = rng.choice([w for w in (0.5, 1.0, 2.0, 4.0) if w <= cpus])
        candidate["pred_cores"] = rng.choice([None, round(rng.uniform(0.1, candidate["weight"]), 2)])
    if rng.random() < 0.25:
        candidate["pred_cores"] = round(rng.uniform(0.1, 0.5), 2)  # a tiny job
    waiting = [dict(row, seq=row["seq"] + (1 if row["seq"] >= cand_seq and row["id"] != "cand" else 0))
               for row in waiting]
    line = sorted(waiting + [candidate], key=lambda row: row["seq"])
    # Rows the database holds with a quiet window: the running windows, plus a few other jobs' (some ended).
    windows = [{"id": row["id"], "quiet_until": row["quiet_until"], "ended": None}
               for row in running if row["quiet_until"] is not None]
    # Distinct ids: tasks.id is the primary key. The candidate may be among them, so the query's id filter is exercised.
    for ident in rng.sample(["cand", "x0", "x1", "x2"], rng.randint(0, 2)):
        windows.append({"id": ident, "quiet_until": NOW - rng.randint(0, 300),
                        "ended": rng.choice([None, NOW - rng.randint(0, 300)])})
    return {"kind": "blocker", "name": f"blocker-{idx:03d}", "now": NOW, "cpus": cpus,
            "total_memory_kib": total, "settings": settings_values, "signals": signals, "running": running,
            "waiting": line, "candidate": "cand", "windows": windows,
            "expected": blocker_expected(cpus, total, settings_values, signals, running, line, windows)}


def blocker_expected(cpus, total, settings_values, signals, running, line, windows):
    settings = make_settings(**settings_values)
    TOTAL_KIB[0] = total
    memory = WORK.memory_budget(settings)
    sig = None if signals is None else WORK.Signals(
        idle=signals["idle_pct"], memory=signals["memory_level"], load=signals["load1"],
        load_gate=signals["load_gate"], memory_used=signals["memory_used_kib"])
    box = WORK.Machine(cpus, settings.cpu_budget or float(cpus), settings.capacity, settings, sig, memory)
    conn = make_db(windows)
    stub = types.SimpleNamespace(lines=lambda _conn, _row: (running, line))
    cand = next(row for row in line if row["id"] == "cand")
    WORK.time = types.SimpleNamespace(time=lambda: NOW)
    held = WORK.Supervisor.blocker(stub, conn, cand, box)
    return None if held is None else {"cause": held[0], "reason": held[1]}


def order_case(rng, idx):
    aging = rng.choice([0.0, 0.5, 1.0])
    cpus = 8.0
    rows = []
    for j in range(rng.randint(3, 8)):
        row = waiting_row(rng, f"o{j}", j, cpus)
        row["pred_duration"] = rng.choice(DURATIONS)
        row["requested"] = float(rng.choice(REQUESTS))
        rows.append(row)
    ordered = sorted(rows, key=lambda row: WORK.waiting_order(row, aging))
    return {"kind": "order", "name": f"order-{idx:03d}", "now": NOW, "aging": aging, "waiting": rows,
            "expected_order": [row["id"] for row in ordered]}


def head_case(rng, idx):
    cpus = rng.choice(CPUS)
    cpu_budget = rng.choice([None, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 16.0])
    running = [running_row(rng, i, cpus) for i in range(rng.randint(0, 4))]
    head_need = float(pick_weight(rng, cpus))
    settings = make_settings(cpu_budget=cpu_budget)
    box = WORK.Machine(cpus, settings.cpu_budget or float(cpus), None, settings, None, None)
    value = WORK.head_start_estimate(head_need, running, box, NOW)
    return {"kind": "head_start", "name": f"head-{idx:03d}", "now": NOW, "cpus": cpus, "cpu_budget": cpu_budget,
            "head_need": head_need, "running": running, "expected": float_text(value)}


def predict_case(rng, idx):
    cpus = rng.choice([2.0, 4.0, 8.0])
    signed = {level: rng.choice([None] + POOLS[level]) for level in LEVELS}
    # Every fourth case asks for one level only, so each signature level is the one that answers now and then.
    only = (None, "tool", "prog", "head")[idx % 4]
    if only is not None:
        signed = {level: (POOLS[level][0] if level == only else None) for level in LEVELS}
    weight = rng.choice([0.5, 1.0, 2.0, 3.0, 4.0, cpus])
    runs = []
    for i in range(rng.randint(0, 12)):
        ended = 1000.0 + 37 * i  # distinct ends: the SQL orders by ended, so ties would make the newest-N ambiguous
        duration = rng.randint(0, 300)
        sig = {level: rng.choice([None] + POOLS[level]) for level in LEVELS}
        if only is not None and rng.random() < 0.8:
            sig[only] = POOLS[only][0]
        runs.append({
            "id": f"p{i}", "weight": rng.choice([1.0, 2.0, 4.0, 8.0]),
            "sig_full": sig["full"], "sig_head": sig["head"], "sig_prog": sig["prog"], "sig_tool": sig["tool"],
            "started": ended - duration, "ended": ended,
            "cpu_time": None if rng.random() < 0.1 else round(rng.uniform(0, 6) * max(duration, 1), 2),
            "rss_peak": rng.choice([None, rng.randint(10_000, 900_000)]),
            "status": rng.choice([0, 0, 0, 1, 126, 127, None]),
        })
    HOST_CPUS[0] = cpus
    conn = make_db(runs)
    result = WORK.predict(conn, signed, weight)
    return {"kind": "predict", "name": f"predict-{idx:03d}", "cpus": cpus, "weight": weight, "signed": signed,
            "runs": runs,
            "expected": {"cores": float_text(result.cores), "rss_kib": result.rss_kib,
                         "rss_p90_kib": result.rss_p90_kib, "duration": float_text(result.duration),
                         "duration_p90": float_text(result.duration_p90), "level": result.level,
                         "runs": result.runs, "basis": result.basis}}


def build_cases():
    rng = random.Random(SEED)
    cases = [blocker_case(rng, i + 1) for i in range(BLOCKER_CASES)]
    cases += [order_case(rng, i + 1) for i in range(ORDER_CASES)]
    cases += [head_case(rng, i + 1) for i in range(HEAD_CASES)]
    cases += [predict_case(rng, i + 1) for i in range(PREDICT_CASES)]
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True, help="directory to write <n>.json into")
    parser.add_argument("--work-dir", default=DEFAULT_WORK_DIR, help="planner scripts directory holding work.py")
    args = parser.parse_args()
    global WORK
    WORK = load_work(args.work_dir)
    # Inject the host: total memory and CPU count come from these, never from the machine.
    WORK.total_memory = lambda *a, **k: TOTAL_KIB[0]
    WORK.host_cpu_capacity = lambda: HOST_CPUS[0]
    os.makedirs(args.out, exist_ok=True)
    cases = build_cases()
    for n, case in enumerate(cases, start=1):
        with open(os.path.join(args.out, f"{n:03d}.json"), "w", encoding="utf-8") as handle:
            json.dump(case, handle, indent=1, sort_keys=True)
            handle.write("\n")
    causes = {}
    for case in cases:
        if case["kind"] == "blocker":
            key = case["expected"]["cause"] if case["expected"] else "start"
            causes[key] = causes.get(key, 0) + 1
    print(f"wrote {len(cases)} cases to {args.out}")
    print("blocker outcomes:", json.dumps(causes, sort_keys=True))


if __name__ == "__main__":
    main()
