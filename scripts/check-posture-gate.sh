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
#   2. Diff it against the manifest committed next to the example. Widening —
#      a route becoming reachable by more callers, a guard removed or loosened —
#      fails this script. Narrowing and neutral changes are printed and pass.
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

  args=(routes posture diff --base "$baseline" --head "$fresh" --format text)
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

  # Only worth saying when the posture itself came back clean: then the bytes
  # moved for a reason the diff deliberately ignores (a line number, a renamed
  # handler, a framework-owned route that shifted), and the baseline is merely
  # stale rather than wrong.
  if [ "$status" = "0" ] && ! diff -q "$baseline" "$fresh" >/dev/null; then
    note "$example: $baseline no longer matches a fresh build, but the posture it"
    note "$example: describes is unchanged (line numbers, a renamed handler, or a"
    note "$example: framework-owned route that moved). Refresh with $0 --update."
  fi
done

echo
[ "$failed" = "0" ] || die "the security posture gate failed for at least one example"
ok "security posture gate passed for: ${EXAMPLES[*]}"
