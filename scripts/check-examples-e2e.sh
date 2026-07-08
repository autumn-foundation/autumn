#!/usr/bin/env bash
# Example fleet e2e gate — issue #1192.
#
# For every catalog-supported example: builds it, boots the real binary
# against isolated ephemeral database(s), runs its Chromium system-test
# smoke (`tests/system/smoke.rs`), and tears down. Aggregates results
# across the whole fleet — does NOT abort on the first failing example —
# prints a per-example pass/fail/skip summary, and exits non-zero if any
# example fails to build or its smoke fails.
#
# Requires Chromium and Docker (for testcontainers-provisioned Postgres). If
# either is unavailable, the affected example smokes are SKIPPED with a
# visible notice — a skip is never reported as a pass.
#
# The full docker-compose multi-container topology (nginx + multiple web
# replicas + real Postgres streaming replication) for bookmarks-distributed
# and bookmarks-sharded is explicitly OUT OF SCOPE here (see issue #1192);
# this gate proves each example's single-process feature (dual-pool
# primary/replica routing, shard fan-out) boots and serves — the compose
# integration is a follow-up.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-examples-e2e.sh
#
# Override the Chromium binary the same way the smoke tests do:
#
#     AUTUMN_CHROMIUM=/path/to/chrome ./scripts/check-examples-e2e.sh
#
# In an environment that provisions Chromium and Docker specifically for
# this gate (e.g. the publish-gate CI job), set REQUIRE_FULL_COVERAGE=1 so
# any SKIP — which there would mean the provisioning silently broke, not
# "no coverage available" — fails the gate instead of letting "all checks
# passed" mask degraded coverage:
#
#     REQUIRE_FULL_COVERAGE=1 ./scripts/check-examples-e2e.sh

set -uo pipefail
# Deliberately NOT `-e`: a failing example must not abort the run — every
# example gets a chance to build and smoke so the summary is complete.

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

CATALOG="EXAMPLES.md"

die() {
  echo "error: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# 0. Discover supported examples from the catalog (same marker format
#    `scripts/check-examples.sh` reads — shared via scripts/lib/catalog.sh so
#    the marker-format regex lives in exactly one place).
# ---------------------------------------------------------------------------
[[ -f "$CATALOG" ]] || die "catalog file '$CATALOG' not found — run scripts/check-examples.sh first"

source "$root/scripts/lib/catalog.sh"

mapfile -t examples < <(catalog_names_by_tier "$CATALOG" "supported" | sort)
[[ ${#examples[@]} -gt 0 ]] || die "no supported examples found in $CATALOG"

echo "==> Discovered ${#examples[@]} supported example(s): ${examples[*]}"
echo ""

# ---------------------------------------------------------------------------
# 1. Environment capability probes — determine what can actually be
#    exercised in this environment, so unavailable capability is a visible
#    skip rather than a silent pass or a hard failure.
# ---------------------------------------------------------------------------
chromium_available=1
if [[ -z "${AUTUMN_CHROMIUM:-}" ]] \
  && ! command -v chromium >/dev/null 2>&1 \
  && ! command -v chromium-browser >/dev/null 2>&1 \
  && ! command -v google-chrome >/dev/null 2>&1 \
  && ! command -v google-chrome-stable >/dev/null 2>&1 \
  && [[ -z "${PLAYWRIGHT_BROWSERS_PATH:-}" || ! -d "${PLAYWRIGHT_BROWSERS_PATH:-/nonexistent}" ]]; then
  chromium_available=0
fi

docker_available=1
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  docker_available=0
fi

if [[ "$chromium_available" -eq 0 ]]; then
  echo "SKIP NOTICE: no Chromium binary found (checked AUTUMN_CHROMIUM, PATH, PLAYWRIGHT_BROWSERS_PATH)." >&2
  echo "             every example's Chromium smoke below will be SKIPPED, not passed." >&2
  echo "" >&2
fi
if [[ "$docker_available" -eq 0 ]]; then
  echo "SKIP NOTICE: Docker is unavailable (\`docker info\` failed)." >&2
  echo "             every DB-backed example's smoke below will be SKIPPED, not passed." >&2
  echo "" >&2
fi

echo "SCOPE NOTICE: the docker-compose multi-container topology (nginx + multiple" >&2
echo "              web replicas + real Postgres streaming replication) for" >&2
echo "              bookmarks-distributed and bookmarks-sharded is out of scope for" >&2
echo "              this gate — see issue #1192. Only each example's single-process" >&2
echo "              feature (dual-pool routing / shard fan-out) is smoke-tested here;" >&2
echo "              the full compose integration is a follow-up gate." >&2
echo "" >&2

# Ephemeral Postgres instances each example's smoke provisions (0 = none).
# hello has no database; bookmarks-distributed needs a primary + a
# replica-stand-in; bookmarks-sharded needs a control + two shards. Every
# other supported example needs exactly one.
db_requirement() {
  case "$1" in
    hello) echo 0 ;;
    bookmarks-distributed) echo 2 ;;
    bookmarks-sharded) echo 3 ;;
    *) echo 1 ;;
  esac
}

