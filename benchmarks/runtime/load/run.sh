#!/usr/bin/env bash
# run.sh — Drive the full benchmark suite against one or all frameworks.
#
# Usage:
#   ./load/run.sh <framework> <base_url> [--vus N] [--duration Xs] [--out-dir DIR]
#   ./load/run.sh all                   [--vus N] [--duration Xs] [--out-dir DIR]
#
# Examples:
#   ./load/run.sh autumn  http://localhost:8001
#   ./load/run.sh all     --vus 50 --duration 60s
#   ./load/run.sh spring-boot http://localhost:8002 --out-dir results/
#
# Requires k6 to be installed: https://k6.io/docs/get-started/installation/
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
OUT_DIR="$SCRIPT_DIR/../results/$(date +%Y%m%d_%H%M%S)"
BENCH_TOKEN="${BENCH_TOKEN:-benchmark-token-abc123}"

declare -A FRAMEWORK_URLS=(
  [autumn]="http://localhost:8001"
  [spring-boot]="http://localhost:8002"
  [rails]="http://localhost:8003"
  [django]="http://localhost:8004"
  [phoenix]="http://localhost:8005"
  [loco]="http://localhost:8006"
)

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
FRAMEWORK="${1:-}"
BASE_URL="${2:-}"

shift 2 2>/dev/null || true

while [[ $# -gt 0 ]]; do
  case "$1" in
    --vus)      VUS="$2";     shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --out-dir)  OUT_DIR="$2"; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$FRAMEWORK" ]]; then
  echo "Usage: $0 <framework|all> [base_url] [--vus N] [--duration Xs] [--out-dir DIR]" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------
run_suite() {
  local fw="$1"
  local url="$2"
  local out="$3"

  mkdir -p "$out"

  echo ""
  echo "========================================"
  echo "Framework: $fw"
  echo "Base URL:  $url"
  echo "VUs:       $VUS"
  echo "Duration:  $DURATION"
  echo "Output:    $out"
  echo "========================================"

  # Percentiles reported in the exported summary. p(50)/p(95)/p(99) are the
  # columns RESULTS.md asks for; k6's default trend stats omit p(99).
  local k6_common_args=(
    --vus "$VUS"
    --duration "$DURATION"
    --summary-trend-stats "avg,min,med,p(50),p(95),p(99),max"
  )

  for script in json-crud html-page validation-fail auth-protected; do
    echo ""
    echo "--- Running $script ---"
    # --summary-export must be per-scenario: a single shared summary.json is
    # overwritten by each successive k6 run, leaving only the last scenario's
    # numbers behind.
    BASE_URL="$url" \
    BENCH_TOKEN="$BENCH_TOKEN" \
    VUS="$VUS" \
    DURATION="$DURATION" \
    k6 run \
      "${k6_common_args[@]}" \
      --summary-export "$out/${script}-summary.json" \
      --out "json=$out/${script}.json" \
      "$K6_DIR/${script}.js" \
      2>&1 | tee "$out/${script}.log" || true
  done

  echo ""
  echo "Results written to: $out"
}

# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------
if [[ "$FRAMEWORK" == "all" ]]; then
  for fw in "${!FRAMEWORK_URLS[@]}"; do
    url="${FRAMEWORK_URLS[$fw]}"
    fw_out="$OUT_DIR/$fw"
    run_suite "$fw" "$url" "$fw_out"
  done
else
  if [[ -z "$BASE_URL" ]]; then
    BASE_URL="${FRAMEWORK_URLS[$FRAMEWORK]:-}"
    if [[ -z "$BASE_URL" ]]; then
      echo "No base URL provided and '$FRAMEWORK' is not a known framework." >&2
      echo "Known frameworks: ${!FRAMEWORK_URLS[*]}" >&2
      exit 1
    fi
  fi
  run_suite "$FRAMEWORK" "$BASE_URL" "$OUT_DIR/$FRAMEWORK"
fi

echo ""
echo "Benchmark run complete."
