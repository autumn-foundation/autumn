#!/usr/bin/env bash
# Report SemVer compatibility of the public API surface for every publishable
# crate against the most recently published version on crates.io.
#
# Uses `cargo-semver-checks` (https://github.com/obi1kenobi/cargo-semver-checks).
# The tool is installed automatically if not already present.
#
# Patch and minor releases fail if any public API break is detected.
# Intentional breaking releases (major bumps, or pre-1.0 minor bumps) pass
# the gate automatically when a migration guide exists at
# docs/migrations/<version>.md — the presence of the guide is proof that the
# break is documented and acknowledged.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-semver.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

workspace_package_value() {
  local key="$1"
  awk -v key="$key" '
    /^\[workspace\.package\]/ { in_block = 1; next }
    /^\[/ && in_block         { in_block = 0 }
    in_block && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      match($0, /"[^"]+"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' Cargo.toml
}

install_cargo_semver_checks() {
  echo "cargo-semver-checks not found — installing..."
  # This installs a local CI/helper tool, not a shipped artifact. Avoid the
  # release-profile LTO/codegen-units=1 hotspot that has produced rustc access
  # violations on Windows while compiling cargo-semver-checks itself.
  CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}" \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}" \
    cargo install cargo-semver-checks --locked
}

# Install cargo-semver-checks if it is not on PATH.
if ! command -v cargo-semver-checks &>/dev/null; then
  install_cargo_semver_checks
fi

# Workspace version — used to locate the migration guide for intentional breaks.
workspace_version="$(workspace_package_value "version")"
[[ -n "$workspace_version" ]] || die "could not parse workspace version from Cargo.toml"
migration_guide="docs/migrations/${workspace_version}.md"

# cargo-semver-checks 0.48 requires rustc >= 1.91.0, while rustc 1.96.0 has
# ICE'd in the rustdoc JSON path for the Diesel/diesel-async graph. Keep this
# pinned to a known-good toolchain instead of following latest stable.
# Floor raised from 1.92.0 to 1.94.1: the aws-smithy-* releases of 2026-07-07
# (e.g. aws-smithy-async 1.3.0) declare rust-version 1.94.1, and
# cargo-semver-checks builds the autumn-storage-s3 crates.io baseline with a
# fresh lockfile, so any toolchain below that MSRV fails the baseline build.
# Keep in sync with the dtolnay/rust-toolchain pin in the semver job of
# .github/workflows/publish-gate.yml.
semver_toolchain="${AUTUMN_SEMVER_RUST_VERSION:-1.94.1}"

command -v rustup &>/dev/null || die "rustup is required to run SemVer checks with Rust $semver_toolchain"
if ! rustup toolchain list | grep -Fq "${semver_toolchain}-"; then
  die "Rust $semver_toolchain toolchain is required for SemVer checks; run: rustup toolchain install $semver_toolchain"
fi
SEMVER_CARGO=(rustup run "$semver_toolchain" cargo)
echo "semver rust toolchain = $semver_toolchain"

# Parse version components (strip any pre-release suffix, e.g. -alpha.1).
_ver="${workspace_version%%-*}"
IFS='.' read -r _vmaj _vmin _vpatch <<< "$_ver"

# Returns 0 if the version bump type allows breaking API changes per release policy:
#   post-1.0 major bump  →  X.0.0  (major >= 1, minor == 0, patch == 0)
#   pre-1.0 minor bump   →  0.Y.0  (major == 0, patch == 0)
# All other version shapes (patch releases, post-1.0 minor releases) must not
# contain breaking changes regardless of whether a migration guide exists.
is_breaking_release_type() {
  if [[ "$_vmaj" -ge 1 && "$_vmin" -eq 0 && "$_vpatch" -eq 0 ]]; then
    return 0  # post-1.0 major bump
  elif [[ "$_vmaj" -eq 0 && "$_vpatch" -eq 0 ]]; then
    return 0  # pre-1.0 minor bump
  else
    return 1  # patch or post-1.0 minor — no breaks allowed
  fi
}

# Publishable crates.
CRATES=(
  autumn-macros
  autumn-web
  autumn-cli
  autumn-admin-plugin
  autumn-storage-s3
  autumn-cache-redis
)

breaking_failures=0
tool_failures=0

