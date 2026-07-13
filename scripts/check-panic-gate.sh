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

for module in "${REQUEST_PATH_MODULES[@]}"; do
  [[ -f "$module" ]] || die "gated request-path module is missing: $module"
  grep -q 'autumn-panic-gate:' "$module" \
    || die "missing gate marker 'autumn-panic-gate:' in $module"
  # The deny header is `#![cfg_attr(not(test), deny(clippy::unwrap_used, …))]`.
  # rustfmt may wrap it across several lines, so match its stable tokens
  # individually rather than as one literal string.
  grep -q '#!\[cfg_attr(' "$module" \
    && grep -q 'not(test)' "$module" \
    && grep -q 'deny(' "$module" \
    && grep -q 'clippy::unwrap_used' "$module" \
    && grep -q 'clippy::indexing_slicing' "$module" \
    || die "missing or incomplete gate deny header (cfg_attr(not(test), deny(…))) in $module"
done

echo "panic-gate: ${#REQUEST_PATH_MODULES[@]} request-path modules gated"
