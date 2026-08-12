#!/usr/bin/env bash
# Verify that every canonical request-path module still carries the #1611
# panic-free gate header. The header opts each module into the panic-class
# clippy denials (unwrap/expect/panic/unreachable/todo/unimplemented/
# indexing_slicing/string_slice/arithmetic_side_effects) on the production code
# path via `cfg_attr(not(test), …)`.
#
# This script guards the *manifest and the header shape*: it fails if a gated
# module is missing, has lost or weakened its gate header, spoofs it with a
# module-wide `allow`, drifted out of the manifest, or is gated behind a Cargo
# feature CI's clippy never enables (so the deny block would never compile).
# The actual panic detection is performed by `cargo clippy` in the same `lint`
# job — this script makes sure that clippy run can still see the denials.
#
# WHAT IT CHECKS
#   1. Every manifest module exists and carries the `autumn-panic-gate:` marker.
#   2. The marker is IMMEDIATELY followed (blank/comment lines aside) by the
#      `#![cfg_attr(` gate header, and THAT block is the one validated — a
#      marker floating free of a header, or an unrelated `cfg_attr` earlier in
#      the file, cannot stand in for the gate.
#   3. The header block is terminated, says `not(test)` + `deny(`, and lists
#      every lint in REQUIRED_PANIC_LINTS (the COMPLETE set, not a subset).
#   4. Anti-spoof: no INNER attribute (`#![…]`) anywhere in a gated module may
#      combine `allow(` with a required lint — a module-wide inner allow would
#      silently defeat the deny. Per-site OUTER `#[allow(…)]` stays legal.
#   5. Per-site allow hygiene: an outer `#[allow(<required lint>…)]` must carry
#      a `reason = "…"` in the same attribute.
#   6. Reverse manifest: every `*.rs` file under the scanned source roots that
#      carries the marker must be listed in the manifest (closes the drift hole
#      where a module was gated in-file but never added to this list).
#   7. Module-count floor: the manifest may not shrink below MODULE_COUNT_FLOOR.
#   8. Feature reachability: a module gated behind a non-default Cargo feature
#      must have that feature enabled by one of ci.yml's `cargo clippy`
#      invocations, otherwise its deny block is never compiled and the gate is
#      decorative.
#
# Called from the `lint` job in ci.yml. Run locally with:
#
#     ./scripts/check-panic-gate.sh              # self-test, then the real check
#     ./scripts/check-panic-gate.sh --self-test  # synthetic fixtures only
#     ./scripts/check-panic-gate.sh --check-only # real tree only
#
# The default invocation runs the self-test FIRST so a refactor that silently
# defangs this script fails here rather than years later on a real regression.
# Both legs together stay well under a second — no toolchain, no network.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

