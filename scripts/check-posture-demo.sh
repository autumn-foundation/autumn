#!/usr/bin/env bash
# The posture gate on reddit-clone, as a golden diff.
#
# `check-posture-gate.sh` proves the examples' posture holds. This script shows
# what the gate says when it does not. It takes reddit-clone's committed
# manifest, makes one `#[secured]` route public in a copy, and diffs
# the copy against the original with `autumn routes posture diff`:
#
#   1. The diff must block (exit 1) and print an `/ack-posture` marker.
#   2. The same diff, given that marker, must pass (exit 0).
#   3. Both reports must match the committed golden files byte for byte.
#
# The golden files are the reports a pull request comment would carry, so a
# change to the diff's wording or rules shows up as a change to them in review.
# `docs/guide/posture-gate.md` embeds the blocked report between the
# `posture-demo` markers, so the docs show real output too.
#
# Nothing here builds reddit-clone: the input is the committed manifest, which
# `check-posture-gate.sh` keeps current. That script's `--update` calls this
# one's, so refreshing the baseline refreshes the goldens with it.
#
# Usage:
#
#     ./scripts/check-posture-demo.sh            # check the goldens
#     ./scripts/check-posture-demo.sh --update   # rewrite the goldens and docs

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

UPDATE=0
case "${1:-}" in
  "") ;;
  --update) UPDATE=1 ;;
  *)
    echo "usage: $0 [--update]" >&2
    exit 2
    ;;
esac

die() {
  echo "error: $*" >&2
  exit 1
}

ok() {
  echo "ok:    $*"
}

baseline="examples/reddit-clone/security-posture.json"
demo="examples/reddit-clone/posture-demo"
blocked_golden="$demo/widened.md"
acked_golden="$demo/acknowledged.md"
doc="docs/guide/posture-gate.md"

# The route the demo un-guards. Deleting a post is the plainest "nobody may do
# this without signing in" route the example has.
METHOD="DELETE"
ROUTE="/r/{sub_slug}/posts/{post_slug}"

command -v cargo >/dev/null || die "cargo is required"
command -v python3 >/dev/null || die "python3 is required"
[ -f "$baseline" ] || die "no committed manifest at $baseline — run ./scripts/check-posture-gate.sh --update"

if [ -z "${AUTUMN_CLI:-}" ]; then
  echo "Building the autumn CLI…"
  cargo build -q --locked -p autumn-cli --bin autumn
  AUTUMN_CLI="${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-$root/target}}/debug/autumn"
fi
[ -x "$AUTUMN_CLI" ] || die "the autumn CLI is not at $AUTUMN_CLI"
# Absolute, because the diffs below run from inside $work.
AUTUMN_CLI="$(cd "$(dirname "$AUTUMN_CLI")" && pwd)/$(basename "$AUTUMN_CLI")"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Run the diff from inside $work with relative names, so no temporary path can
# leak into a report.
cp "$baseline" "$work/security-posture.json"

# The widening: the route keeps its path and handler and loses its guard, as if
# someone replaced `#[secured]` with `#[public]` above `delete_post`.
python3 -I - "$work/security-posture.json" "$work/widened.json" "$METHOD" "$ROUTE" <<'PY'
import json
import sys

src, dst, method, path = sys.argv[1:]
with open(src, encoding="utf-8") as f:
    manifest = json.load(f)
hits = [
    r
    for r in manifest["dimensions"]["routes"]["entries"]
    if r["method"] == method and r["path"] == path
]
if len(hits) != 1:
    sys.exit(f"expected one {method} {path} route in the manifest, found {len(hits)}")
route = hits[0]
if route["classification"] != "gated":
    sys.exit(f"{method} {path} is {route['classification']!r}, not 'gated'; pick another route")
route.update(classification="public", roles=[], scopes=[], policy=False)
with open(dst, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PY

cd "$work"

set +e
"$AUTUMN_CLI" routes posture diff --base security-posture.json --head widened.json \
  --format markdown >blocked.md 2>blocked.err
status=$?
set -e
[ "$status" = "1" ] || {
  cat blocked.err >&2
  die "the widened manifest must block (exit 1), got exit $status"
}
ok "removing the guard on $METHOD $ROUTE blocks"

# The marker a reviewer would paste: the first `/ack-posture` line, minus the
# command word.
ack="$(grep -m1 -E '^\s*/ack-posture ' blocked.md | sed -E 's#^\s*/ack-posture ##')"
[ -n "$ack" ] || die "the blocked report prints no /ack-posture marker"

set +e
"$AUTUMN_CLI" routes posture diff --base security-posture.json --head widened.json \
  --format markdown --ack "$ack" >acked.md 2>acked.err
status=$?
set -e
[ "$status" = "0" ] || {
  cat acked.err >&2
  die "the acknowledged widening must pass (exit 0), got exit $status"
}
ok "the printed /ack-posture marker unblocks it"

cd "$root"

if [ "$UPDATE" = "1" ]; then
  mkdir -p "$demo"
  cp "$work/blocked.md" "$blocked_golden"
  cp "$work/acked.md" "$acked_golden"
  ok "refreshed $blocked_golden and $acked_golden"
fi

# The docs embed the blocked report between two marker lines, inside a fence.
python3 -I - "$doc" "$work/blocked.md" "$UPDATE" <<'PY'
import sys

doc_path, report_path, update = sys.argv[1], sys.argv[2], sys.argv[3] == "1"
begin = "<!-- posture-demo:begin -->"
end = "<!-- posture-demo:end -->"
with open(doc_path, encoding="utf-8") as f:
    doc = f.read()
with open(report_path, encoding="utf-8") as f:
    report = f.read().rstrip("\n")
if doc.count(begin) != 1 or doc.count(end) != 1:
    sys.exit(f"{doc_path} must hold exactly one {begin} … {end} block")
head, rest = doc.split(begin, 1)
_, tail = rest.split(end, 1)
block = f"{begin}\n\n````markdown\n{report}\n````\n\n{end}"
fresh = head + block + tail
if fresh == doc:
    print(f"ok:    {doc_path} shows the current report")
elif update:
    with open(doc_path, "w", encoding="utf-8") as f:
        f.write(fresh)
    print(f"ok:    refreshed the report in {doc_path}")
else:
    sys.exit(f"error: {doc_path} shows a stale report. Run scripts/check-posture-demo.sh --update")
PY

if [ "$UPDATE" = "0" ]; then
  failed=0
  for pair in "blocked.md:$blocked_golden" "acked.md:$acked_golden"; do
    fresh="$work/${pair%%:*}"
    golden="${pair#*:}"
    if [ ! -f "$golden" ]; then
      echo "error: no golden at $golden — run $0 --update" >&2
      failed=1
    elif ! diff -u "$golden" "$fresh"; then
      echo "error: $golden does not match the diff's output (above)." >&2
      echo "       If the change is intended, run $0 --update." >&2
      failed=1
    else
      ok "$golden matches"
    fi
  done
  [ "$failed" = "0" ] || exit 1
fi

ok "posture demo passed"
