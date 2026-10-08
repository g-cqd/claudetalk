#!/usr/bin/env python3
"""Generate differential fixtures for `work order|plan` on given tasks (no database).
python3 gen_ordering.py [--out DIR] [--extra] [--check | --dry]
  default: write DIR/<case>.input.json and <case>.expected.json
  --check: regenerate and compare with files in DIR (exit 1 on difference)
  --dry:   write nothing; print exit code and sha256 prefix of each stdout
Each case runs twice in fresh temp dirs and must give identical output."""
import argparse, hashlib, json, os, subprocess, sys, tempfile

WORK = "/Users/gc/Developer/skills/skills/planner/scripts/work.py"
DEFAULT_OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ordering")
J = "--json"
CASES, EXTRA = [], []


def t(task_id, **fields):
    return {"id": task_id, **fields}


def case(name, args, tasks=None, stderr=True, extra=False):
    (EXTRA if extra else CASES).append({"name": name, "args": args, "tasks": tasks, "stderr": stderr})


# ---- order (12)
case("order_fifo_queued_object_form", ["order", J],
     {"tasks": [t("c", queued=3), t("a", queued=1), t("b", queued=2), t("n")]})
case("order_validity_kinds", ["order", J],
     [t("k1", kind="SECURITY"), t("k2", kind=" crash"), t("k3", kind="Data-Loss"), t("k4", kind="safety"),
      t("k5", kind="crashes"), t("k6"), t("k7", kind="correctness"), t("k8", kind="validity"), t("k9", kind="crash")])
case("order_inherit_validity", ["order", J],
     [t("old", kind="performance"), t("refactor", kind="structure"), t("fix", kind="validity", after=["refactor"])])
case("order_inherit_transitive_diamond", ["order", J],
     [t("tail", kind="docs"), t("a"), t("b", after=["a"]), t("c", after="b"), t("fix", kind="validity", after=["c"]),
      t("base"), t("right", after=["base"]), t("left", after=["base"]),
      t("top", kind="security", after=["left", "right"]), t("other")])
case("order_inherit_tie_by_id", ["order", J],
     [t("x", kind="validity", after=["zeta", "alpha", "Mid"]), t("zeta"), t("alpha"), t("Mid"), t("plain")])
case("order_queued_iso_lexicographic", ["order", J],
     [t("sept", queued="2026-9-24"), t("oct", queued="2026-10-01"), t("none"),
      t("iso", queued="2026-09-30T08:00:00")])
case("order_wsjf_ties_inherit", ["order", "--policy", "wsjf", J],
     [t("big", size=8, value=8), t("quick", size=1, value=4), t("tie", size=8, value=8), t("low", size=4, value=1),
      t("prep", size=5, value=0.5), t("hot", size=1, value=40, after=["prep"])])
case("order_critical_path", ["order", "--policy", "critical-path", J],
     [t("short", size=3), t("head", size=1), t("tail", size=5, after=["head"]), t("lone", size=6),
      t("v", kind="security", size=0.5)])
case("order_unicode_ids", ["order", J],
     [t("\u65e5\u672c"), t("\u00e9"), t("Z"), t("a"), t("\U0001f642"),
      t("v", kind="validity", after=["\u65e5\u672c", "\u00e9", "Z", "a", "\U0001f642"])])
case("order_task_specs", ["order", J,
     "--task", "id=b,after=a,title=Fix the parser, then the lexer,kind=validity,queued=7,size=2,value=3.5",
     "--task", "id=sp, kind=validity",
     "--task", "id=dup,kind=x,kind=validity",
     "--task", "id= padded ,files= x + y ,after= a+ ,size= 2 "],
     [t("a")])
case("order_text_full", ["order"],
     [t("plain", title="Tidy docs", kind="docs"), t("pre", title="Refactor"),
      t("fix", title="Crash on empty input", kind="Crash", queued=2, after=["pre"]),
      t("esc", title="x\u001b[2Jy\nz\u202eq\u00a0w, kind=odd"), t("empty", kind="")])
case("order_text_queued_forms", ["order"],
     [t("i", queued=3), t("f", queued=2.5), t("s", queued="7"), t("e", queued="1e1")])

# ---- plan schedule (10)
case("plan_two_agents_basic", ["plan", "--agents", "2", J], [t("a", size=2), t("b"), t("c", after=["a"])])
case("plan_overlap_three_agents", ["plan", "--agents", "3", J],
     [t("a", files="src/app"), t("b", files="src/app/view.swift"), t("c", files="docs")])