die() {
  echo "error: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Canonical configuration
# ---------------------------------------------------------------------------

# Canonical request-path module set, as `<path>:<cargo feature>`. Every file
# listed here must be panic-free on the production code path and carry the gate
# header. Keep this list in sync with CONTRIBUTING.md "Request-path panic gate".
#
# The `<cargo feature>` suffix is the feature that gates the module's `mod`
# declaration (verified against `autumn/src/lib.rs` and the relevant `mod.rs`
# files, NOT guessed): it drives the feature-reachability check below. Use
# `default` for a module compiled by a plain `cargo clippy --workspace` — that
# includes `autumn-search`, a separate always-built crate. A feature named here
# that is part of `autumn`'s `default` feature set (e.g. `db`) also counts as
# reachable; if it ever leaves the default set, the plain workspace clippy stops
# compiling the module and this check starts demanding an explicit CI feature.
#
# Suffix syntax rather than a parallel array on purpose: a path and its feature
# cannot drift apart when they live in one token.
REQUEST_PATH_MODULES=(
  autumn/src/form.rs:default
  autumn/src/nested_form.rs:default
  autumn/src/extract.rs:default
  autumn/src/idempotency.rs:default
  autumn/src/time_math.rs:default
  autumn/src/mail.rs:mail
  autumn/src/inbound_mail.rs:inbound-mail
  autumn/src/channels.rs:ws
  autumn/src/job.rs:default
  autumn/src/job_tracking.rs:default
  autumn/src/session.rs:default
  autumn/src/session_redis.rs:redis
  autumn/src/scheduler.rs:default
  autumn/src/security/trusted_proxies.rs:default
  autumn/src/storage/blob.rs:storage
  autumn/src/storage/direct_upload.rs:storage
  autumn/src/sync/store.rs:offline-sync
  autumn/src/sync/server.rs:offline-sync
  autumn/src/sync/engine.rs:offline-sync
  autumn/src/middleware/access_log.rs:default
  autumn/src/middleware/exception_filter.rs:default
  autumn/src/middleware/request_id.rs:default
  autumn/src/middleware/method_override.rs:default
  autumn/src/middleware/metrics.rs:default
  autumn/src/middleware/trace_context.rs:telemetry-otlp
  autumn/src/middleware/maintenance.rs:default
  autumn/src/middleware/error_page_filter.rs:default
  autumn/src/middleware/load_shed.rs:default
  autumn/src/search.rs:db
  autumn-search/src/lib.rs:default
)

# The manifest may grow, never shrink. Deleting a gated module is a deliberate
# act that has to move this number too, so a silent `git revert` of an entry
# cannot quietly shrink the gate's surface.
MODULE_COUNT_FLOOR=30

# Gated modules whose feature is KNOWINGLY not enabled by any CI clippy lane,
# as `<path>:<feature>`. Their headers are real but unenforced: the deny block
# is never compiled, so clippy cannot fail on a panic-class site there.
#
# This list exists so that hole is *visible and reviewed* rather than hidden by
# mislabelling the module as `default`. The check prints a NOTE for every entry
# on every run, refuses an entry whose module is not in the manifest, and — so
# the list cannot rot — refuses an entry whose feature has since become
# reachable. Adding an entry is a deliberate, reviewable act; the honest default
# for a request-path module is to be linted.
#
#   middleware/trace_context.rs (#[cfg(feature = "telemetry-otlp")],
#   autumn/src/middleware/mod.rs:32) — telemetry-otlp pulls prost/tonic, whose
#   build scripts need `protoc`, which the `lint` runner does not install (see
#   the docs.rs note at autumn/Cargo.toml:385 and the explicit protoc steps in
#   feature-combinations.yml / publish-gate.yml). Adding the feature to the
#   gated-features clippy step therefore also means adding a protoc install step
#   to the `lint` job. Verified locally with protoc present that
#   `cargo clippy -p autumn-web --features telemetry-otlp --lib -- -D warnings`
#   is already clean, so the burn-down is a workflow change, not a code change.
FEATURE_LINT_EXEMPT=(
  autumn/src/middleware/trace_context.rs:telemetry-otlp
)

# What the checks below actually read; the self-test points it at its own list.
GATE_FEATURE_EXEMPT=("${FEATURE_LINT_EXEMPT[@]}")

# Source roots swept by the reverse-manifest check.
SCAN_DIRS="autumn/src,autumn-search/src"

# Read-only inputs for the feature-reachability check.
CI_WORKFLOW=".github/workflows/ci.yml"
AUTUMN_MANIFEST="autumn/Cargo.toml"

# Canonical panic-class lints every gated module must deny on the production
# code path. The header spells each as a fully-qualified `clippy::<lint>` token
# (that is how rustfmt normalizes it), so match the qualified form to prevent a
# bare `unwrap_used` elsewhere in the file from spoofing the check. If a gated
# module drops any one of these while keeping the rest, the gate is silently
# weakened — so we require the COMPLETE set, not a subset.
REQUIRED_PANIC_LINTS=(
  clippy::unwrap_used
  clippy::expect_used
  clippy::panic
  clippy::unreachable
  clippy::todo
  clippy::unimplemented
  clippy::indexing_slicing
  clippy::string_slice
  clippy::arithmetic_side_effects
)

# ---------------------------------------------------------------------------
# Parsing helpers
# ---------------------------------------------------------------------------

# Lint tokens are matched with a trailing non-identifier guard so `clippy::panic`
# is not satisfied by `clippy::panic_in_result_fn`, nor `clippy::todo` by
# `clippy::todos`. Matching happens with bash's own `=~` rather than a grep
# subprocess: the per-site allow scan walks every attribute of every gated
# module (inbound_mail.rs alone has hundreds), and a subshell per lint per
# attribute is the difference between a snappy gate and a sluggish one.
LINT_BOUNDARY='([^A-Za-z0-9_]|$)'

# One alternation over the whole required set, used as a cheap pre-filter
# before identifying which specific lint matched.
build_any_lint_regex() {
  local joined="" lint
  for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
    joined+="${joined:+|}${lint#clippy::}"
  done
  printf 'clippy::(%s)%s' "$joined" "$LINT_BOUNDARY"
}
ANY_REQUIRED_LINT_RE="$(build_any_lint_regex)"

# Print the gate header block of $1: the `#![cfg_attr(…)]` attribute that
# IMMEDIATELY follows the `autumn-panic-gate:` marker (only blank lines and the
# marker's own `//` comment block may sit between them). Anchoring to the marker
# — rather than grabbing the file's first `cfg_attr` — is what stops a
# `#![cfg_attr(docsrs, …)]` above the header, or a marker buried in a doc
# comment or in test code, from passing for the gate.
#
# Exit codes: 0 ok, 3 no marker, 4 marker not adjacent to a header,
# 5 header opener never terminated, 6 marker but no header before EOF.
gate_header_block() {
  awk '
    BEGIN { rc = 3 }
    !seen && index($0, "autumn-panic-gate:") { seen = 1; next }
    seen && !started {
      s = $0
      sub(/^[[:space:]]+/, "", s)
      if (s == "")        { next }   # blank line
      if (s ~ /^\/\//)    { next }   # rest of the marker comment block
      if (s !~ /^#!\[cfg_attr\(/) { rc = 4; exit }
      started = 1
    }
    started {
      buf = buf $0 ORS
      t = $0
      gsub(/"[^"]*"/, "", t)         # brackets inside string literals do not count
      depth += gsub(/\[/, "[", t)
      depth -= gsub(/\]/, "]", t)
      if (depth <= 0) { printf "%s", buf; rc = 0; exit }
    }
    END {
      if (!seen)          { rc = 3 }
      else if (!started)  { if (rc != 4) rc = 6 }
      else if (rc != 0)   { rc = 5 }
      exit rc
    }
  ' "$1"
}

# Print every attribute in $1 as `KIND<TAB>LINE<TAB>flattened text`, where KIND
# is INNER (`#![…]`) or OUTER (`#[…]`). Multi-line, rustfmt-wrapped attributes
# are flattened into one record, which is what makes the `reason =` hygiene
# check work on wrapped `#[allow(…)]` annotations. Lines that begin a comment
# are skipped, so the marker comment's own `#[allow(clippy::<lint>, …)]`
# example — and doc-comment code samples — are not mistaken for real attributes.
attr_blocks() {
  awk '
    function emit(   t) {
      t = buf
      gsub(/[[:space:]]+/, " ", t)
      sub(/^ /, "", t)
      sub(/ $/, "", t)
      printf "%s\t%d\t%s\n", kind, start, t
      inattr = 0; buf = ""; depth = 0
    }
    {
      s = $0
      sub(/^[[:space:]]+/, "", s)
      if (!inattr) {
        if (s ~ /^\/\//)  { next }
        if (s !~ /^#!?\[/) { next }
        inattr = 1
        kind = (s ~ /^#!\[/) ? "INNER" : "OUTER"
        start = NR
        buf = ""
        depth = 0
      }
      buf = buf " " $0
      t = $0
      gsub(/"[^"]*"/, "", t)
      depth += gsub(/\[/, "[", t)
      depth -= gsub(/\]/, "]", t)
      if (depth <= 0) { emit() }
    }
    END { if (inattr) { emit() } }
  ' "$1"
}

# Print (one per line) every Cargo feature named by a `cargo clippy … --features
# "…"` invocation in the workflow file $1. Any clippy lane counts: what matters
# for the gate is that SOME `-D warnings` clippy run compiles the module with
# its feature on.
ci_clippy_features() {
  grep -oE 'cargo clippy .*--features "[^"]+"' "$1" \
    | grep -oE -- '--features "[^"]+"' \
    | sed -E 's/--features "//; s/"$//' \
    | tr ',' '\n' \
    | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' \
    | sed '/^$/d' \
    | sort -u || true
}

# Print (one per line) the crate's default feature set from the Cargo manifest $1.
cargo_default_features() {
  local line
  line="$(grep -m1 -E '^default[[:space:]]*=[[:space:]]*\[' "$1" || true)"
  [[ -n "$line" ]] \
    || die "could not find a 'default = [ … ]' feature line in $1"
  [[ "$line" == *"]"* ]] \
    || die "'default = [' in $1 wraps across lines; teach cargo_default_features() to parse it"
  grep -oE '"[^"]+"' <<<"$line" | tr -d '"' || true
}

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

# check_module <file> <path-for-messages>
check_module() {
  local file="$1" label="$2"
  local block rc=0

  block="$(gate_header_block "$file")" || rc=$?
  case "$rc" in
    0) ;;
    3) die "missing gate marker 'autumn-panic-gate:' in $label" ;;
    4) die "the 'autumn-panic-gate:' marker in $label is not immediately followed by its
  '#![cfg_attr(not(test), deny(…))]' header (only blank lines and the marker's own
  '//' comment block may sit between them). A marker that floats free of a header
  — in a doc comment, in test code, or above an unrelated cfg_attr — proves nothing." ;;
    5) die "unterminated '#![cfg_attr(not(test), deny(…))]' gate header in $label" ;;
    6) die "the 'autumn-panic-gate:' marker in $label is not followed by any
  '#![cfg_attr(not(test), deny(…))]' header" ;;
    *) die "internal: could not read the gate header of $label (awk exit $rc)" ;;
  esac

  if ! grep -q 'not(test)' <<<"$block" || ! grep -q 'deny(' <<<"$block"; then
    die "missing gate deny header opener '#![cfg_attr(not(test), deny(' in $label"
  fi
  # An `allow(` sharing the gate's own cfg_attr would re-permit what the block
  # next to it denies.
  if grep -q 'allow(' <<<"$block"; then
    die "the gate header in $label contains an 'allow(' — the deny block must deny only.
  Justify a site with a narrowly-scoped outer #[allow(…, reason = \"…\")] instead."
  fi
  # Require every canonical panic lint by its fully-qualified token, matched
  # WITHIN the extracted deny block so no gated module can quietly drop one from
  # the header while a per-site `#[allow(...)]` keeps the token alive elsewhere.
  local lint re
  for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
    re="${lint}${LINT_BOUNDARY}"
    [[ "$block" =~ $re ]] \
      || die "gate header in $label is missing required panic lint '$lint'"
  done

  # Attribute-level checks over the whole file.
  local kind lineno text has_lint
  while IFS=$'\t' read -r kind lineno text; do
    [[ "$text" == *"allow("* ]] || continue
    [[ "$text" =~ $ANY_REQUIRED_LINT_RE ]] || continue
    has_lint=""
    for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
      re="${lint}${LINT_BOUNDARY}"
      if [[ "$text" =~ $re ]]; then
        has_lint="$lint"
        break
      fi
    done
    [[ -n "$has_lint" ]] || continue
    if [[ "$kind" == INNER ]]; then
      die "module-wide inner attribute at $label:$lineno allows '$has_lint':
      $text
  An inner #![…allow(…)] re-permits the lint for the WHOLE module and silently
  defeats the gate header. Move it to the narrowest outer #[allow(…, reason = \"…\")]."
    fi
    if [[ "$text" != *"reason ="* ]]; then
      die "the #[allow(…)] of '$has_lint' at $label:$lineno carries no 'reason = \"…\"':
      $text
  Every panic-gate exception must state the invariant that makes it safe."
    fi
  done < <(attr_blocks "$file")
}

