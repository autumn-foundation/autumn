#!/usr/bin/env bash
# Run autumn's own security posture gate over its example apps (issue #1624).
#
# The gate autumn scaffolds into every app is only worth shipping if autumn's
# own apps run it. This is that run, and it is the same pipeline the scaffolded
# `.github/workflows/posture-gate.yml` executes, minus the GitHub-specific
# halves (comment posting, acknowledgment harvesting):
#
#   1. Build each example's posture manifest with `autumn routes audit`. That
#      is the head side, and it also enforces #1604's default-deny rule — an
#      unclassified route fails here before any diffing happens.
#   2. Diff it against the baseline as of the BASE REVISION — not the working
#      tree. A change that widens an example and refreshes its committed
#      manifest in the same commit would otherwise compare identical sides and
#      pass, which is precisely the change the gate exists to catch. Widening
#      fails this script; narrowing and neutral changes are printed and pass.
#
# Drift between the committed manifest and the freshly built one is reported,
# not failed: unlike an app repository, this one *is* the framework, so the
# framework-owned routes in every example's manifest move whenever the
# framework does. What must never move silently is the security *posture*,
# which is exactly what the diff — and not the byte comparison — measures.
# Refresh the committed manifests with `--update` when they drift.
#
# Escape hatch, mirroring the product's: export AUTUMN_POSTURE_ACK with the
# digest the diff prints to acknowledge a deliberate widening. It is recorded
# in this script's output, and (in CI) in the job log.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-posture-gate.sh
#     ./scripts/check-posture-gate.sh --update      # refresh the baselines
#     AUTUMN_POSTURE_ACK=<digest> ./scripts/check-posture-gate.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Examples carrying a committed posture baseline. Keep this list and the
# `security-posture.json` files next to those examples in step.
EXAMPLES=("hello")

UPDATE=0
case "${1:-}" in
  "") ;;
  --update) UPDATE=1 ;;
  *)
    echo "usage: $0 [--update]" >&2
    exit 2
    ;;
esac

ACK="${AUTUMN_POSTURE_ACK:-}"

# The revision whose committed manifests count as the accepted posture. In CI
# this is the pull request's base branch; locally it defaults to the upstream
# trunk, and falls back to HEAD (with a notice) when neither is fetched — a
# working-tree comparison is still useful when you have not touched the
# baseline, and the CI run is the one that must be strict.
POSTURE_BASE_REF="${POSTURE_BASE_REF:-}"
if [ -z "$POSTURE_BASE_REF" ]; then
  if [ -n "${GITHUB_BASE_REF:-}" ]; then
    POSTURE_BASE_REF="origin/${GITHUB_BASE_REF}"
  else
    POSTURE_BASE_REF="origin/trunk-dev"
  fi
fi

die() {
  echo "error: $*" >&2
  exit 1
}

ok() {
  echo "ok:    $*"
}

note() {
  echo "note:  $*"
}

command -v cargo >/dev/null || die "cargo is required"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Building the autumn CLI…"
# `--locked`: a Cargo.lock that does not match the manifests should fail the
# gate, not resolve something else (same rule as scripts/check-sbom.sh).
cargo build -q --locked -p autumn-cli --bin autumn
# Honour a shared target directory, which maintainers running these scripts
# locally commonly set (again mirroring check-sbom.sh).
CLI="${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-$root/target}}/debug/autumn"
[ -x "$CLI" ] || die "the autumn CLI did not build at $CLI"

failed=0

