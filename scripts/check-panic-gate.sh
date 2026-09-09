#!/usr/bin/env bash
# Verify that every canonical request-path module still carries the #1611
# panic-free gate header. The header opts each module into the panic-class
# clippy denials (unwrap/expect/panic/unreachable/todo/unimplemented/
# indexing_slicing/string_slice/arithmetic_side_effects) on the production code
# path via `cfg_attr(not(test), …)`.
#
# This script guards the *manifest and the header shape*, and — crucially — the
# absence of any module-wide escape hatch that would let a production panic ship
# while `cargo clippy -- -D warnings` stays green. The actual panic detection is
# performed by clippy in the same `lint` job; this script keeps that clippy run
# able to see the denials in the first place.
#
# WHAT IT CHECKS
#   1. Every manifest module exists and carries the `autumn-panic-gate:` marker.
#   2. The marker is IMMEDIATELY followed (blank/comment lines aside) by the
#      `#![cfg_attr(` gate header, and THAT block is the one validated.
#   3. STRUCTURAL header shape: after stripping `//` and `/* … */` comments and all whitespace,
#      the header must open EXACTLY `#![cfg_attr(not(test),deny(` — a widened
#      predicate like `all(not(test), any())` (whose deny never compiles) or a
#      `not(test)` that lives only in a comment therefore fail. The block must
#      carry no `allow(`/`expect(`/`warn(` of its own, and must list every lint
#      in REQUIRED_PANIC_LINTS (the COMPLETE set, not a subset).
#   4. Tree-wide inner-suppression scan: NO inner attribute (`#![allow(…)]` or
#      `#![expect(…)]`, including the `cfg_attr(…, allow(…))` form) anywhere under
#      the scan roots may re-permit a required lint OR a blanket lint group
#      (`clippy::restriction|all|pedantic|nursery`). This runs over EVERY `*.rs`
#      under the roots — not just manifest entries — so an unmarked submodule of a
#      gated module cannot slip a module-wide allow past both the manifest and the
#      reverse-manifest check. Attributes inside a `#[cfg(test)]` scope are
#      exempt (that is where a gated module's own tests legitimately allow them).
#   5. Per-site allow hygiene: an OUTER `#[allow(<required lint>…)]` in a gated
#      module must carry a NON-EMPTY `reason = "…"`.
#   6. Reverse manifest: every marker-carrying `*.rs` under the scan roots must be
#      listed in the manifest (closes the in-file-gated-but-unlisted drift hole).
#   7. Module-count floor: the manifest may not shrink below MODULE_COUNT_FLOOR.
#   8. Feature reachability: a module gated behind a non-default Cargo feature must
#      have that feature enabled by an ENFORCING ci.yml clippy lane (one that is
#      not commented out and carries both `-p autumn-web` and `-D warnings`),
#      otherwise its deny block is never compiled. The single knowing exception
#      (telemetry-otlp) is declared in FEATURE_LINT_EXEMPT and announced on every
#      run; that list is itself validated so it cannot rot into a silent hole.
#
# NOT covered (documented in CONTRIBUTING.md "Request-path panic gate"): panics
# that reach a gated module only through a `macro_rules!` expansion, because
# clippy suppresses lints inside macro expansions and no source-level gate can
# see them. Request-path modules must not invoke panic-expanding local macros.
#
# Called from the `lint` job in ci.yml. Run locally with:
#
#     ./scripts/check-panic-gate.sh              # self-test, then the real check
#     ./scripts/check-panic-gate.sh --self-test  # synthetic fixtures only
#     ./scripts/check-panic-gate.sh --check-only # real tree only
#
# The default invocation runs the self-test FIRST so a refactor that silently
# defangs this script fails here rather than years later on a real regression.
# Both legs together run in a couple of seconds — no toolchain, no network
# (the `--check-only` real pass alone is about a second and a half).

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
  autumn/src/query_string.rs:default
  autumn/src/idempotency.rs:default
  autumn/src/time_math.rs:default
  autumn/src/mail.rs:mail
  autumn/src/inbound_mail.rs:inbound-mail
  autumn/src/channels.rs:ws
  autumn/src/job.rs:default
  # The durable SQLite job backend (#1907): a submodule of the gated job.rs,
  # listed on its own because its enforcing clippy lane is the sqlite one.
  autumn/src/job/sqlite.rs:sqlite
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
  autumn/src/middleware/short_circuit.rs:default
  autumn/src/capsule/capture.rs:reporting
  autumn/src/capsule/wire.rs:db
  autumn/src/capsule/record_db.rs:db
  autumn/src/capsule/clock.rs:reporting
  autumn/src/capsule/entropy.rs:reporting
  autumn/src/capsule/effects.rs:reporting
  autumn/src/capsule/persist.rs:reporting
  autumn/src/capsule/redact.rs:reporting
  autumn/src/capsule/schema.rs:reporting
  autumn/src/search.rs:db
  autumn/src/cluster/mod.rs:default
  autumn/src/cluster/counter.rs:default
  autumn/src/cluster/membership.rs:default
  autumn/src/cluster/wire.rs:default
  autumn/src/cluster/transport.rs:default
  autumn/src/cluster/node.rs:default
  autumn/src/shadow/diff.rs:default
  autumn/src/shadow/sample.rs:default
  autumn/src/shadow/transport.rs:default
  autumn/src/shadow/layer.rs:default
  autumn/src/shadow/registry.rs:default
  autumn/src/plugin_sandbox/host.rs:plugin-sandbox
  autumn/src/plugin_sandbox/wire.rs:plugin-sandbox
  autumn/src/plugin_sandbox/plugin.rs:plugin-sandbox
  autumn/src/plugin_sandbox/capability/mod.rs:plugin-sandbox
  autumn/src/plugin_sandbox/grants.rs:plugin-sandbox
  autumn/src/plugin_sandbox/slots.rs:plugin-sandbox
  autumn-search/src/lib.rs:default
  autumn/src/replication/wal.rs:db
  autumn/src/replication/segment.rs:db
  autumn/src/replication/destination.rs:db
  autumn/src/replication/sqlite.rs:db
  autumn/src/replication/restore.rs:db
  autumn/src/replication/engine.rs:db
  autumn/src/replication/status.rs:db
  autumn/src/replication/s3.rs:http-client
  autumn/src/sigv4.rs:default
)