# check_reverse_manifest <dir> <scan-dirs csv> <module…>
#
# Every marker-carrying file under the scanned roots must be in the manifest.
# Without this a module can carry the header (and look gated in review) while
# never being checked here — exactly how nested_form.rs drifted out.
check_reverse_manifest() {
  local dir="$1" scan_csv="$2"
  shift 2
  local -a scan modules=("$@")
  IFS=',' read -r -a scan <<<"$scan_csv"

  # Fail loudly on a bad scan root: a sweep that silently finds nothing would
  # turn this check into a no-op, which is the very failure mode it exists to
  # prevent.
  local d
  for d in "${scan[@]}"; do
    [[ -d "$dir/$d" ]] \
      || die "reverse-manifest scan root '$d' does not exist under $dir"
  done

  local manifest_paths found
  manifest_paths="$(printf '%s\n' "${modules[@]%:*}")"
  while IFS= read -r found; do
    [[ -n "$found" ]] || continue
    found="${found#"$dir"/}"
    grep -qxF "$found" <<<"$manifest_paths" \
      || die "$found carries the 'autumn-panic-gate:' marker but is NOT in
  REQUEST_PATH_MODULES in scripts/check-panic-gate.sh. A gated module outside the
  manifest is unchecked: add it (with its cargo feature suffix), or drop the marker."
  done < <(grep -rl --include='*.rs' 'autumn-panic-gate:' "${scan[@]/#/$dir/}" | sort || true)
}

