#!/usr/bin/env python3
"""Run the Bolt profile of one or more workloads in docs/perf/bolt/workloads.toml.

Usage (from the repository root, through the wrapper):

  scripts/bolt-profile.sh autumn-web/csrf_verify [more ...]
  scripts/bolt-profile.sh --all        every non-exempt workload
  scripts/bolt-profile.sh --missing    only workloads with no committed profile
  scripts/bolt-profile.sh --list       print the manifest

Each workload is built in release mode with its features, run under
`valgrind --tool=callgrind` and `valgrind --tool=dhat`, and summarized by
`scripts/perf/bolt_summarize.py` into `docs/perf/bolt/<package>/<bench>/`.

Environment:
  BOLT_PG_URL   a TLS-free Postgres URL for DB-backed workloads
                (default postgres://postgres@127.0.0.1:5432/postgres).
  BOLT_RAW_DIR  where raw valgrind output goes (default: a temp directory).
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "docs/perf/bolt/workloads.toml"
PROFILES = ROOT / "docs/perf/bolt"


def load() -> list[dict]:
    return tomllib.loads(MANIFEST.read_text())["workload"]


def key(w: dict) -> str:
    return f"{w['package']}/{w['bench']}"


def profile_dir(w: dict) -> Path:
    return PROFILES / w["package"] / w["bench"]


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    print("+", shlex.join(cmd), flush=True)
    return subprocess.run(cmd, check=True, **kw)


def build(w: dict) -> tuple[Path, str, Path]:
    """Build the bench and return (executable, build command, source path)."""
    cmd = ["cargo", "build", "--release", "-p", w["package"], "--bench", w["bench"]]
    if w.get("features"):
        cmd += ["--features", ",".join(w["features"])]
    shown = shlex.join(cmd)
    out = run(cmd + ["--message-format=json"], cwd=ROOT, capture_output=True, text=True).stdout
    exe = src = None
    for line in out.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") == "compiler-artifact" and msg["target"]["name"] == w["bench"] and "bench" in msg["target"]["kind"]:
            exe = msg.get("executable")
            src = msg["target"]["src_path"]
    if not exe:
        sys.exit(f"bolt: no executable for {key(w)}")
    return Path(exe), shown, Path(src)


def bench_args(w: dict, n: int | None) -> list[str]:
    return [a.replace("{n}", str(n)) for a in w.get("args", [])]


def bench_env(w: dict) -> dict[str, str]:
    env = dict(os.environ)
    pg = os.environ.get("BOLT_PG_URL", "postgres://postgres@127.0.0.1:5432/postgres")
    for item in w.get("env", []):
        k, _, v = item.partition("=")
        env[k] = v.replace("{BOLT_PG_URL}", pg)
    return env


def valgrind(tool: str, outfile: Path, exe: Path, args: list[str], env: dict) -> str:
    flag = "--callgrind-out-file" if tool == "callgrind" else "--dhat-out-file"
    extra = ["--branch-sim=no", "--cache-sim=no"] if tool == "callgrind" else []
    cmd = ["valgrind", f"--tool={tool}", *extra, f"{flag}={outfile}", str(exe), *args]
    run(cmd, cwd=exe.parent, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return " ".join([shlex.join(["valgrind", f"--tool={tool}", *extra, f"{flag}={outfile.name}"]), '"$BIN"', shlex.join(args)]).strip()


PGSS_SNAPSHOT = """
COPY (
  SELECT s.calls, s.rows, s.shared_blks_hit + s.shared_blks_read AS buffers,
         s.temp_blks_written AS temp, regexp_replace(s.query, '\\s+', ' ', 'g') AS query
  FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid
  WHERE d.datname = current_database()
    AND s.query !~* '^\\s*(SET|BEGIN|COMMIT|ROLLBACK|SAVEPOINT|RELEASE|SELECT pg_stat_statements_reset)'
) TO STDOUT WITH CSV
"""


def pgss(url: str, sql: str) -> str:
    return subprocess.run(["psql", url, "-XAtq", "-c", sql], check=True, capture_output=True, text=True).stdout


def ledger_run(w: dict, exe: Path, env: dict, n: int) -> dict[str, list[float]]:
    """Run the bench natively between a pg_stat_statements reset and a snapshot."""
    import csv
    import io

    url = env["DATABASE_URL"]
    pgss(url, "CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
    pgss(url, "SELECT pg_stat_statements_reset()")
    run([str(exe), *bench_args(w, n)], cwd=exe.parent, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    out: dict[str, list[float]] = {}
    for calls, rows, buffers, temp, query in csv.reader(io.StringIO(pgss(url, PGSS_SNAPSHOT))):
        acc = out.setdefault(query, [0, 0, 0, 0])
        for i, v in enumerate((calls, rows, buffers, temp)):
            acc[i] += float(v)
    return out


def write_ledger(w: dict, base: dict, runs: dict, n: int, cmd: str) -> None:
    rows = []
    for q in set(base) | set(runs):
        b, r = base.get(q, [0, 0, 0, 0]), runs.get(q, [0, 0, 0, 0])
        d = [(r[i] - b[i]) / n for i in range(4)]
        if d[0] > 0:
            rows.append((d, q))
    rows.sort(key=lambda x: -x[0][2])
    total = [sum(d[i] for d, _ in rows) for i in range(4)]
    lines = [
        f"# Ledger profile: {key(w)}",
        "# unit: per iteration (N-iteration run minus zero-iteration run, divided by N)",
        "# tool: pg_stat_statements, reset before each run; buffers = shared hit + read",
        f"# command: {cmd}",
        "",
        f"statements per iteration: {total[0]:.2f}",
        f"rows per iteration: {total[1]:.2f}",
        f"shared buffers per iteration: {total[2]:.2f}",
        f"temp blocks written per iteration: {total[3]:.2f}",
        "",
        f"{'calls':>8} {'rows':>8} {'buffers':>9}  statement",
        "",
    ]
    for d, q in rows:
        lines.append(f"{d[0]:>8.2f} {d[1]:>8.2f} {d[2]:>9.2f}  {q[:200]}")
    (profile_dir(w) / "ledger.txt").write_text("\n".join(lines) + "\n")


def tool_version(cmd: list[str]) -> str:
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip().splitlines()[0]


def profile(w: dict, raw_root: Path) -> None:
    exe, build_cmd, src = build(w)
    raw = raw_root / w["package"] / w["bench"]
    raw.mkdir(parents=True, exist_ok=True)
    env = bench_env(w)
    mode = w["mode"]
    if mode == "marginal":
        cn, dn = w["callgrind_n"], w["dhat_n"]
        valgrind("callgrind", raw / "callgrind-base.out", exe, bench_args(w, 0), env)
        cg = valgrind("callgrind", raw / "callgrind-run.out", exe, bench_args(w, cn), env)
        valgrind("dhat", raw / "dhat-base.json", exe, bench_args(w, 0), env)
        dh = valgrind("dhat", raw / "dhat-run.json", exe, bench_args(w, dn), env)
        cg = cg + "  # and again with {n}=0, subtracted"
        dh = dh + "  # and again with {n}=0, subtracted"
    else:
        cn = dn = 0
        cg = valgrind("callgrind", raw / "callgrind-run.out", exe, bench_args(w, None), env)
        dh = valgrind("dhat", raw / "dhat-run.json", exe, bench_args(w, None), env)
    commit = subprocess.run(["git", "rev-parse", "--short=12", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    run(
        [
            sys.executable,
            str(ROOT / "scripts/perf/bolt_summarize.py"),
            "--out", str(profile_dir(w)),
            "--raw", str(raw),
            "--mode", mode,
            "--package", w["package"],
            "--bench", w["bench"],
            "--callgrind-n", str(cn),
            "--dhat-n", str(dn),
            "--build", build_cmd,
            "--callgrind-cmd", cg,
            "--dhat-cmd", dh,
            "--source-sha256", hashlib.sha256(src.read_bytes()).hexdigest(),
            "--rustc", tool_version(["rustc", "-V"]),
            "--valgrind", tool_version(["valgrind", "--version"]),
            "--commit", commit,
            "--date", dt.date.today().isoformat(),
        ]
    )
    if w.get("ledger") == "postgres":
        n = w["dhat_n"]
        base = ledger_run(w, exe, env, 0)
        runs = ledger_run(w, exe, env, n)
        write_ledger(w, base, runs, n, '"$BIN" ' + shlex.join(bench_args(w, n)) + "  # and again with {n}=0, subtracted")


def main(argv: list[str]) -> None:
    workloads = load()
    if not argv or argv == ["--list"]:
        for w in workloads:
            state = "exempt" if w.get("exempt") else ("ok" if (profile_dir(w) / "profile.toml").exists() else "missing")
            print(f"{key(w):40} {state}")
        return
    if argv == ["--all"]:
        chosen = [w for w in workloads if not w.get("exempt")]
    elif argv == ["--missing"]:
        chosen = [w for w in workloads if not w.get("exempt") and not (profile_dir(w) / "profile.toml").exists()]
    else:
        by_key = {key(w): w for w in workloads}
        unknown = [a for a in argv if a not in by_key]
        if unknown:
            sys.exit(f"bolt: not in {MANIFEST.relative_to(ROOT)}: {', '.join(unknown)}")
        chosen = [by_key[a] for a in argv]
        exempt = [key(w) for w in chosen if w.get("exempt")]
        if exempt:
            sys.exit(f"bolt: exempt in the manifest: {', '.join(exempt)}")
    raw_root = Path(os.environ.get("BOLT_RAW_DIR") or tempfile.mkdtemp(prefix="bolt-"))
    failed = []
    for w in chosen:
        print(f"== bolt {key(w)} ({w['mode']})", flush=True)
        try:
            profile(w, raw_root)
        except subprocess.CalledProcessError as e:
            print(f"bolt: {key(w)} failed: {e}", file=sys.stderr)
            failed.append(key(w))
    print(f"raw valgrind output: {raw_root}")
    if failed:
        sys.exit(f"bolt: failed: {', '.join(failed)}")


if __name__ == "__main__":
    main(sys.argv[1:])