# ---------------------------------------------------------------------------
# 2. Build + smoke each example, aggregating results — a failure never stops
#    the remaining examples from getting their own turn.
# ---------------------------------------------------------------------------
result_names=()
result_status=() # PASS | FAIL | SKIP
result_detail=()
failures=0

for example in "${examples[@]}"; do
  echo "==> $example: building"
  if ! cargo build -p "$example" --quiet; then
    result_names+=("$example")
    result_status+=("FAIL")
    result_detail+=("build failed")
    failures=$((failures + 1))
    echo "FAIL:  $example — build failed"
    echo ""
    continue
  fi

  needs_db="$(db_requirement "$example")"

  if [[ "$chromium_available" -eq 0 ]]; then
    result_names+=("$example")
    result_status+=("SKIP")
    result_detail+=("no Chromium available")
    echo "SKIP:  $example — no Chromium available"
    echo ""
    continue
  fi

  if [[ "$needs_db" -gt 0 && "$docker_available" -eq 0 ]]; then
    result_names+=("$example")
    result_status+=("SKIP")
    result_detail+=("needs $needs_db Postgres testcontainer(s); Docker unavailable")
    echo "SKIP:  $example — needs Docker for testcontainers, unavailable"
    echo ""
    continue
  fi

  echo "==> $example: running Chromium smoke"
  if cargo test -p "$example" --features system-tests --test smoke -- --include-ignored --test-threads=1; then
    result_names+=("$example")
    result_status+=("PASS")
    result_detail+=("")
    echo "PASS:  $example"
  else
    result_names+=("$example")
    result_status+=("FAIL")
    result_detail+=("smoke test failed")
    failures=$((failures + 1))
    echo "FAIL:  $example — smoke test failed"
  fi
  echo ""
done

# ---------------------------------------------------------------------------
# 3. Summary — printed even when everything skipped, so skipped never reads
#    as green.
# ---------------------------------------------------------------------------
echo "==> Example e2e fleet summary"
echo ""
printf "%-24s %-6s %s\n" "EXAMPLE" "RESULT" "DETAIL"
for i in "${!result_names[@]}"; do
  printf "%-24s %-6s %s\n" "${result_names[$i]}" "${result_status[$i]}" "${result_detail[$i]}"
done
echo ""

pass_count=0
skip_count=0
fail_count=0
for status in "${result_status[@]}"; do
  case "$status" in
    PASS) pass_count=$((pass_count + 1)) ;;
    SKIP) skip_count=$((skip_count + 1)) ;;
    FAIL) fail_count=$((fail_count + 1)) ;;
    *) die "internal error: unrecognized result status '$status' — this is a bug in the harness" ;;
  esac
done
echo "  $pass_count passed, $fail_count failed, $skip_count skipped (of ${#examples[@]} supported examples)"
echo ""

if [[ "$skip_count" -gt 0 ]]; then
  echo "NOTE: $skip_count example(s) skipped — a skip is not a pass; rerun with Chromium/Docker available to get real coverage." >&2
  echo "" >&2
fi

if [[ "$failures" -gt 0 ]]; then
  die "$failures example(s) failed — see per-example output above."
fi

# In an environment that provisions Chromium and Docker for exactly this
# purpose (the publish-gate CI job), a skip means that provisioning silently
# broke — not "no coverage available here" — so it must fail the release
# gate rather than let "all checks passed" mask degraded coverage. Local/dev
# runs without Docker or Chromium leave REQUIRE_FULL_COVERAGE unset and stay
# lenient, matching AC6 ("visibly skip, don't silently pass").
if [[ "${REQUIRE_FULL_COVERAGE:-0}" == "1" && "$skip_count" -gt 0 ]]; then
  die "$skip_count example(s) skipped in an environment that requires full coverage (REQUIRE_FULL_COVERAGE=1) — see SKIP NOTICE(s) above."
fi

echo "Example e2e fleet gate: all checks passed."
