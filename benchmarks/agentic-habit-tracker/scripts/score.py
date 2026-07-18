#!/usr/bin/env python3
"""Deterministic scorer for the habit-tracker benchmark.

Reads a run's metrics.json and prints three SEPARATE axis scores
(speed / correctness / maintainability) plus a summary table. It never collapses
them into a single number.

Deterministic: the output is a pure function of metrics.json. There are NO
wall-clock calls in this script — the elapsed time is read from the metrics
file, not measured here.

Usage:
    python3 score.py runs/<run-id>/metrics.json
    python3 score.py runs/<run-id>            # dir containing metrics.json
"""

import json
import os
import sys

# Normalization reference points (documented, fixed -> deterministic).
SPEED_FAST_SECONDS = 120.0     # <= this scores 100 on the time sub-axis
SPEED_SLOW_SECONDS = 3600.0    # >= this scores 0 on the time sub-axis
SPEED_FEW_CALLS = 20.0         # <= this scores 100 on the tool-call sub-axis
SPEED_MANY_CALLS = 300.0       # >= this scores 0 on the tool-call sub-axis

VISIBLE_WEIGHT = 0.40
HIDDEN_WEIGHT = 0.60


def clamp01(x):
    return max(0.0, min(1.0, x))


def linear_score(value, best, worst):
    """Map value to 0..100; `best` -> 100, `worst` -> 0, clamped."""
    if best == worst:
        return 100.0
    return 100.0 * clamp01((worst - value) / (worst - best))


def safe_ratio(num, den):
    if not den:
        return 0.0
    return num / den


def load_metrics(path):
    if os.path.isdir(path):
        path = os.path.join(path, "metrics.json")
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f), path


def score_speed(m):
    secs = float(m.get("wall_clock_seconds", 0) or 0)
    calls = float(m.get("agent_tool_calls", 0) or 0)
    time_score = linear_score(secs, SPEED_FAST_SECONDS, SPEED_SLOW_SECONDS)
    call_score = linear_score(calls, SPEED_FEW_CALLS, SPEED_MANY_CALLS)
    normalized = round((time_score + call_score) / 2.0, 1)
    return {
        "wall_clock_seconds": secs,
        "agent_tool_calls": calls,
        "time_score": round(time_score, 1),
        "tool_call_score": round(call_score, 1),
        "normalized": normalized,
    }


def score_correctness(m):
    vis = safe_ratio(
        m.get("visible_checks_passed") or 0, m.get("visible_checks_total") or 0
    )
    hid = safe_ratio(
        m.get("hidden_checks_passed") or 0, m.get("hidden_checks_total") or 0
    )
    score = round((VISIBLE_WEIGHT * vis + HIDDEN_WEIGHT * hid) * 100.0, 1)
    return {
        "visible_pass_rate": round(vis, 3),
        "hidden_pass_rate": round(hid, 3),
        "score": score,
    }


def score_maintainability(m):
    return {
        "generated_loc": int(m.get("generated_loc", 0) or 0),
        "has_tests": bool(m.get("has_tests", False)),
        "readme_quality": int(m.get("readme_quality", 0) or 0),      # 0-3 manual
        "uses_conventions": int(m.get("uses_conventions", 0) or 0),  # 0-3 manual
    }


def bar(pct, width=20):
    filled = int(round(clamp01(pct / 100.0) * width))
    return "#" * filled + "-" * (width - filled)


def main():
    if len(sys.argv) != 2:
        print("usage: score.py <runs/<run-id>/metrics.json | runs/<run-id>>")
        sys.exit(2)

    m, path = load_metrics(sys.argv[1])

    speed = score_speed(m)
    correctness = score_correctness(m)
    maint = score_maintainability(m)

    fw = m.get("framework", "?")
    pv = m.get("prompt_version", "?")
    phase = m.get("phase", "?")

    print(f"metrics: {path}")
    print(f"framework={fw}  prompt_version={pv}  phase={phase}")
    print()

    print("== SPEED ==")
    print(f"  wall_clock_seconds : {speed['wall_clock_seconds']:.1f}")
    print(f"  agent_tool_calls   : {int(speed['agent_tool_calls'])}")
    print(f"  time_score         : {speed['time_score']:.1f} / 100")
    print(f"  tool_call_score    : {speed['tool_call_score']:.1f} / 100")
    print(f"  SPEED (normalized) : {speed['normalized']:.1f} / 100  [{bar(speed['normalized'])}]")
    print()

    print("== CORRECTNESS ==")
    print(f"  visible_pass_rate  : {correctness['visible_pass_rate']:.3f}"
          f"  ({m.get('visible_checks_passed') or 0}/{m.get('visible_checks_total') or 0})")
    print(f"  hidden_pass_rate   : {correctness['hidden_pass_rate']:.3f}"
          f"  ({m.get('hidden_checks_passed') or 0}/{m.get('hidden_checks_total') or 0})")
    print(f"  weighting          : visible {int(VISIBLE_WEIGHT*100)}% / hidden {int(HIDDEN_WEIGHT*100)}%")
    print(f"  CORRECTNESS        : {correctness['score']:.1f} / 100  [{bar(correctness['score'])}]")
    print()

    print("== MAINTAINABILITY (reported, not collapsed) ==")
    print(f"  generated_loc      : {maint['generated_loc']}")
    print(f"  has_tests          : {maint['has_tests']}")
    print(f"  readme_quality     : {maint['readme_quality']} / 3  (manual)")
    print(f"  uses_conventions   : {maint['uses_conventions']} / 3  (manual)")
    print()

    print("== SUMMARY ==")
    print(f"  {'axis':<16}{'score':>10}")
    print(f"  {'-'*16}{'-'*10:>10}")
    print(f"  {'speed':<16}{speed['normalized']:>10.1f}")
    print(f"  {'correctness':<16}{correctness['score']:>10.1f}")
    print(f"  {'maintainability':<16}{'(see above)':>10}")
    notes = m.get("notes")
    if notes:
        print()
        print(f"notes: {notes}")


if __name__ == "__main__":
    main()
