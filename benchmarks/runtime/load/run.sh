#!/usr/bin/env bash
# run.sh — Drive the k6 benchmark suite against one or all frameworks.
#
# Usage:
#   ./load/run.sh <framework> [base_url] [options]
#   ./load/run.sh all                    [options]
#
# Options:
#   --vus N            virtual users                    (default 20)
#   --duration Xs      duration per script              (default 30s)
#   --repeat N         repeat the whole matrix N times  (default 1)
#   --out-dir DIR      results directory  (default results/k6_<timestamp>)
#   --scripts a,b      comma-separated k6 scripts
#                      (default json-crud,html-page,validation-fail,auth-protected)
#   --k6 MODE          docker | local                   (default docker)
#   --k6-image IMAGE   k6 image for --k6 docker         (default grafana/k6:2.3.0)
#
# Examples:
#   ./load/run.sh autumn
#   ./load/run.sh all --vus 50 --duration 60s --repeat 3
#   ./load/run.sh spring-boot http://localhost:8002 --k6 local
#
# --k6 docker runs k6 in a container on the host network, so the URLs below
# work unchanged. --k6 local needs k6 on PATH:
# https://grafana.com/docs/k6/latest/set-up/install-k6/
#
# Output: one <script>-summary.json per framework and script, then
# aggregate.csv (one row per run/framework/scenario) and aggregate-summary.csv
# (medians across runs), written by load/aggregate.py. This is the bash
# counterpart of load/run-docker.ps1 and writes the same layout.
#
# Port map (docker compose comparable track):
#   autumn      http://localhost:8001
#   spring-boot http://localhost:8002
#   rails       http://localhost:8003
#   django      http://localhost:8004
#   phoenix     http://localhost:8005
#   loco        http://localhost:8006

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
K6_DIR="$SCRIPT_DIR/k6"

# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------
VUS=20
DURATION=30s
REPEAT=1
OUT_DIR="$SCRIPT_DIR/../results/k6_$(date +%Y%m%d_%H%M%S)"
SCRIPTS="json-crud,html-page,validation-fail,auth-protected"
K6_MODE=docker
K6_IMAGE="grafana/k6:2.3.0"
BENCH_TOKEN="${BENCH_TOKEN:-benchmark-token-abc123}"
TREND_STATS="avg,min,med,max,p(90),p(95),p(99)"

FRAMEWORK_ORDER=(autumn spring-boot rails django phoenix loco)
declare -A FRAMEWORK_URLS=(
  [autumn]="http://localhost:8001"
  [spring-boot]="http://localhost:8002"
  [rails]="http://localhost:8003"
  [django]="http://localhost:8004"
  [phoenix]="http://localhost:8005"
  [loco]="http://localhost:8006"
)

usage() {
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
FRAMEWORK="${1:-}"
[[ -z "$FRAMEWORK" || "$FRAMEWORK" == -* ]] && usage
shift

BASE_URL=""
if [[ $# -gt 0 && "$1" != --* ]]; then
  BASE_URL="$1"
  shift
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --vus)      VUS="$2";       shift 2 ;;
    --duration) DURATION="$2";  shift 2 ;;
    --repeat)   REPEAT="$2";    shift 2 ;;
    --out-dir)  OUT_DIR="$2";   shift 2 ;;
    --scripts)  SCRIPTS="$2";   shift 2 ;;
    --k6)       K6_MODE="$2";   shift 2 ;;
    --k6-image) K6_IMAGE="$2";  shift 2 ;;
    -h|--help)  usage ;;
    *) echo "Unknown option: $1" >&2; usage ;;
  esac
done

if ! [[ "$REPEAT" =~ ^[0-9]+$ ]] || (( REPEAT < 1 || REPEAT > 100 )); then
  echo "--repeat must be between 1 and 100" >&2
  exit 1
fi

case "$K6_MODE" in
  docker) command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; } ;;
  local)  command -v k6 >/dev/null     || { echo "k6 not found on PATH" >&2; exit 1; } ;;
  *) echo "--k6 must be 'docker' or 'local'" >&2; exit 1 ;;
esac

if [[ "$FRAMEWORK" == "all" ]]; then
  [[ -n "$BASE_URL" ]] && { echo "'all' takes no base_url" >&2; exit 1; }
  TARGETS=("${FRAMEWORK_ORDER[@]}")
else
  if [[ -z "$BASE_URL" ]]; then
    BASE_URL="${FRAMEWORK_URLS[$FRAMEWORK]:-}"
    if [[ -z "$BASE_URL" ]]; then
      echo "No base URL provided and '$FRAMEWORK' is not a known framework." >&2
      echo "Known frameworks: ${FRAMEWORK_ORDER[*]}" >&2
      exit 1
    fi
  fi
  TARGETS=("$FRAMEWORK")
fi

IFS=',' read -r -a SCRIPT_LIST <<< "$SCRIPTS"
for script in "${SCRIPT_LIST[@]}"; do
  [[ -f "$K6_DIR/$script.js" ]] || { echo "Missing k6 script: $K6_DIR/$script.js" >&2; exit 1; }
done

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
run_k6() {
  local url="$1" out="$2" script="$3"
  if [[ "$K6_MODE" == "docker" ]]; then
    docker run --rm --network host \
      --user "$(id -u):$(id -g)" \
      -e BASE_URL="$url" \
      -e BENCH_TOKEN="$BENCH_TOKEN" \
      -e VUS="$VUS" \
      -e DURATION="$DURATION" \
      -v "$K6_DIR:/scripts:ro" \
      -v "$out:/results" \
      "$K6_IMAGE" run --quiet \
      --summary-trend-stats "$TREND_STATS" \
      --summary-export "/results/$script-summary.json" \
      "/scripts/$script.js"
  else
    BASE_URL="$url" BENCH_TOKEN="$BENCH_TOKEN" VUS="$VUS" DURATION="$DURATION" \
      k6 run --quiet \
      --summary-trend-stats "$TREND_STATS" \
      --summary-export "$out/$script-summary.json" \
      "$K6_DIR/$script.js"
  fi
}

# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------
had_failure=0

for (( run = 1; run <= REPEAT; run++ )); do
  run_name="$(printf 'run-%02d' "$run")"
  if (( REPEAT == 1 )); then run_root="$OUT_DIR"; else run_root="$OUT_DIR/$run_name"; fi

  for fw in "${TARGETS[@]}"; do
    url="${BASE_URL:-${FRAMEWORK_URLS[$fw]}}"
    fw_out="$run_root/$fw"
    mkdir -p "$fw_out"

    echo ""
    echo "========================================"
    echo "Run:       $run_name of $REPEAT"
    echo "Framework: $fw"
    echo "Base URL:  $url"
    echo "VUs:       $VUS"
    echo "Duration:  $DURATION"
    echo "Output:    $fw_out"
    echo "========================================"

    for script in "${SCRIPT_LIST[@]}"; do
      echo ""
      echo "--- Running $script ---"
      if ! run_k6 "$url" "$fw_out" "$script" 2>&1 | tee "$fw_out/$script.log"; then
        had_failure=1
        echo "k6 failed for $fw/$script (threshold breach or error)"
      fi
    done
  done
done

python3 "$SCRIPT_DIR/aggregate.py" "$OUT_DIR"

echo ""
echo "Results written to: $OUT_DIR"
exit "$had_failure"