# check_feature_reachability <dir> <ci workflow> <cargo manifest> <module…>
#
# A module gated behind a feature no CI clippy run enables is never compiled
# with its deny block, so the gate is decorative there. Hard failure by default,
# because the failure mode is invisible: CI stays green while the module is
# unguarded. The only way past it is an explicit entry in FEATURE_LINT_EXEMPT
# (see the comment on that array), which is announced on every run.
#
# Reads the GATE_FEATURE_EXEMPT array so the self-test can supply its own.
check_feature_reachability() {
  local dir="$1" ci_file="$2" cargo_toml="$3"
  shift 3
  local modules=("$@")
  local exempt=(${GATE_FEATURE_EXEMPT[@]+"${GATE_FEATURE_EXEMPT[@]}"})

  [[ -f "$dir/$ci_file" ]] || die "cannot read $ci_file (feature-reachability check)"
  [[ -f "$dir/$cargo_toml" ]] || die "cannot read $cargo_toml (feature-reachability check)"

  local ci_feats default_feats
  ci_feats="$(ci_clippy_features "$dir/$ci_file")"
  [[ -n "$ci_feats" ]] \
    || die "found no 'cargo clippy … --features \"…\"' invocation in $ci_file — the
  feature-reachability check cannot verify that feature-gated request-path modules
  are linted at all."
  default_feats="$(cargo_default_features "$dir/$cargo_toml")"

  # A feature is "linted" if it is on in a default-feature build or named by
  # some CI clippy lane.
  linted() {
    [[ "$1" == "default" ]] && return 0
    grep -qxF "$1" <<<"$default_feats" && return 0
    grep -qxF "$1" <<<"$ci_feats" && return 0
    return 1
  }

  # Validate the exemption list BEFORE using it, so it cannot rot into a
  # permanent blind spot: every entry must still be in the manifest and must
  # still be genuinely unreachable.
  local manifest_entries ex ex_path ex_feat
  manifest_entries="$(printf '%s\n' "${modules[@]}")"
  for ex in ${exempt[@]+"${exempt[@]}"}; do
    ex_path="${ex%:*}"
    ex_feat="${ex##*:}"
    grep -qxF "$ex" <<<"$manifest_entries" \
      || die "FEATURE_LINT_EXEMPT names '$ex', which is not a REQUEST_PATH_MODULES entry.
  An exemption for a module (or a feature) the manifest no longer has is dead weight —
  delete it."
    if linted "$ex_feat"; then
      die "FEATURE_LINT_EXEMPT still exempts $ex_path, but '$ex_feat' IS now enabled by a
  CI clippy lane (or is a default feature), so the module is enforced again. Delete the
  FEATURE_LINT_EXEMPT entry — a stale exemption is how a temporary hole becomes permanent."
    fi
  done

  local entry path feat
  for entry in "${modules[@]}"; do
    path="${entry%:*}"
    feat="${entry##*:}"
    [[ -n "$feat" && "$path" != "$entry" ]] \
      || die "manifest entry '$entry' has no ':<cargo feature>' suffix (use ':default'
  for a module built by a plain \`cargo clippy --workspace\`)"
    linted "$feat" && continue
    if grep -qxF "$entry" <<<"$(printf '%s\n' ${exempt[@]+"${exempt[@]}"})"; then
      echo "panic-gate: NOTE — $path is gated behind '$feat', which no CI clippy lane" \
        "enables, so its deny block is NOT enforced (documented in FEATURE_LINT_EXEMPT)."
      continue
    fi
    die "$path is gated behind the '$feat' cargo feature, but no 'cargo clippy … --features'
  invocation in $ci_file enables it and it is not a default feature. Its
  #![cfg_attr(not(test), deny(…))] block is therefore NEVER compiled and the panic gate
  does not apply to it. Fix by adding '$feat' to the gated-features clippy step in
  $ci_file (installing any system deps that feature needs); if that is genuinely not
  affordable, add '$entry' to FEATURE_LINT_EXEMPT with the reason; or, if the module is
  not really request-path, remove it from REQUEST_PATH_MODULES and drop its marker."
  done
}

# gate_check <dir> <ci workflow> <cargo manifest> <floor> <scan-dirs csv> <module…>
#
# The whole gate, parameterised on its inputs so --self-test can point it at a
# fixture tree instead of the real one.
gate_check() {
  local dir="$1" ci_file="$2" cargo_toml="$3" floor="$4" scan_csv="$5"
  shift 5
  local modules=("$@")

  (( ${#modules[@]} >= floor )) \
    || die "REQUEST_PATH_MODULES has ${#modules[@]} entries, below the committed floor of
  $floor. The gate's surface may grow but not shrink: if a module was genuinely
  retired, lower MODULE_COUNT_FLOOR in the same commit and say why."

  local entry path
  for entry in "${modules[@]}"; do
    path="${entry%:*}"
    [[ -f "$dir/$path" ]] || die "gated request-path module is missing: $path"
    check_module "$dir/$path" "$path"
  done

  check_reverse_manifest "$dir" "$scan_csv" "${modules[@]}"
  check_feature_reachability "$dir" "$ci_file" "$cargo_toml" "${modules[@]}"

  echo "panic-gate: ${#modules[@]} request-path modules gated"
}

run_real_check() {
  gate_check "$root" "$CI_WORKFLOW" "$AUTUMN_MANIFEST" "$MODULE_COUNT_FLOOR" \
    "$SCAN_DIRS" "${REQUEST_PATH_MODULES[@]}"
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

# Fixture tree: a `src/` root, a minimal ci.yml with one clippy feature list,
# and a Cargo.toml with one default feature. Nothing here touches the real tree.
self_test() {
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 -- expand now: $tmp is function-local.
  trap "rm -rf '$tmp'" EXIT
  local pass=0 total=0
  # Shadow the real exemption list for the whole self-test (bash's dynamic
  # scoping reaches gate_check and the subshells the checks run in), so fixtures
  # are judged by the checker's rules alone and the real list is restored on
  # return — the default invocation runs the real check straight afterwards.
  local -a GATE_FEATURE_EXEMPT=()

  # Emit a valid 9-lint gate header (byte-identical in shape to the real ones).
  valid_header() {
    cat <<'EOF'
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
EOF
  }

  # A fixture root holds ONLY the module(s) a scenario is about: every negative
  # case must have exactly one marker-carrying file, so it cannot pass on some
  # other check's error (an early version seeded every root with a spare valid
  # module, and the reverse-manifest check then masked the failures the cases
  # were actually testing).
  make_fixture() {
    local dir="$1"
    mkdir -p "$dir/src" "$dir/.github/workflows"
    printf 'default = ["deffeat"]\n' >"$dir/Cargo.toml"
    printf '        run: cargo clippy -p autumn-web --features "cifeat,other" --all-targets -- -D warnings\n' \
      >"$dir/.github/workflows/ci.yml"
  }

  # Write a valid gated module at $1, optionally followed by extra body lines
  # supplied on stdin.
  write_module() {
    local out="$1"
    valid_header >"$out"
    if [[ ! -t 0 ]]; then cat >>"$out"; fi
  }

  record() {
    local outcome="$1" name="$2" detail="${3-}"
    total=$((total + 1))
    if [[ "$outcome" == ok ]]; then
      echo "self-test PASS: $name"
      pass=$((pass + 1))
    else
      echo "self-test FAIL: $name — $detail" >&2
    fi
  }

  # check_pass <name> <cmd…>
  check_pass() {
    local name="$1"
    shift
    local out got=0
    out="$( ("$@") 2>&1 )" || got=$?
    if [[ "$got" -eq 0 ]]; then
      record ok "$name"
    else
      record no "$name" "expected success, exit=$got: $out"
    fi
  }

  # check_fail <name> <expected message substring> <cmd…>
  #
  # Asserting the MESSAGE, not just the exit code, is the point: it pins each
  # scenario to the specific check it exercises, so a checker that has been
  # defanged (a lint dropped from REQUIRED_PANIC_LINTS, say) cannot keep the
  # self-test green by failing the fixture for an unrelated reason.
  check_fail() {
    local name="$1" want="$2"
    shift 2
    local out got=0
    out="$( ("$@") 2>&1 )" || got=$?
    if [[ "$got" -eq 0 ]]; then
      record no "$name" "expected failure, but the check passed"
    elif [[ "$out" != *"$want"* ]]; then
      record no "$name" "failed for the wrong reason (wanted '$want'): $out"
    else
      record ok "$name"
    fi
  }

  local wf=".github/workflows/ci.yml"

  # 1. A fully valid fixture passes.
  local d1="$tmp/d1"; make_fixture "$d1"
  write_module "$d1/src/good.rs" <<<'pub fn ok() {}'
  check_pass "valid gated module passes" \
    gate_check "$d1" "$wf" Cargo.toml 1 src src/good.rs:default

  # 2. Manifest entry with no gate header at all.
  local d2="$tmp/d2"; make_fixture "$d2"
  printf 'pub fn nope() {}\n' >"$d2/src/bare.rs"
  check_fail "module without a gate header fails" "missing gate marker" \
    gate_check "$d2" "$wf" Cargo.toml 1 src src/bare.rs:default

  # 3. Header missing one required lint (the newest one, string_slice).
  local d3="$tmp/d3"; make_fixture "$d3"
  write_module "$d3/src/thin.rs" </dev/null
  sed -i '/clippy::string_slice/d' "$d3/src/thin.rs"
  check_fail "header missing one required lint fails" "missing required panic lint" \
    gate_check "$d3" "$wf" Cargo.toml 1 src src/thin.rs:default

  # 4. Header opener never closed.
  local d4="$tmp/d4"; make_fixture "$d4"
  write_module "$d4/src/unterminated.rs" </dev/null
  sed -i '/^)\]$/d' "$d4/src/unterminated.rs"
  check_fail "unterminated header fails" "unterminated" \
    gate_check "$d4" "$wf" Cargo.toml 1 src src/unterminated.rs:default

  # 5. Module-wide inner allow after the header (spoof).
  local d5="$tmp/d5"; make_fixture "$d5"
  write_module "$d5/src/spoof.rs" <<'EOF'

#![allow(clippy::unwrap_used)]

pub fn x() {}
EOF
  check_fail "inner #![allow] spoof after the header fails" "module-wide inner attribute" \
    gate_check "$d5" "$wf" Cargo.toml 1 src src/spoof.rs:default

  # 6. allow() smuggled into the gate's own cfg_attr block.
  local d6="$tmp/d6"; make_fixture "$d6"
  write_module "$d6/src/inblock.rs" </dev/null
  sed -i 's/^        clippy::string_slice,$/        clippy::string_slice,\n    ),\n    allow(clippy::indexing_slicing/' \
    "$d6/src/inblock.rs"
  check_fail "allow() inside the cfg_attr block fails" "contains an 'allow('" \
    gate_check "$d6" "$wf" Cargo.toml 1 src src/inblock.rs:default

  # 7. Marker separated from the header by real code.
  local d7="$tmp/d7"; make_fixture "$d7"
  {
    printf '// autumn-panic-gate: request-path module — production code path must be panic-free.\n\n'
    printf 'use std::fmt;\n\n'
    valid_header | tail -n +4
    printf '\npub fn x() {}\n'
  } >"$d7/src/detached.rs"
  check_fail "marker not adjacent to the header fails" "not immediately followed" \
    gate_check "$d7" "$wf" Cargo.toml 1 src src/detached.rs:default

  # 8. Marker-carrying file that is not in the manifest (drift). The only
  #    scenario that deliberately has two marker files in one root.
  local d8="$tmp/d8"; make_fixture "$d8"
  write_module "$d8/src/good.rs" </dev/null
  write_module "$d8/src/unlisted.rs" </dev/null
  check_fail "marker file missing from the manifest fails" "is NOT in" \
    gate_check "$d8" "$wf" Cargo.toml 1 src src/good.rs:default

  # 9. Per-site allow of a gated lint with no reason.
  local d9="$tmp/d9"; make_fixture "$d9"
  write_module "$d9/src/noreason.rs" <<'EOF'

#[allow(clippy::indexing_slicing)]
pub fn x() {}
EOF
  check_fail "per-site #[allow] without a reason fails" "carries no 'reason" \
    gate_check "$d9" "$wf" Cargo.toml 1 src src/noreason.rs:default

  # 10. …and the same allow WITH a reason passes, even rustfmt-wrapped across
  #     lines (the flattening in attr_blocks() is what makes that work).
  local d10="$tmp/d10"; make_fixture "$d10"
  write_module "$d10/src/good.rs" <<'EOF'

#[allow(
    clippy::indexing_slicing,
    reason = "bounds proven by the caller"
)]
pub fn x() {}
EOF
  check_pass "wrapped per-site #[allow(…, reason)] passes" \
    gate_check "$d10" "$wf" Cargo.toml 1 src src/good.rs:default

  # 11. Manifest shrunk below the floor.
  local d11="$tmp/d11"; make_fixture "$d11"
  write_module "$d11/src/good.rs" </dev/null
  check_fail "manifest below the module-count floor fails" "below the committed floor" \
    gate_check "$d11" "$wf" Cargo.toml 2 src src/good.rs:default

  # 12. Feature-gated module whose feature no CI clippy run enables.
  local d12="$tmp/d12"; make_fixture "$d12"
  write_module "$d12/src/good.rs" </dev/null
  check_fail "module gated behind a feature CI never lints fails" "NEVER compiled" \
    gate_check "$d12" "$wf" Cargo.toml 1 src src/good.rs:unlinted-feat

  # 13. …and it passes once that feature is in a CI clippy feature list.
  local d13="$tmp/d13"; make_fixture "$d13"
  write_module "$d13/src/good.rs" </dev/null
  check_pass "module gated behind a CI-linted feature passes" \
    gate_check "$d13" "$wf" Cargo.toml 1 src src/good.rs:cifeat

  # 14. …or is part of the crate's default feature set.
  local d14="$tmp/d14"; make_fixture "$d14"
  write_module "$d14/src/good.rs" </dev/null
  check_pass "module gated behind a default feature passes" \
    gate_check "$d14" "$wf" Cargo.toml 1 src src/good.rs:deffeat

  # 15. Manifest entry pointing at a file that does not exist.
  local d15="$tmp/d15"; make_fixture "$d15"
  check_fail "missing manifest file fails" "module is missing" \
    gate_check "$d15" "$wf" Cargo.toml 1 src src/gone.rs:default

  # 16. A commented-out example attribute must not be mistaken for a real one
  #     (the marker comment block itself contains such an example).
  local d16="$tmp/d16"; make_fixture "$d16"
  write_module "$d16/src/good.rs" <<'EOF'

// #[allow(clippy::unwrap_used)] would need a reason.
pub fn x() {}
EOF
  check_pass "commented-out #[allow] example is ignored" \
    gate_check "$d16" "$wf" Cargo.toml 1 src src/good.rs:default

  # 17. A reverse-manifest sweep that finds nothing because its scan root is
  #     wrong must fail loudly rather than silently checking zero files.
  local d17="$tmp/d17"; make_fixture "$d17"
  write_module "$d17/src/good.rs" </dev/null
  check_fail "bad reverse-manifest scan root fails" "scan root" \
    gate_check "$d17" "$wf" Cargo.toml 1 nosuchdir src/good.rs:default

  # Run gate_check with a specific FEATURE_LINT_EXEMPT list. Assigning the
  # global is safe because check_pass/check_fail run their command in a subshell.
  with_exempt() {
    local list="$1"
    shift
    # shellcheck disable=SC2206 -- deliberate word splitting of a space-separated list.
    GATE_FEATURE_EXEMPT=($list)
    gate_check "$@"
  }

  # 18. An unlinted feature passes when it is documented in FEATURE_LINT_EXEMPT…
  local d18="$tmp/d18"; make_fixture "$d18"
  write_module "$d18/src/good.rs" </dev/null
  check_pass "documented feature-lint exemption passes" \
    with_exempt "src/good.rs:unlinted-feat" \
    "$d18" "$wf" Cargo.toml 1 src src/good.rs:unlinted-feat

  # 19. …and the run says so out loud rather than passing silently.
  local d19="$tmp/d19"; make_fixture "$d19"
  write_module "$d19/src/good.rs" </dev/null
  if [[ "$(with_exempt "src/good.rs:unlinted-feat" \
      "$d19" "$wf" Cargo.toml 1 src src/good.rs:unlinted-feat 2>&1)" == *"NOTE"* ]]; then
    record ok "feature-lint exemption announces itself"
  else
    record no "feature-lint exemption announces itself" "no NOTE line in the output"
  fi

  # 20. A stale exemption — one whose feature IS linted now — is rejected, so a
  #     temporary hole cannot quietly become permanent.
  local d20="$tmp/d20"; make_fixture "$d20"
  write_module "$d20/src/good.rs" </dev/null
  check_fail "stale feature-lint exemption fails" "IS now enabled" \
    with_exempt "src/good.rs:cifeat" \
    "$d20" "$wf" Cargo.toml 1 src src/good.rs:cifeat

  # 21. An exemption for a module the manifest no longer lists is rejected too.
  local d21="$tmp/d21"; make_fixture "$d21"
  write_module "$d21/src/good.rs" </dev/null
  check_fail "exemption for an unlisted module fails" "not a REQUEST_PATH_MODULES entry" \
    with_exempt "src/gone.rs:unlinted-feat" \
    "$d21" "$wf" Cargo.toml 1 src src/good.rs:default

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]] || die "panic-gate self-test failed — the checker is not
  catching what it claims to catch. Fix the checker before trusting a green gate."
  trap - EXIT
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------

case "${1-}" in
  --self-test)
    self_test
    ;;
  --check-only)
    run_real_check
    ;;
  "")
    self_test
    echo
    run_real_check
    ;;
  *)
    die "unknown argument '$1' (expected --self-test, --check-only, or none)"
    ;;
esac
