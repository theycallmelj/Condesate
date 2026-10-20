"""Grades real condesate trajectories (written by `agentevals_run`, the Rust
binary) with the real, pip-installed `agentevals` library.

https://github.com/langchain-ai/agentevals
"""

import json
import sys

from agentevals.trajectory.match import create_trajectory_match_evaluator

with open("trajectory.json") as f:
    data = json.load(f)

reference = data["reference"]
scenarios = [("actual_good", data["actual_good"]), ("actual_regressed", data["actual_regressed"])]
modes = ["strict", "unordered", "subset", "superset"]

results = {}
for name, actual in scenarios:
    print(f"--- {name} ---")
    results[name] = {}
    for mode in modes:
        evaluator = create_trajectory_match_evaluator(trajectory_match_mode=mode)
        result = evaluator(outputs=actual, reference_outputs=reference)
        results[name][mode] = result["score"]
        print(f"  [{'PASS' if result['score'] else 'FAIL'}] {mode}")
    print()

ok = True
if not results["actual_good"]["strict"]:
    print("FAIL: the real, correct condesate run did not strictly match the reference plan.")
    ok = False
if results["actual_regressed"]["strict"]:
    print("FAIL: the regressed run (dropped the reporting step) incorrectly strict-matched the reference.")
    ok = False
if not results["actual_regressed"]["subset"]:
    print("FAIL: the regressed run's smaller trajectory should still be a subset of the reference plan.")
    ok = False

if ok:
    print(
        "agentevals confirms: the correct run matches the reference plan; the "
        "regressed run (which silently drops the reporting step) is caught by "
        "strict matching, and is correctly identified as a subset (fewer tool "
        "calls, same order) of the intended plan rather than an unrelated failure."
    )
    sys.exit(0)
else:
    sys.exit(1)
