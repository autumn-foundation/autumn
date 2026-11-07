#!/usr/bin/env bash
# resources.sh — Collect the supplementary (non-k6) metrics for each framework.
#
# Usage:
#   ./load/resources.sh [framework...] [--out FILE] [--idle-wait SECONDS]
#
# With no framework, measures all six. Run it from benchmarks/runtime after
# `docker compose build`, with the db service up. For each framework it records:
#
#   image_mb        `docker image inspect` size of the running container's image
#   cold_start_s    `docker compose up -d <fw>` on a removed container until the
#                   first 200 from the health route (includes migrations and
#                   pool setup)
#   stop_s          `docker compose stop <fw>`: SIGTERM until the process exits
#                   (graceful drain; Docker sends SIGKILL after 10 s)
#   restart_s       `docker compose start <fw>` until the first 200
#   warm_restart_s  stop_s + restart_s
#   idle_rss_mb     `docker stats` memory after --idle-wait seconds (default 30)
#
# Health routes: /health everywhere except Loco, which serves its built-in
# /_health.
#
# Memory under load is sampled separately by load/sample-mem.sh while k6 runs.
# Output is a CSV (default results/resources_<timestamp>.csv) and a table.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUNTIME_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$RUNTIME_DIR"

IDLE_WAIT=30
OUT="$RUNTIME_DIR/results/resources_$(date +%Y%m%d_%H%M%S).csv"
TARGETS=()

declare -A PORTS=(
  [autumn]=8001 [spring-boot]=8002 [rails]=8003
  [django]=8004 [phoenix]=8005 [loco]=8006
)
declare -A HEALTH=(
  [autumn]=/health [spring-boot]=/health [rails]=/health
  [django]=/health [phoenix]=/health [loco]=/_health
)
ORDER=(autumn spring-boot rails django phoenix loco)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)       OUT="$2";       shift 2 ;;
    --idle-wait) IDLE_WAIT="$2"; shift 2 ;;
    -h|--help)   sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)
      [[ -n "${PORTS[$1]:-}" ]] || { echo "Unknown framework: $1" >&2; exit 1; }
      TARGETS+=("$1"); shift ;;
  esac
done
[[ ${#TARGETS[@]} -eq 0 ]] && TARGETS=("${ORDER[@]}")

now() { date +%s.%N; }

# Seconds from $1 (epoch) until http://localhost:$2$3 answers 200; 120 s timeout.
wait_healthy() {
  local start="$1" port="$2" path="$3"
  local deadline
  deadline=$(echo "$start + 120" | bc)
  while :; do
    if curl -sf -o /dev/null --max-time 1 "http://localhost:$port$path"; then
      echo "$(now) - $start" | bc
      return 0
    fi
    if (( $(echo "$(now) > $deadline" | bc) )); then
      echo "timeout"
      return 1
    fi
    sleep 0.05
  done
}

# Memory of a compose service's container in MiB, from `docker stats`.
mem_mb() {
  local id
  id=$(docker compose ps -q "$1")
  docker stats --no-stream --format '{{.MemUsage}}' "$id" | awk '{print $1}' | python3 -c '
import re, sys
v = sys.stdin.read().strip()
m = re.match(r"([0-9.]+)\s*([KMG]i?B|B)", v)
n, u = float(m.group(1)), m.group(2)
scale = {"B": 1 / 2**20, "KiB": 1 / 1024, "KB": 1 / 1024, "MiB": 1, "MB": 1, "GiB": 1024, "GB": 1024}[u]
print(round(n * scale, 1))'
}

mkdir -p "$(dirname "$OUT")"
# Two decimals, or the word "timeout" unchanged.
fmt() { [[ "$1" == timeout ]] && echo timeout || printf '%.2f' "$1"; }

echo "framework,image_mb,cold_start_s,stop_s,restart_s,warm_restart_s,idle_rss_mb" > "$OUT"

for fw in "${TARGETS[@]}"; do
  port="${PORTS[$fw]}"
  path="${HEALTH[$fw]}"
  echo "==> $fw"

  docker compose rm -sf "$fw" >/dev/null 2>&1
  start=$(now)
  docker compose up -d --no-build "$fw" >/dev/null 2>&1
  cold=$(wait_healthy "$start" "$port" "$path") || true

  image=$(docker inspect -f '{{.Image}}' "$(docker compose ps -q "$fw")")
  image_mb=$(docker image inspect -f '{{.Size}}' "$image" | awk '{printf "%.1f", $1 / 1048576}')

  start=$(now)
  docker compose stop "$fw" >/dev/null 2>&1
  stop=$(echo "$(now) - $start" | bc)
  start=$(now)
  docker compose start "$fw" >/dev/null 2>&1
  restart=$(wait_healthy "$start" "$port" "$path") || true
  if [[ "$restart" == timeout ]]; then warm=timeout; else warm=$(echo "$stop + $restart" | bc); fi

  sleep "$IDLE_WAIT"
  idle=$(mem_mb "$fw")

  echo "$fw,$image_mb,$(fmt "$cold"),$(fmt "$stop"),$(fmt "$restart"),$(fmt "$warm"),$idle" | tee -a "$OUT"
done

echo ""
python3 -c 'import csv,sys;r=list(csv.reader(open(sys.argv[1])));w=[max(len(x[i]) for x in r) for i in range(len(r[0]))];[print("  ".join(c.ljust(n) for c,n in zip(x,w))) for x in r]' "$OUT"
echo ""
echo "Written: $OUT"