for example in "${EXAMPLES[@]}"; do
  baseline="examples/$example/security-posture.json"
  fresh="$work/$example.json"

  echo
  echo "── $example ────────────────────────────────────────────────"

  # Also the #1604 default-deny gate: an unclassified route fails right here.
  "$CLI" routes audit -p "$example" --manifest "$fresh" >/dev/null \
    || die "$example: \`autumn routes audit\` failed — see the output above"
  ok "$example: every route carries a proven classification"

  if [ "$UPDATE" = "1" ]; then
    cp "$fresh" "$baseline"
    ok "$example: refreshed $baseline"
    continue
  fi

  if [ ! -f "$baseline" ]; then
    die "$example: no committed baseline at $baseline — run $0 --update"
  fi

  # Resolve the base side from the base revision, so refreshing the committed
  # manifest in the same change cannot make both sides agree.
  accepted="$work/$example.base.json"
  bootstrap=0
  if ! git rev-parse --verify --quiet "$POSTURE_BASE_REF^{commit}" >/dev/null; then
    # No base revision in this checkout (a shallow clone, or a local clone with
    # no remote). Comparing against the working tree still catches a widening
    # you have not also written into the baseline; the CI run, which does have
    # the base ref, is the strict one.
    cp "$baseline" "$accepted"
    note "$example: $POSTURE_BASE_REF is not in this checkout — comparing against"
    note "$example: the working tree's $baseline (CI resolves the base ref)"
  elif git cat-file -e "$POSTURE_BASE_REF:$baseline" 2>/dev/null; then
    git show "$POSTURE_BASE_REF:$baseline" > "$accepted"
    note "$example: comparing against $POSTURE_BASE_REF:$baseline"
  else
    # The baseline is new on this branch: there is no previously accepted
    # posture to compare against, which is a bootstrap, not a clean bill.
    bootstrap=1
    accepted="$work/$example.absent.json"
    note "$example: no baseline on $POSTURE_BASE_REF yet — bootstrapping"
  fi

  args=(routes posture diff --base "$accepted" --head "$fresh" --format text)
  if [ "$bootstrap" = "1" ]; then
    args+=(--allow-missing-base)
  fi
  if [ -n "$ACK" ]; then
    args+=(--ack "$ACK")
    note "$example: acknowledgment supplied via AUTUMN_POSTURE_ACK"
  fi

  set +e
  "$CLI" "${args[@]}"
  status=$?
  set -e

  case "$status" in
    0) ok "$example: no unacknowledged widening" ;;
    1)
      echo "error: $example: this change widens the example's security surface." >&2
      echo "       If that is intended, re-run with AUTUMN_POSTURE_ACK=<digest printed above>" >&2
      echo "       and refresh the baseline with $0 --update in the same change." >&2
      failed=1
      ;;
    *)
      echo "error: $example: \`autumn routes posture diff\` could not run (exit $status)" >&2
      failed=1
      # Keep going: one broken example must not hide the verdict for the rest.
      continue
      ;;
  esac

  # The committed baseline has to describe *this* commit's posture, or the next
  # change diffs against a fiction. Exit 0 above does not establish that: it
  # also covers a narrowing, a neutral change and an acknowledged widening, any
  # of which can leave a committed baseline security-stale — and a later change
  # widening back to that stale baseline would then read as no change at all.
  #
  # Compared by digest rather than by bytes, the way the scaffolded workflow
  # compares it: the digest excludes handler names and source locations, so a
  # moved line number stays a note and a real drift is an error.
  if [ -f "$baseline" ] && ! diff -q "$baseline" "$fresh" >/dev/null; then
    committed_digest=$("$CLI" routes posture digest --manifest "$baseline" 2>/dev/null || echo unreadable)
    fresh_digest=$("$CLI" routes posture digest --manifest "$fresh" 2>/dev/null || echo unbuildable)
    if [ "$committed_digest" = "$fresh_digest" ]; then
      note "$example: $baseline no longer matches a fresh build, but the posture it"
      note "$example: describes is unchanged (line numbers, a renamed handler, or a"
      note "$example: framework-owned route that moved). Refresh with $0 --update."
    else
      echo "error: $example: $baseline no longer describes this commit's posture" >&2
      echo "       committed $committed_digest, built $fresh_digest" >&2
      echo "       Refresh it with $0 --update, or the next change will be" >&2
      echo "       compared against a posture this commit does not have." >&2
      failed=1
    fi
  fi
done

echo
[ "$failed" = "0" ] || die "the security posture gate failed for at least one example"
ok "security posture gate passed for: ${EXAMPLES[*]}"
