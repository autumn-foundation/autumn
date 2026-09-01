#!/usr/bin/env bash
# Migration version gate.
#
# Framework migrations ship into every Autumn app's database alongside the
# app's own migrations and any plugin's. Diesel keys applied migrations by
# their 14-digit version, and `autumn_migration_checksums` makes that version
# a PRIMARY KEY -- so two migrations that share a version are not two
# migrations. One of them silently never runs.
#
# Hand-written day-granularity versions (`YYYYMMDD000000`) are how that
# happens in practice: every author who types a date and pads six zeros lands
# on the same number as everyone else who touched the tree that day. The
# damage is already visible in this repo --
#
#   autumn/migrations/20260513000000_create_job_queue     (framework)
#   examples/reddit-clone/.../20260513000001_create_autumn_jobs   (+1 by hand)
#   autumn/migrations/20260702000000_create_job_tracking  (framework)
#   examples/reddit-clone/.../20260702000001_create_tags          (+1 by hand)
#
# -- those `...0001` suffixes are collisions someone dodged manually, and
# `00000000000000` is duplicated across the framework, the starters, the
# benchmark app and eight examples.
#
# The tooling already solves this: `autumn generate migration`, `autumn
# generate model/scaffold`, and `autumn schema diff --write-migration` all
# mint a full `YYYYMMDDHHMMSS` from the wall clock via
# `autumn_cli::generate::timestamp_now`. Two authors collide only if they
# generate in the same second. This gate exists because the tooling is
# bypassed by hand-created directories, not because it is missing.
#
# Enforced over every Autumn-owned `migrations/` tree:
#
#   1. Shape.      `<14 digits>_<snake_case suffix>`.
#   2. Real time.  The digits must parse as a UTC timestamp -- month 01-12,
#                  a day that exists in that month, hour 00-23, minute and
#                  second 00-59. This catches `20260530300000` (hour 30),
#                  which is in the tree today.
#   3. Precision.  The `HHMMSS` component may not be `000000`. This is the
#                  rule that actually stops the collisions.
#   4. Uniqueness. No two Autumn-owned migrations may share a version.
#
# Existing violations are grandfathered through
# `scripts/migration-version-baseline.txt`. They are NOT renamed: these
# migrations have already been applied to real databases and are recorded by
# version in `__diesel_schema_migrations` and `autumn_migration_checksums`.
# Renaming one makes the framework consider it unapplied and re-run it. The
# baseline is a record of debt, not a place to add new entries -- a new
# migration must satisfy the rules instead.
#
# Exit status 0 = all checks passed.
# Exit status 1 = one or more failures found.
#
# Run locally:
#   ./scripts/check-migration-versions.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

BASELINE="scripts/migration-version-baseline.txt"