case("plan_prefix_boundaries", ["plan", "--agents", "4", J],
     [t("a", files=["src/a"], size=2), t("b", files=["src/ab"]), t("c", files=["src/a/"]),
      t("d", files=["./src/a"]), t("e", files=["src/a/deep/f.rs"]), t("f", files=["/"]),
      t("g", files=["/etc/x"]), t("h", files=["etc"])])
case("plan_empty_file_path", ["plan", "--agents", "3", J],
     [t("a", files=[""]), t("b", files=[""]), t("c")])
case("plan_critical_path_two", ["plan", "--agents", "2", "--policy", "critical-path", J],
     [t("a", size=3), t("b", size=1), t("c", size=1, after=["b"]), t("d", size=4, after=["c"]), t("e", size=2)])
case("plan_wsjf_float_sizes", ["plan", "--agents", "2", "--policy", "wsjf", J],
     [t("a", size=0.1, value=1), t("b", size=0.2, value=1, after=["a"]), t("c", size=0.3, value=5, after=["b"]),
      t("d", size=0.7, value=2), t("e", size=1e-5), t("f", size=1e16)])
case("plan_diamond_four_agents_idle", ["plan", "--agents", "4", J],
     [t("a"), t("b"), t("c"), t("d", after=["a", "b", "c"]), t("e", after=["d"]), t("x", size=2.5, after=["e"])])
case("plan_validity_deps_files", ["plan", "--agents", "3", "--policy", "critical-path", J],
     [t("lib", files=["src/lib"], size=2), t("ui", files=["src/lib/ui"], after=["lib"], kind="crash"),
      t("docs", files=["docs"], size=3), t("api", files="src/api"),
      t("api2", files="src/api/v2", after=["api"])])
case("plan_one_agent_is_order", ["plan", "--agents", "1", J],
     [t("b", after=["a"]), t("a"), t("v", kind="safety")])
case("plan_text_three_agents", ["plan", "--agents", "3"],
     [t("a", size=1.5), t("b"), t("c", after=["b"]), t("d", size=0.3333333333), t("e", size=1234567)])

# ---- assign (6)
case("assign_basic_three_agents", ["plan", "--agents", "3", "--assign", J],
     [t("a", files=["src"]), t("b", files=["src/x"]), t("c", files=["docs"]), t("d", after=["c"])])
case("assign_low_conflict_cost", ["plan", "--agents", "2", "--assign", "--conflict-cost", "0.5", J],
     [t("a", files=["src"]), t("b", files=["src/x"])])
case("assign_zero_cost_distinct", ["plan", "--agents", "3", "--assign", "--conflict-cost", "0", J],
     [t("a"), t("b")])
case("assign_single_agent_int_types", ["plan", "--agents", "1", "--assign", J], [t("a"), t("b", after=["a"])])
case("assign_fractional_cost", ["plan", "--agents", "5", "--assign", "--conflict-cost", "0.1", J],
     [t("a", files=["m"]), t("b", files=["m/b"]), t("c", files=["m/c"]), t("d", files=["m"]), t("e")])
case("assign_text_wsjf", ["plan", "--agents", "3", "--assign", "--policy", "wsjf"],
     [t("a", size=4, value=1), t("b", size=1, value=9), t("c", size=2, value=2, after=["b"]),
      t("d", size=1, value=5, kind="validity", files=["x"]), t("e", files=["x/y"])])

# ---- errors (12)
case("err_unknown_prerequisite", ["order", J], [t("a", after=["ghost"])])
case("err_duplicate_id", ["order", J], [t("a"), t("b"), t("a")])
case("err_after_itself", ["order", J], [t("a", after=["a"])])
case("err_cycle_three", ["order", J],
     [t("a", after=["c"]), t("b", after=["a"]), t("c", after=["b"]), t("free")])
case("err_cycle_downstream_start", ["order", J],
     [t("a", after=["z", "c"]), t("b", after=["c"]), t("c", after=["b"]), t("z", after=["c"]), t("ok")])