for crate in "${CRATES[@]}"; do
  echo ""
  echo "==> semver-checks: $crate"
  # Capture output so we can parse it; cargo-semver-checks always exits 0
  # (compatible) or 1 (any other outcome: breaking changes, no library target,
  # crate not published, compilation error, registry failure, etc.).
  # We distinguish these cases by parsing the output rather than relying on
  # exit codes alone.
  set +e
  crate_output="$("${SEMVER_CARGO[@]}" semver-checks check-release --package "$crate" 2>&1)"
  exit_code=$?
  set -e

  echo "$crate_output"

  if [[ $exit_code -eq 0 ]]; then
    echo "  PASS: $crate API is semver-compatible with crates.io baseline"
  elif echo "$crate_output" | grep -q "no crates with library targets selected"; then
    # Proc-macro or binary-only crate — no public API surface to semver-check.
    echo "  SKIP: $crate has no library targets (proc-macro or binary)"
  elif echo "$crate_output" | grep -q "not found in registry"; then
    # Crate has never been published on crates.io; nothing to compare against.
    echo "  SKIP: $crate not yet published on crates.io"
  elif echo "$crate_output" | grep -qE "error\[E0282\]" && echo "$crate_output" | grep -q "aws-runtime"; then
    # aws-runtime has a type-inference regression (E0282) on Rust 1.92.x that
    # affects every published version of the crate.  This is an upstream bug
    # unrelated to our public API surface; skip rather than hard-fail so the
    # gate remains actionable for real semver breaks.
    echo "  SKIP: $crate — aws-runtime E0282 upstream regression on Rust $semver_toolchain (not a semver issue)"
  elif echo "$crate_output" | grep -qE "SyncConnectionWrapper" && echo "$crate_output" | grep -q "AsyncPgConnection"; then
    echo "  SKIP: $crate — sqlite feature unification breaks Postgres defaults during semver checks (not a semver issue)"
  elif echo "$crate_output" | grep -qE "error\[E0119\]" && echo "$crate_output" | grep -qE 'HourBase|conflicting implementation in crate `time`'; then
    # time 0.3.48 added a blanket From<HourBase> impl that breaks trait
    # coherence (E0119) in downstream crates with their own blanket From
    # impls — bollard 0.20.x, aws-smithy-types 1.4.x, and ratatui-widgets
    # are all affected. The isolated workspace used by cargo-semver-checks
    # resolves time fresh, so it picks up the broken release even though
    # the workspace pins time below it (<0.3.48). Upstream breakage, not a
    # semver issue; remove once time yanks/fixes 0.3.48 or the affected
    # crates ship releases compatible with it.
    echo "  SKIP: $crate — time 0.3.48 coherence regression (E0119) breaking downstream crates (not a semver issue)"
  elif echo "$crate_output" | grep -qE "checks failed|semver requires"; then
    # Exit 1 with semver-violation output → actual breaking API changes found.
    # Allow them through only when BOTH conditions hold:
    #   1. A migration guide exists (explicit acknowledgement).
    #   2. The version bump type permits breaking changes per release policy
    #      (post-1.0 major bump X.0.0, or pre-1.0 minor bump 0.Y.0).
    # A patch release or a post-1.0 minor release must never break even if a
    # migration stub is present, to prevent an accidental regression slipping
    # through because an old migration document was lying around.
    if [[ -f "$migration_guide" ]] && is_breaking_release_type; then
      echo "  ADVISORY: $crate has breaking API changes; intentional — migration guide found at $migration_guide"
    elif [[ -f "$migration_guide" ]]; then
      echo "  FAIL: $crate has breaking API changes but $workspace_version is a patch/minor release." >&2
      echo "        Breaking changes require a major bump (X.0.0, X≥1) or a pre-1.0 minor bump (0.Y.0)." >&2
      breaking_failures=$((breaking_failures + 1))
    else
      echo "  FAIL: $crate has unacknowledged breaking API changes." >&2
      echo "        Add a migration guide at $migration_guide to acknowledge an intentional break," >&2
      echo "        or fix the API regression before releasing." >&2
      breaking_failures=$((breaking_failures + 1))
    fi
  else
    # Exit 1 with unrecognised output → tool/invocation error (compilation
    # failure, registry timeout, unsupported flag, etc.).  Hard-fail so the
    # error is investigated rather than silently skipped.
    echo "  FAIL: $crate — cargo-semver-checks failed with an unexpected error (exit $exit_code)." >&2
    tool_failures=$((tool_failures + 1))
  fi
done

echo ""
if [[ "$tool_failures" -gt 0 ]]; then
  die "SemVer check had $tool_failures tool/invocation failure(s)."
fi

if [[ "$breaking_failures" -gt 0 ]]; then
  # On pull requests the SemVer gate is informational: surface the findings so
  # reviewers can see them, but exit 0 so the check run shows green and does not
  # block merging.  The gate is a hard blocker only on tag-push releases, where
  # `continue-on-error` cannot be used because branch-protection rules evaluate
  # the individual check-run conclusion (which stays "failure" even when
  # continue-on-error is set at the job level).
  if [[ "${GITHUB_EVENT_NAME:-}" == "pull_request" ]]; then
    echo "NOTE: $breaking_failures crate(s) have breaking API changes."
    echo "      These findings are informational on pull requests."
    echo "      They will block a tag-push release unless a migration guide"
    echo "      exists at $migration_guide."
    exit 0
  fi
  die "$breaking_failures crate(s) have unacknowledged breaking public API changes."
fi

echo "SemVer check OK — no unacknowledged breaking API changes."
