#!/usr/bin/env python3
"""Aggregate k6 summary exports from a benchmark run directory.

Usage:
    python3 load/aggregate.py <results-dir>

Reads every ``<framework>/<scenario>-summary.json`` (single run) or
``run-NN/<framework>/<scenario>-summary.json`` (repeated runs) written by
``load/run.sh`` and writes two CSV files into ``<results-dir>``:

- ``aggregate.csv``: one row per run, framework and scenario.
- ``aggregate-summary.csv``: the median of each metric across runs, per
  framework and scenario, plus the p95 spread.

Latency columns are in milliseconds and cover every request the scenario
sent (``http_req_duration``). p50 and p99 are present only when k6 ran with
``--summary-trend-stats`` listing ``med`` and ``p(99)``, as run.sh does.
Standard library only.
"""

from __future__ import annotations

import csv
import json
import statistics
import sys
from pathlib import Path

ROW_FIELDS = [
    "run",
    "framework",
    "scenario",
    "requests",
    "req_s",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "avg_ms",
    "checks_pass_pct",
    "check_fails",
    "http_failed_pct",
]

MEDIAN_FIELDS = [
    "req_s",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "avg_ms",
    "checks_pass_pct",
    "http_failed_pct",
]


def _round(value: float | None, digits: int) -> float | None:
    return None if value is None else round(value, digits)


def parse_summary(path: Path, results_dir: Path) -> dict:
    """Turn one k6 ``--summary-export`` file into an aggregate row."""
    metrics = json.loads(path.read_text(encoding="utf-8"))["metrics"]
    parts = path.parent.relative_to(results_dir).parts
    if len(parts) == 1:
        run, framework = "run-01", parts[0]
    else:
        run, framework = parts[0], parts[1]

    duration = metrics.get("http_req_duration", {})
    reqs = metrics.get("http_reqs", {})
    checks = metrics.get("checks", {})
    failed = metrics.get("http_req_failed", {})

    return {
        "run": run,
        "framework": framework,
        "scenario": path.name.removesuffix("-summary.json"),
        "requests": reqs.get("count"),
        "req_s": _round(reqs.get("rate"), 1),
        "p50_ms": _round(duration.get("med"), 2),
        "p95_ms": _round(duration.get("p(95)"), 2),
        "p99_ms": _round(duration.get("p(99)"), 2),
        "avg_ms": _round(duration.get("avg"), 2),
        "checks_pass_pct": _round(
            None if "value" not in checks else checks["value"] * 100, 2
        ),
        "check_fails": checks.get("fails"),
        "http_failed_pct": _round(
            None if "value" not in failed else failed["value"] * 100, 2
        ),
    }


def summarize(rows: list[dict]) -> list[dict]:
    """Median of each metric across runs, per framework and scenario."""
    groups: dict[tuple[str, str], list[dict]] = {}
    for row in rows:
        groups.setdefault((row["framework"], row["scenario"]), []).append(row)

    out = []
    for (framework, scenario), items in sorted(groups.items()):
        summary: dict = {"framework": framework, "scenario": scenario, "runs": len(items)}
        for field in MEDIAN_FIELDS:
            values = [r[field] for r in items if r[field] is not None]
            summary[f"median_{field}"] = (
                round(statistics.median(values), 2) if values else None
            )
        summary["total_check_fails"] = sum(r["check_fails"] or 0 for r in items)
        p95 = [r["p95_ms"] for r in items if r["p95_ms"] is not None]
        summary["min_p95_ms"] = min(p95) if p95 else None
        summary["max_p95_ms"] = max(p95) if p95 else None
        out.append(summary)
    return out


def write_csv(path: Path, rows: list[dict]) -> None:
    with path.open("w", newline="", encoding="utf-8") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)


def print_table(rows: list[dict]) -> None:
    headers = list(rows[0].keys())
    cells = [[("" if r[h] is None else str(r[h])) for h in headers] for r in rows]
    widths = [max(len(h), *(len(c[i]) for c in cells)) for i, h in enumerate(headers)]
    print("  ".join(h.ljust(w) for h, w in zip(headers, widths)))
    for c in cells:
        print("  ".join(v.ljust(w) for v, w in zip(c, widths)))


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    results_dir = Path(argv[1]).resolve()
    files = sorted(results_dir.rglob("*-summary.json"))
    if not files:
        print(f"no *-summary.json files under {results_dir}", file=sys.stderr)
        return 1

    rows = sorted(
        (parse_summary(f, results_dir) for f in files),
        key=lambda r: (r["run"], r["framework"], r["scenario"]),
    )
    summary = summarize(rows)

    write_csv(results_dir / "aggregate.csv", rows)
    write_csv(results_dir / "aggregate-summary.csv", summary)

    print("\nAggregate summary (medians across runs):")
    print_table(summary)
    print(f"\nAggregate CSV:         {results_dir / 'aggregate.csv'}")
    print(f"Aggregate summary CSV: {results_dir / 'aggregate-summary.csv'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