case("err_missing_id", ["order", J], [t("a"), {"title": "no id"}])
case("err_size_zero", ["order", J], [t("a", size=0)])
case("err_value_not_number", ["order", J], [t("a", value="lots")])
case("err_queued_mixed", ["order", J], [t("a", queued=1), t("b", queued="2026-09-24")])
case("err_no_tasks", ["order", J], [])
case("err_bad_spec", ["order", "--task", "nonsense"])
case("err_conflict_cost_without_assign", ["plan", "--agents", "2", "--conflict-cost", "1", "--task", "id=a"])

# ---- extra (--extra)
x = dict(extra=True)
case("x_err_scope_with_task", ["order", "--scope", "proj", "--task", "id=a"], **x)
case("x_err_stdin_twice", ["order", "--tasks", "-", "--tasks", "-"], **x)
case("x_err_queued_bool", ["order", J], [t("a", queued=True)], **x)
case("x_err_queued_list", ["order", J], [t("a", queued=[1])], **x)
case("x_err_after_not_list", ["order", J], [t("a", after=5)], **x)
case("x_err_task_not_object", ["order", J], [1], **x)
case("x_err_json_not_list", ["order", J], {"x": 1}, **x)
case("x_err_size_null", ["order", J], [t("a", size=None)], **x)
case("x_err_size_nan_spec", ["order", "--task", "id=a,size=nan"], **x)
case("x_err_id_whitespace", ["order", J], [{"id": "  "}], **x)
case("x_err_repr_quotes", ["order", J], [t("it's", after=['x"y'])], **x)
case("x_err_repr_control_char", ["order", J], [t("a\u0001b", after=["q"])], **x)
case("x_err_agents_zero", ["plan", "--agents", "0", "--task", "id=a"], stderr=False, **x)
case("x_err_policy_unknown", ["order", "--policy", "x", "--task", "id=a"], stderr=False, **x)
case("x_err_missing_file", ["order", "--tasks", "/nonexistent/tasks.json"], stderr=False, **x)
case("x_ok_coerced_scalars", ["order", "--policy", "wsjf", J],
     [{"id": 1}, {"id": None}, {"id": True}, {"id": 2.5, "size": True, "value": "3"},
      {"id": "z", "after": [1, None]}], **x)
case("x_ok_plan_three_cost_tie", ["plan", "--agents", "3", "--assign", "--conflict-cost", "2", J],
     [t("a"), t("b"), t("c")], **x)


def run_once(args, tasks):
    with tempfile.TemporaryDirectory() as work:
        state = os.path.join(work, "state", "agent-work")
        argv = [sys.executable, "-I", "-X", "utf8", WORK, *args]
        if tasks is not None:
            path = os.path.join(work, "tasks.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(tasks, handle)
            argv += ["--tasks", path]
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "AGENT_WORK_HOME": state, "COLUMNS": "80"}
        done = subprocess.run(argv, input=b"", capture_output=True, env=env, cwd=work, timeout=60)
        if os.path.lexists(os.path.dirname(state)):
            raise SystemExit(f"{args}: touched AGENT_WORK_HOME")
        out, err = done.stdout.decode("utf-8"), done.stderr.decode("utf-8")
        if work in out or work in err:
            raise SystemExit(f"{args}: output mentions the temp directory")
        return {"exit_code": done.returncode, "stdout": out, "stderr": err}


def dump(path, obj):
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(obj, handle, indent=2)
        handle.write("\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=DEFAULT_OUT)
    ap.add_argument("--extra", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--dry", action="store_true")
    opts = ap.parse_args()
    cases = CASES + (EXTRA if opts.extra else [])
    assert len({c["name"] for c in cases}) == len(cases)
    if not (opts.check or opts.dry):
        os.makedirs(opts.out, exist_ok=True)
    bad = 0
    for c in cases:
        first, second = run_once(c["args"], c["tasks"]), run_once(c["args"], c["tasks"])
        if first != second:
            print("NONDETERMINISTIC", c["name"])
            bad += 1
            continue
        doc = {"args": c["args"], "tasks": c["tasks"]}
        if not c["stderr"]:
            doc["compare_stderr"] = False
        base = os.path.join(opts.out, c["name"])
        if opts.dry:
            print(c["name"], first["exit_code"], hashlib.sha256(first["stdout"].encode()).hexdigest()[:16])
        elif opts.check:
            with open(base + ".expected.json", encoding="utf-8") as handle:
                if json.load(handle) != first:
                    print("DIFF", c["name"])
                    bad += 1
        else:
            dump(base + ".input.json", doc)
            dump(base + ".expected.json", first)
    print(f"{len(cases)} cases, {bad} problems")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