# The manifest may grow, never shrink. Deleting a gated module is a deliberate
# act that has to move this number too, so a silent `git revert` of an entry
# cannot quietly shrink the gate's surface. It tracks the manifest's length, so
# it moves with every addition too — otherwise a one-entry revert would shrink
# the manifest back under the floor while the gate still passed.
MODULE_COUNT_FLOOR=65

# Gated modules whose feature is KNOWINGLY not enabled by any enforcing CI clippy
# lane, as `<path>:<feature>`. Their headers are real but unenforced: the deny
# block is never compiled, so clippy cannot fail on a panic-class site there.
#
# This list exists so that hole is *visible and reviewed* rather than hidden by
# mislabelling the module as `default`. The check prints a NOTE for every entry
# on every run, refuses an entry whose module is not in the manifest, and — so
# the list cannot rot — refuses an entry whose feature has since become
# reachable. Adding an entry is a deliberate, reviewable act; the honest default
# for a request-path module is to be linted.
#
#   Empty. The last entry — middleware/trace_context.rs behind `telemetry-otlp`
#   — was retired by the `sqlite,telemetry-otlp` clippy lane the sqlite-runtime
#   job gained with issue #1907, which installs protoc and enforces the feature.
FEATURE_LINT_EXEMPT=()

# What the checks below actually read; the self-test points it at its own list.
# Guarded expansion so an emptied list is safe under `set -u` on bash < 4.4.
GATE_FEATURE_EXEMPT=(${FEATURE_LINT_EXEMPT[@]+"${FEATURE_LINT_EXEMPT[@]}"})

