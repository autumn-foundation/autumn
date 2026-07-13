#!/usr/bin/env bash
# Verify that every canonical request-path module still carries the #1611
# panic-free gate header. The header opts each module into the panic-class
# clippy denials (unwrap/expect/panic/unreachable/todo/unimplemented/
# indexing_slicing) on the production code path via `cfg_attr(not(test), …)`.
#
# This script only guards the *manifest*: it fails if a gated module is missing
# or has lost its gate header, so the gate cannot be silently dropped. The
# actual panic detection is performed by `cargo clippy` in the same `lint` job.
#
# Called from the `lint` job in ci.yml. Run locally with:
#
#     ./scripts/check-panic-gate.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

# Canonical request-path module set. Every file listed here must be panic-free
# on the production code path and carry the gate header. Keep this list in sync
# with CONTRIBUTING.md "Request-path panic gate".
REQUEST_PATH_MODULES=(
  autumn/src/form.rs
  autumn/src/extract.rs
  autumn/src/idempotency.rs
  autumn/src/mail.rs
  autumn/src/channels.rs
  autumn/src/job.rs
  autumn/src/job_tracking.rs
  autumn/src/session.rs
  autumn/src/session_redis.rs
  autumn/src/scheduler.rs
  autumn/src/security/trusted_proxies.rs
  autumn/src/storage/blob.rs
  autumn/src/storage/direct_upload.rs
  autumn/src/sync/store.rs
  autumn/src/sync/server.rs
  autumn/src/sync/engine.rs
  autumn/src/middleware/access_log.rs
  autumn/src/middleware/exception_filter.rs
  autumn/src/middleware/request_id.rs
  autumn/src/middleware/method_override.rs
  autumn/src/middleware/metrics.rs
  autumn/src/middleware/trace_context.rs
  autumn/src/middleware/maintenance.rs
  autumn/src/middleware/error_page_filter.rs
  autumn/src/middleware/load_shed.rs
)

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
)

for module in "${REQUEST_PATH_MODULES[@]}"; do
  [[ -f "$module" ]] || die "gated request-path module is missing: $module"
  grep -q 'autumn-panic-gate:' "$module" \
    || die "missing gate marker 'autumn-panic-gate:' in $module"
  # Extract ONLY the crate-level panic-gate deny attribute. rustfmt wraps it
  # across several lines:
  #
  #     #![cfg_attr(
  #         not(test),
  #         deny(
  #             clippy::unwrap_used,
  #             …
  #             clippy::indexing_slicing,
  #         )
  #     )]
  #
  # Capture the region from the `#![cfg_attr(` opener through its closing `)]`
  # and validate the required lints INSIDE that block only. The block is
  # buffered and emitted solely once the closer is seen, so an unterminated
  # header yields an empty block (and a hard failure below) rather than a run
  # to EOF. Scoping to the block matters: gated modules now carry legitimate
  # per-site `#[allow(clippy::expect_used, reason = "…")]` annotations, and a
  # whole-file scan would let such an `#[allow(...)]` spoof a lint that was
  # actually dropped from the deny header.
  deny_block="$(awk '
    /#!\[cfg_attr\(/       { capturing = 1 }
    capturing             { buf = buf $0 ORS }
    capturing && /\)\]/    { printf "%s", buf; exit }
  ' "$module")"

  [[ -n "$deny_block" ]] \
    || die "could not locate a terminated '#![cfg_attr(not(test), deny(…))]' gate header in $module"
  grep -q 'not(test)' <<<"$deny_block" \
    && grep -q 'deny(' <<<"$deny_block" \
    || die "missing gate deny header opener '#![cfg_attr(not(test), deny(' in $module"
  # Require every canonical panic lint by its fully-qualified token, matched
  # WITHIN the extracted deny block so no gated module can quietly drop one from
  # the header while a per-site `#[allow(...)]` keeps the token alive elsewhere.
  for lint in "${REQUIRED_PANIC_LINTS[@]}"; do
    grep -qF "$lint" <<<"$deny_block" \
      || die "gate header in $module is missing required panic lint '$lint'"
  done
done

echo "panic-gate: ${#REQUEST_PATH_MODULES[@]} request-path modules gated"