# `migrations/` trees that are not Autumn migrations:
#   docs/migrations          -- release upgrade guides (markdown), not DDL.
#   benchmarks/runtime/{phoenix,django,loco}
#                            -- competing frameworks in the benchmark suite,
#                               each with its own naming convention.
is_excluded_tree() {
  case "$1" in
    ./docs/migrations) return 0 ;;
    ./benchmarks/runtime/phoenix/*) return 0 ;;
    ./benchmarks/runtime/django/*) return 0 ;;
    ./benchmarks/runtime/loco/*) return 0 ;;
    *) return 1 ;;
  esac
}

failures=0

ok()   { echo "ok:    $*"; }
fail() { echo "FAIL:  $*" >&2; failures=$((failures + 1)); }

# ---------------------------------------------------------------------------
# 1. Discover Autumn-owned migration directories
# ---------------------------------------------------------------------------
echo "==> Discovering Autumn-owned migration trees"

trees=()
while IFS= read -r tree; do
  is_excluded_tree "$tree" && continue
  trees+=("$tree")
done < <(find . -type d -name migrations \
  -not -path './target/*' -not -path './.git/*' | sort)

if [[ ${#trees[@]} -eq 0 ]]; then
  fail "no migration trees found — has the layout changed?"
  exit 1
fi

# entries[] holds "<tree>/<dirname>" for every versioned migration directory.
entries=()
for tree in "${trees[@]}"; do
  while IFS= read -r dir; do
    [[ -z "$dir" ]] && continue
    entries+=("${tree#./}/$(basename "$dir")")
  done < <(find "$tree" -mindepth 1 -maxdepth 1 -type d | sort)
done

echo "     ${#trees[@]} trees, ${#entries[@]} migration directories"
echo ""

# ---------------------------------------------------------------------------
# 2. Load the grandfather baseline
# ---------------------------------------------------------------------------
echo "==> Loading baseline: $BASELINE"

if [[ ! -f "$BASELINE" ]]; then
  fail "$BASELINE not found — it records the pre-existing violations this gate grandfathers"
  exit 1
fi

# Held as a newline-delimited string rather than an associative array: this
# script is run locally by contributors, and `declare -A` is a syntax error on
# the bash 3.2 that ships with macOS. No other script in scripts/ requires
# bash 4, and a cryptic parse error is a poor way to deliver a style rule.
legacy_list=""
legacy_count=0
while IFS= read -r line || [[ -n "$line" ]]; do
  line="${line%%#*}"                 # strip comments
  line="${line#"${line%%[![:space:]]*}"}"  # trim leading whitespace
  line="${line%"${line##*[![:space:]]}"}"  # trim trailing whitespace
  [[ -z "$line" ]] && continue
  legacy_list="${legacy_list}${line}"$'\n'
  legacy_count=$((legacy_count + 1))
done < "$BASELINE"

# Exact whole-line membership test.
is_legacy() {
  case $'\n'"$legacy_list" in
    *$'\n'"$1"$'\n'*) return 0 ;;
    *) return 1 ;;
  esac
}

echo "     $legacy_count grandfathered entries"
echo ""

# ---------------------------------------------------------------------------
# 3. Validate each migration directory
# ---------------------------------------------------------------------------
echo "==> Checking migration versions"

# Echoes a human-readable reason if $1 is not a valid, precise version;
# stays silent when the name is fine.
version_problem() {
  local name="$1" version time year month day hour minute second max_day

  if [[ ! "$name" =~ ^[0-9]{14}_[a-z0-9]+(_[a-z0-9]+)*$ ]]; then
    echo "not <14 digits>_<snake_case suffix>"
    return
  fi

  version="${name:0:14}"
  year="${version:0:4}"
  month="${version:4:2}"
  day="${version:6:2}"
  time="${version:8:6}"
  hour="${version:8:2}"
  minute="${version:10:2}"
  second="${version:12:2}"

  # Strip leading zeros for arithmetic without octal interpretation.
  year=$((10#$year)); month=$((10#$month)); day=$((10#$day))
  hour=$((10#$hour)); minute=$((10#$minute)); second=$((10#$second))

  if (( year < 1970 )); then
    echo "year $year is before the Unix epoch"
    return
  fi
  if (( month < 1 || month > 12 )); then
    echo "month $month is not 01-12"
    return
  fi
  case $month in
    1|3|5|7|8|10|12) max_day=31 ;;
    4|6|9|11)        max_day=30 ;;
    2) if (( (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 )); then
         max_day=29
       else
         max_day=28
       fi ;;
    *) max_day=31 ;;
  esac
  if (( day < 1 || day > max_day )); then
    echo "day $day does not exist in month $month of $year"
    return
  fi
  if (( hour > 23 )); then
    echo "hour $hour is not 00-23"
    return
  fi
  if (( minute > 59 )); then
    echo "minute $minute is not 00-59"
    return
  fi
  if (( second > 59 )); then
    echo "second $second is not 00-59"
    return
  fi
  if [[ "$time" == "000000" ]]; then
    echo "day-granularity version (time component is 000000)"
    return
  fi
}

new_violations=()
for entry in "${entries[@]}"; do
  name="$(basename "$entry")"
  problem="$(version_problem "$name")"
  [[ -z "$problem" ]] && continue

  is_legacy "$entry" && continue
  new_violations+=("$entry — $problem")
done

if [[ ${#new_violations[@]} -eq 0 ]]; then
  ok "every migration outside the baseline has a precise, valid version"
else
  for violation in "${new_violations[@]}"; do
    fail "$violation"
  done
fi
echo ""

# ---------------------------------------------------------------------------
# 4. Version uniqueness across every Autumn-owned tree
# ---------------------------------------------------------------------------
echo "==> Checking version uniqueness across trees"

# "<version> <entry>" pairs, sorted so equal versions land adjacent — plain
# arrays only, for the bash 3.2 reason above.
pairs=()
for entry in "${entries[@]}"; do
  name="$(basename "$entry")"
  version="${name:0:14}"
  [[ ! "$version" =~ ^[0-9]{14}$ ]] && continue
  pairs+=("$version $entry")
done

collisions=()
prev_version=""
prev_entry=""
while IFS=' ' read -r version entry; do
  [[ -z "$version" ]] && continue
  if [[ "$version" == "$prev_version" ]]; then
    # A collision is grandfathered only when BOTH sides are in the baseline;
    # a new migration colliding with a legacy one is exactly the failure this
    # gate exists to catch.
    if is_legacy "$entry" && is_legacy "$prev_entry"; then
      : # both pre-existing — already-shipped debt, recorded in the baseline
    else
      collisions+=("$version — $prev_entry and $entry")
    fi
  fi
  prev_version="$version"
  prev_entry="$entry"
done < <(printf '%s\n' "${pairs[@]}" | sort)

if [[ ${#collisions[@]} -eq 0 ]]; then
  ok "no version is claimed by two migrations outside the baseline"
else
  for collision in "${collisions[@]}"; do
    fail "duplicate version $collision"
  done
fi
echo ""

# ---------------------------------------------------------------------------
# 5. Baseline hygiene — a stale entry hides a regression
# ---------------------------------------------------------------------------
echo "==> Checking baseline is current"

stale=()
while IFS= read -r entry; do
  [[ -z "$entry" ]] && continue
  [[ -d "$entry" ]] || stale+=("$entry")
done <<< "$legacy_list"

if [[ ${#stale[@]} -eq 0 ]]; then
  ok "every baseline entry still exists"
else
  for entry in "${stale[@]}"; do
    fail "baseline lists $entry, which no longer exists — remove the line"
  done
fi
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
if [[ $failures -eq 0 ]]; then
  echo "All migration version checks passed."
  exit 0
fi

cat >&2 <<'GUIDANCE'

------------------------------------------------------------------------
Migration versions must be precise, not day-granularity.

A version like 20260831000000 collides with every other migration authored
that day — in this app, in a plugin, or in the framework itself. Diesel and
autumn_migration_checksums both key on the version, so the loser of a
collision silently never runs.

Generate migrations instead of hand-creating the directory:

    autumn generate migration <Name> [field:type ...]
    autumn generate model <Name> [field:type ...]
    autumn schema diff --write-migration --name <name>

Each mints a full YYYYMMDDHHMMSS from the wall clock, so two authors collide
only if they generate in the same second.

Already created the directory by hand? Rename it to a precise timestamp:

    date -u +%Y%m%d%H%M%S

Do NOT add the new migration to scripts/migration-version-baseline.txt.
That file grandfathers migrations already applied to real databases, which
cannot be renamed without the framework re-running them.
------------------------------------------------------------------------
GUIDANCE

echo "$failures check(s) failed." >&2
exit 1