# Source roots swept by the reverse-manifest and inner-suppression scans. The
# request-path modules live in autumn/src + autumn-search/src, but a module-wide
# panic-lint allow anywhere in a library crate that ships on/near the request
# path is a hole, so the sibling framework crates are swept too. `autumn-cli`
# (an operator tool) and `autumn-macros` (compile-time proc-macro internals) are
# deliberately EXEMPT and absent here.
SCAN_DIRS="autumn/src,autumn-search/src,autumn-admin-plugin/src,autumn-media-plugin/src,autumn-storage-s3/src,autumn-cache-redis/src"

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

# Blanket clippy lint-group tokens. Our nine lints all live in the `restriction`
# group; allowing any of these groups module-wide re-permits them wholesale, so
# an inner allow/expect of a group is as much a spoof as naming the lint.
LINT_GROUPS=(restriction all pedantic nursery)

# ---------------------------------------------------------------------------
# Parsing helpers
# ---------------------------------------------------------------------------

# Lint tokens are matched with a trailing non-identifier guard so `clippy::panic`
# is not satisfied by `clippy::panic_in_result_fn`, nor `clippy::todo` by
# `clippy::todos`. Matching happens with bash's own `=~` rather than a grep
# subprocess: the hot paths walk every attribute of every gated module, and a
# subshell per lint per attribute is the difference between a snappy gate and a
# sluggish one.
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

# The token set the tree-wide inner-suppression scan rejects: every required
# lint PLUS the blanket groups. Emitted as an awk-ready ERE.
build_suppression_token_regex() {
  local joined="" lint grp
  for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
    joined+="${joined:+|}${lint#clippy::}"
  done
  for grp in "${LINT_GROUPS[@]}"; do
    joined+="|$grp"
  done
  printf 'clippy::(%s)([^A-Za-z0-9_]|$)' "$joined"
}
SUPPRESSION_TOKEN_RE="$(build_suppression_token_regex)"

# A non-empty reason string on a per-site allow: `reason = "…"` with at least one
# character inside the quotes. `reason = ""` does not satisfy it.
NONEMPTY_REASON_RE='reason[[:space:]]*=[[:space:]]*"[^"]+"'

# Print the STRUCTURALLY NORMALIZED gate header of $1: the `#![cfg_attr(…)]`
# attribute that IMMEDIATELY follows the `autumn-panic-gate:` marker (only blank
# lines and the marker's own `//` comment block may sit between them), with its
# `//` comments stripped and ALL whitespace removed. Anchoring to the marker —
# rather than grabbing the file's first `cfg_attr` — is what stops a
# `#![cfg_attr(docsrs, …)]` above the header, or a marker buried in a doc comment
# or in test code, from passing for the gate. Normalizing in the same pass (so a
# lint or a `not(test)` that appears only in a `//` comment does not count) keeps
# check_module to a single subprocess per module.
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
      # Strip BOTH `//` line comments and `/* … */` block comments (which may
      # span header lines) with a single left-to-right scan that carries block
      # state in `inblk`. Doing this before the lint/shape checks stops a
      # required lint that is commented out — `/* clippy::panic, */` — from
      # surviving into `norm` and satisfying the token grep while rustc ignores
      # the (commented-out) denial.
      line = $0
      out = ""; i = 1; n = length(line)
      while (i <= n) {
        c = substr(line, i, 2)
        if (inblk) {
          if (c == "*/") { inblk = 0; i += 2 } else { i += 1 }
        } else if (c == "/*") { inblk = 1; i += 2 }
        else if (c == "//")   { break }   # rest of the line is a line comment
        else { out = out substr(line, i, 1); i += 1 }
      }
      line = out
      t = line
      gsub(/"[^"]*"/, "", t)         # brackets inside string literals do not count
      depth += gsub(/\[/, "[", t)
      depth -= gsub(/\]/, "]", t)
      gsub(/[[:space:]]/, "", line)  # remove all whitespace
      norm = norm line
      # Only terminate on a balanced header that is NOT mid-block-comment; an
      # unterminated `/*` in the header runs to EOF and fails as malformed.
      if (!inblk && depth <= 0) { printf "%s", norm; rc = 0; exit }
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

