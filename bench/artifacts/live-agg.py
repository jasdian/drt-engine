#!/usr/bin/env python3
"""Aggregate a matrix run: pass / routing / avg LLM-calls per condition + control per-task."""
import json, glob, os, re, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.abspath(__file__))
RUNS = os.path.join(ROOT, sys.argv[1] if len(sys.argv) > 1 else "runs-live")
TASKS = os.path.join(ROOT, "..", "tasks")

expected = {}
for tf in glob.glob(os.path.join(TASKS, "*.toml")):
    txt = open(tf).read()
    m = re.search(r'^\s*expected_capability\s*=\s*(\d+)', txt, re.M)
    tid = os.path.splitext(os.path.basename(tf))[0]
    if m:
        expected[tid] = int(m.group(1))

def root_choice(jp):
    for line in open(jp):
        try:
            e = json.loads(line)
        except Exception:
            continue
        if e.get("type") == "branch_decision" and e.get("branch") == "root":
            return e.get("chosen")
    return None

def llm_calls(jp):
    return sum(1 for line in open(jp) if json.loads(line).get("type") == "llm_call")

CONDS = ["last_k", "decayed", "full", "control"]
print(f"=== {os.path.basename(RUNS)}: pass / routing / avg LLM-calls ===")
for c in CONDS:
    p = n = rc = rn = lc = cells = 0
    for gp in glob.glob(os.path.join(RUNS, c, "*", "*", "grade.json")):
        task = gp.split(os.sep)[-3]
        g = json.load(open(gp))
        n += 1; p += 1 if g.get("pass") else 0
        jp = os.path.join(os.path.dirname(gp), "journal.jsonl")
        if os.path.exists(jp):
            lc += llm_calls(jp); cells += 1
            if c != "control":
                e = expected.get(task)
                if e is not None:
                    rn += 1; rc += 1 if root_choice(jp) == e else 0
    if n:
        rt = f"routing {rc}/{rn}" if c != "control" else "routing   —  "
        print(f"  {c:8} pass {p}/{n}  {rt}  avg_llm_calls {lc/cells:.2f}" if cells else f"  {c:8} pass {p}/{n}")

print("\n=== control (fused free-driver) per-task — does bug_triage_search survive now? ===")
per = defaultdict(lambda: [0, 0])
for gp in glob.glob(os.path.join(RUNS, "control", "*", "*", "grade.json")):
    task = gp.split(os.sep)[-3]
    g = json.load(open(gp))
    per[task][1] += 1; per[task][0] += 1 if g.get("pass") else 0
for task in sorted(per):
    p, n = per[task]
    print(f"  control/{task:22} {p}/{n}")