# Print (one per line) every Cargo feature named by an ENFORCING `cargo clippy`
# lane in the workflow file $1. A lane counts only if it is not a YAML comment
# and carries BOTH `-p autumn-web` and `-D warnings` — otherwise a commented-out,
# wrong-package, or non-deny lane could make a feature look linted when no error
# would ever fire, hiding a real hole (and, worse, force deletion of an honest
# FEATURE_LINT_EXEMPT entry). One awk pass (this runs on every gate_check).
ci_clippy_features() {
  awk '
    /^[[:space:]]*#/ { next }                               # skip YAML comment lines
    /cargo clippy/ && /-p autumn-web/ && /-D warnings/ {
      s = $0
      while (match(s, /--features "[^"]+"/)) {
        f = substr(s, RSTART + 12, RLENGTH - 13)            # inside the quotes
        n = split(f, arr, ",")
        for (i = 1; i <= n; i++) {
          g = arr[i]
          gsub(/^[[:space:]]+|[[:space:]]+$/, "", g)
          if (g != "") print g
        }
        s = substr(s, RSTART + RLENGTH)
      }
    }
  ' "$1" | sort -u || true
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
#
# Validates the gate header shape and the per-site OUTER allow hygiene of one
# manifest module. Module-wide INNER suppressions are NOT judged here — they are
# caught tree-wide by check_inner_suppressions so an unmarked submodule cannot
# escape — so this loop simply skips inner attributes.
check_module() {
  local file="$1" label="$2"
  local norm rc=0

  norm="$(gate_header_block "$file")" || rc=$?
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

  # Structural shape: gate_header_block already stripped comments + whitespace,
  # so the normalized header must OPEN exactly `#![cfg_attr(not(test),deny(`. A
  # substring test for 'not(test)' and 'deny(' is not enough —
  # `#![cfg_attr(all(not(test), any()), deny(…))]` would pass it while `any()`
  # keeps the deny from ever compiling, and a `not(test)` sitting only in a `//`
  # comment would pass it too.
  local want='#![cfg_attr(not(test),deny('
  [[ "${norm:0:${#want}}" == "$want" ]] \
    || die "the gate header in $label does not open exactly '#![cfg_attr(not(test), deny(…'.
  After stripping comments and whitespace it began: '${norm:0:48}…'. A widened cfg
  predicate (e.g. all(not(test), any())) makes the deny block never compile, and a
  'not(test)' that appears only in a comment does not gate anything. Use the header
  verbatim from CONTRIBUTING.md."
  # The block must deny only — no allow/expect/warn smuggled beside the deny.
  if [[ "$norm" == *"allow("* || "$norm" == *"expect("* || "$norm" == *"warn("* ]]; then
    die "the gate header in $label contains an allow(/expect(/warn( — the deny block must
  deny only. Justify a site with a narrowly-scoped outer #[allow(…, reason = \"…\")] instead."
  fi
  # Require every canonical panic lint by its fully-qualified token, matched in
  # the comment-stripped normalized header so a lint that appears only in a `//`
  # comment cannot stand in for a lint dropped from the real deny list.
  local lint re
  for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
    re="${lint}${LINT_BOUNDARY}"
    [[ "$norm" =~ $re ]] \
      || die "gate header in $label is missing required panic lint '$lint'"
  done

  # Per-site OUTER allow hygiene: every #[allow(<required lint>…)] needs a
  # non-empty reason. INNER attributes are handled tree-wide, so skip them here.
  local kind lineno text has_lint
  while IFS=$'\t' read -r kind lineno text; do
    [[ "$kind" == OUTER ]] || continue
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
    [[ "$text" =~ $NONEMPTY_REASON_RE ]] \
      || die "the #[allow(…)] of '$has_lint' at $label:$lineno carries no non-empty 'reason = \"…\"':
      $text
  Every panic-gate exception must state the invariant that makes it safe."
  done < <(attr_blocks "$file")
}

# check_inner_suppressions <dir> <scan-dirs csv>
#
# Tree-wide: reject any module-wide inner suppression of a panic-gate lint. An
# inner `#![allow(…)]` / `#![expect(…)]` (or the `#![cfg_attr(…, allow(…))]`
# form) that names a required lint OR a blanket group re-permits it for the whole
# module and beats the parent module's deny — and because it needs no marker and
# no manifest entry, neither check_module nor the reverse-manifest scan would see
# it in an unmarked submodule. Attributes inside a `#[cfg(test)]` scope are
# exempt: that is where a gated module's own tests legitimately allow them.
check_inner_suppressions() {
  local dir="$1" scan_csv="$2"
  local -a scan
  IFS=',' read -r -a scan <<<"$scan_csv"

  local -a files=()
  mapfile -t files < <(find "${scan[@]/#/$dir/}" -name '*.rs' 2>/dev/null | sort)
  [[ ${#files[@]} -gt 0 ]] || return 0

  local hit
  # awk emits `file<TAB>line<TAB>attr` for each offending inner suppression.
  hit="$(awk -v BAD="$SUPPRESSION_TOKEN_RE" '
    FNR == 1 { depth = 0; ntest = 0; pending = 0; inattr = 0; abuf = ""; adepth = 0; delete tat }
    {
      code = $0
      sub(/\/\/.*/, "", code)   # strip line comment

      # A #[cfg(test)] / #[cfg(all(test, …))] on this line arms the next brace as
      # a test scope. (Wrapped cfg attrs are rare; rustfmt keeps them one-line.)
      if (code ~ /#\[[[:space:]]*cfg\([[:space:]]*(all\([[:space:]]*)?test[[:space:],)]/) pending = 1

      # Accumulate a (possibly multi-line) INNER attribute.
      if (!inattr) {
        a = code
        sub(/^[[:space:]]+/, "", a)
        if (a ~ /^#!\[/) { inattr = 1; astart = FNR; abuf = ""; adepth = 0 }
      }
      if (inattr) {
        abuf = abuf " " code
        t = code
        gsub(/"[^"]*"/, "", t)
        adepth += gsub(/\[/, "[", t)
        adepth -= gsub(/\]/, "]", t)
        if (adepth <= 0) {
          flat = abuf
          gsub(/[[:space:]]+/, " ", flat)
          sub(/^ /, "", flat)
          if (ntest == 0 && (flat ~ /allow\(/ || flat ~ /expect\(/) && flat ~ BAD)
            printf "%s\t%d\t%s\n", FILENAME, astart, flat
          inattr = 0
        }
      }

      # Brace + test-scope tracking (attributes carry no { }, so counting their
      # lines is a no-op).
      n = length(code)
      for (i = 1; i <= n; i++) {
        c = substr(code, i, 1)
        if (c == "{") {
          depth++
          if (pending) { tat[depth] = 1; ntest++; pending = 0 } else { tat[depth] = 0 }
        } else if (c == "}") {
          if (depth > 0) { if (tat[depth]) { ntest--; tat[depth] = 0 } depth-- }
        }
      }
    }
  ' "${files[@]}")"

  if [[ -n "$hit" ]]; then
    local f l a
    IFS=$'\t' read -r f l a <<<"${hit%%$'\n'*}"
    f="${f#"$dir"/}"
    die "module-wide inner suppression at $f:$l re-permits a panic-gate lint:
      $a
  An inner #![allow(…)] / #![expect(…)] (or a cfg_attr wrapping one) applies to the
  WHOLE module and beats the request-path deny — even in an unmarked submodule of a
  gated module. Move it to the narrowest OUTER #[allow(…, reason = \"…\")] on the one
  site that needs it, or fix the underlying panic. (Inner allows inside a
  #[cfg(test)] scope are fine and are exempt.)"
  fi
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
# A module gated behind a feature no enforcing CI clippy run enables is never
# compiled with its deny block, so the gate is decorative there. Hard failure by
# default, because the failure mode is invisible: CI stays green while the module
# is unguarded. The only way past it is an explicit entry in FEATURE_LINT_EXEMPT
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
    || die "found no enforcing 'cargo clippy … -p autumn-web … --features \"…\" … -D warnings'
  invocation in $ci_file — the feature-reachability check cannot verify that
  feature-gated request-path modules are linted at all."
  default_feats="$(cargo_default_features "$dir/$cargo_toml")"

  # A feature is "linted" if it is on in a default-feature build or named by
  # some enforcing CI clippy lane.
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
      die "FEATURE_LINT_EXEMPT still exempts $ex_path, but '$ex_feat' IS now enabled by an
  enforcing CI clippy lane (or is a default feature), so the module is enforced again.
  Delete the FEATURE_LINT_EXEMPT entry — a stale exemption is how a temporary hole
  becomes permanent."
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
      echo "panic-gate: NOTE — $path is gated behind '$feat', which no enforcing CI clippy" \
        "lane enables, so its deny block is NOT enforced (documented in FEATURE_LINT_EXEMPT)."
      continue
    fi
    die "$path is gated behind the '$feat' cargo feature, but no enforcing 'cargo clippy …
  --features' invocation in $ci_file enables it and it is not a default feature. Its
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
  check_inner_suppressions "$dir" "$scan_csv"
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
  # shellcheck disable=SC2064 # expand now: $tmp is function-local.
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

  # A default enforcing ci.yml clippy lane (has -p autumn-web AND -D warnings).
  default_ci() {
    printf '        run: cargo clippy -p autumn-web --features "cifeat,other" --all-targets -- -D warnings\n'
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
    default_ci >"$dir/.github/workflows/ci.yml"
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

  # 3b. Required lint commented out with a `/* … */` BLOCK comment. rustc stops
  # applying the denial, so the token must not survive into the normalized
  # header (line comments were already handled; this pins the block-comment case).
  local d3b="$tmp/d3b"; make_fixture "$d3b"
  write_module "$d3b/src/blockcomment.rs" </dev/null
  sed -i 's#clippy::panic,#/* clippy::panic, */#' "$d3b/src/blockcomment.rs"
  check_fail "required lint commented out with a block comment fails" "missing required panic lint" \
    gate_check "$d3b" "$wf" Cargo.toml 1 src src/blockcomment.rs:default

  # 4. Header opener never closed.
  local d4="$tmp/d4"; make_fixture "$d4"
  write_module "$d4/src/unterminated.rs" </dev/null
  sed -i '/^)\]$/d' "$d4/src/unterminated.rs"
  check_fail "unterminated header fails" "unterminated" \
    gate_check "$d4" "$wf" Cargo.toml 1 src src/unterminated.rs:default

  # 5. Module-wide inner allow after the header (spoof) — caught tree-wide.
  local d5="$tmp/d5"; make_fixture "$d5"
  write_module "$d5/src/spoof.rs" <<'EOF'

#![allow(clippy::unwrap_used)]

pub fn x() {}
EOF
  check_fail "inner #![allow] spoof after the header fails" "module-wide inner suppression" \
    gate_check "$d5" "$wf" Cargo.toml 1 src src/spoof.rs:default

  # 6. allow() smuggled into the gate's own cfg_attr block.
  local d6="$tmp/d6"; make_fixture "$d6"
  write_module "$d6/src/inblock.rs" </dev/null
  sed -i 's/^        clippy::string_slice,$/        clippy::string_slice,\n    ),\n    allow(clippy::indexing_slicing/' \
    "$d6/src/inblock.rs"
  check_fail "allow() inside the cfg_attr block fails" "deny block must" \
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
  check_fail "per-site #[allow] without a reason fails" "non-empty 'reason" \
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
    # shellcheck disable=SC2206 # deliberate word splitting of a space-separated list.
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

  # ----- hardening bypasses (gate-integrity review) -----

  # 22. Widened cfg predicate: all(not(test), any()) — deny never compiles.
  local d22="$tmp/d22"; make_fixture "$d22"
  {
    valid_header | sed 's/^    not(test),$/    all(not(test), any()),/'
    printf '\npub fn x() {}\n'
  } >"$d22/src/widened.rs"
  check_fail "widened cfg predicate (all(not(test), any())) fails" "does not open exactly" \
    gate_check "$d22" "$wf" Cargo.toml 1 src src/widened.rs:default

  # 23. not(test) only in a comment; the real predicate is `test`.
  local d23="$tmp/d23"; make_fixture "$d23"
  {
    printf '// autumn-panic-gate: request-path module — production code path must be panic-free.\n'
    printf '#![cfg_attr(\n    test, // really not(test) — honest, guv\n    deny(\n'
    printf '        clippy::unwrap_used,\n        clippy::expect_used,\n        clippy::panic,\n'
    printf '        clippy::unreachable,\n        clippy::todo,\n        clippy::unimplemented,\n'
    printf '        clippy::indexing_slicing,\n        clippy::string_slice,\n'
    printf '        clippy::arithmetic_side_effects,\n    )\n)]\n\npub fn x() {}\n'
  } >"$d23/src/commented.rs"
  check_fail "not(test) only in a comment fails" "does not open exactly" \
    gate_check "$d23" "$wf" Cargo.toml 1 src src/commented.rs:default

  # 24. Inner #![expect(…)] of required lints (expect, not allow) — tree-wide.
  local d24="$tmp/d24"; make_fixture "$d24"
  write_module "$d24/src/exp.rs" <<'EOF'

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

pub fn x() {}
EOF
  check_fail "inner #![expect] of required lints fails" "module-wide inner suppression" \
    gate_check "$d24" "$wf" Cargo.toml 1 src src/exp.rs:default

  # 25. Inner #![allow(clippy::restriction)] blanket group — tree-wide.
  local d25="$tmp/d25"; make_fixture "$d25"
  write_module "$d25/src/grp.rs" <<'EOF'

#![allow(clippy::restriction)]

pub fn x() {}
EOF
  check_fail "inner #![allow(clippy::restriction)] group fails" "module-wide inner suppression" \
    gate_check "$d25" "$wf" Cargo.toml 1 src src/grp.rs:default

  # 26. Unmarked submodule of a gated module carrying an inner allow: invisible
  #     to both the manifest and the reverse-manifest scan (no marker), caught
  #     tree-wide. The manifest module itself is clean.
  local d26="$tmp/d26"; make_fixture "$d26"
  write_module "$d26/src/good.rs" </dev/null
  printf '#![allow(clippy::unwrap_used)]\n\npub fn helper() -> u8 { "1".parse().unwrap() }\n' \
    >"$d26/src/helpers.rs"
  check_fail "unmarked submodule inner allow fails" "module-wide inner suppression" \
    gate_check "$d26" "$wf" Cargo.toml 1 src src/good.rs:default

  # 27. Inner allow inside a #[cfg(test)] mod tests { … } is exempt (PASS).
  local d27="$tmp/d27"; make_fixture "$d27"
  write_module "$d27/src/good.rs" <<'EOF'

pub fn x() {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #[test]
    fn t() { let _: u8 = "1".parse().unwrap(); }
}
EOF
  check_pass "inner allow inside #[cfg(test)] mod is exempt" \
    gate_check "$d27" "$wf" Cargo.toml 1 src src/good.rs:default

  # 28. A cfg_attr(feature = …, allow(dead_code)) inner attr (a real pattern in
  #     job.rs) must NOT trip the tree-wide scan — dead_code is not a panic lint.
  local d28="$tmp/d28"; make_fixture "$d28"
  write_module "$d28/src/good.rs" <<'EOF'
#![cfg_attr(feature = "sqlite", allow(dead_code))]

pub fn x() {}
EOF
  check_pass "cfg_attr allow(dead_code) does not trip the scan" \
    gate_check "$d28" "$wf" Cargo.toml 1 src src/good.rs:default

  # 29. Empty reason string on a per-site allow is rejected.
  local d29="$tmp/d29"; make_fixture "$d29"
  write_module "$d29/src/empty.rs" <<'EOF'

#[allow(clippy::indexing_slicing, reason = "")]
pub fn x() {}
EOF
  check_fail "empty reason string fails" "non-empty 'reason" \
    gate_check "$d29" "$wf" Cargo.toml 1 src src/empty.rs:default

  # 30. A COMMENTED-OUT ci lane mentioning the exempt feature must not count as
  #     linting it — the exemption stays valid and the NOTE still prints (PASS).
  local d30="$tmp/d30"; make_fixture "$d30"
  {
    default_ci
    printf '        # run: cargo clippy -p autumn-web --features "otlp" --all-targets -- -D warnings\n'
  } >"$d30/.github/workflows/ci.yml"
  write_module "$d30/src/good.rs" </dev/null
  if [[ "$(with_exempt "src/good.rs:otlp" \
      "$d30" "$wf" Cargo.toml 1 src src/good.rs:otlp 2>&1)" == *"NOTE"* ]]; then
    record ok "commented-out ci lane does not count as linting the feature"
  else
    record no "commented-out ci lane does not count as linting the feature" "expected a PASS with NOTE"
  fi

  # 31. A clippy lane WITHOUT -D warnings does not count as enforcing.
  local d31="$tmp/d31"; make_fixture "$d31"
  printf '        run: cargo clippy -p autumn-web --features "softfeat" --all-targets\n' \
    >"$d31/.github/workflows/ci.yml"
  write_module "$d31/src/good.rs" </dev/null
  check_fail "non-deny clippy lane does not count" "no enforcing" \
    gate_check "$d31" "$wf" Cargo.toml 1 src src/good.rs:softfeat

  # 32. A clippy lane WITHOUT -p autumn-web does not count either.
  local d32="$tmp/d32"; make_fixture "$d32"
  printf '        run: cargo clippy --workspace --features "wsfeat" --all-targets -- -D warnings\n' \
    >"$d32/.github/workflows/ci.yml"
  write_module "$d32/src/good.rs" </dev/null
  check_fail "workspace-scoped clippy lane does not count" "no enforcing" \
    gate_check "$d32" "$wf" Cargo.toml 1 src src/good.rs:wsfeat

  # 33. The inner-suppression scan really is tree-wide: a suppression in a
  #     SECOND scan root (not the manifest module's root) is still caught.
  local d33="$tmp/d33"; make_fixture "$d33"
  mkdir -p "$d33/other"
  write_module "$d33/src/good.rs" </dev/null
  printf '#![allow(clippy::panic)]\n\npub fn y() {}\n' >"$d33/other/sneaky.rs"
  check_fail "suppression in a second scan root is caught" "module-wide inner suppression" \
    gate_check "$d33" "$wf" Cargo.toml 1 src,other src/good.rs:default

  # 34. A non-required inner allow (circuit_breaker.rs pattern) is fine (PASS).
  local d34="$tmp/d34"; make_fixture "$d34"
  write_module "$d34/src/good.rs" </dev/null
  printf '#![allow(clippy::missing_panics_doc, clippy::items_after_statements)]\n\npub fn z() {}\n' \
    >"$d34/src/adjacent.rs"
  check_pass "non-required inner allow is fine" \
    gate_check "$d34" "$wf" Cargo.toml 1 src src/good.rs:default

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
