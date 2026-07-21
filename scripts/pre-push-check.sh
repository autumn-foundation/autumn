#!/usr/bin/env bash
# Pre-push gate: compile the SAME targets CI compiles, COMPILE-ONLY.
#
# Why this exists: CI's blocking gate is `cargo test --workspace` (the `test`
# job in .github/workflows/ci.yml), which compiles every workspace test target
# — including the autumn-web consolidated `integration_tests` binary. A narrower
# local loop like `cargo test -p autumn-cli` never links that binary, so a
# cross-package compile break (e.g. the #1614 sqlite+mail E0308) sails past a
# green local run and only surfaces in CI, where it looks like a "flake." This
# script closes that gap: it builds exactly what CI's always-on `lint` + `test`
# jobs build, so a compile break is caught here instead of on the PR.
#
# Why `--no-run`: `cargo test --workspace --no-run` compiles every test binary
# (catching cross-package breaks in the consolidated integration binary) WITHOUT
# executing them. That deliberately avoids the trybuild suite
# (autumn/tests/integration/compile_fail.rs), whose compile-fail / compile-pass
# cases each spawn a *nested* `cargo build` at test-RUN time — expanding scratch
# by ~17GB and risking ENOSPC. trybuild compiles at run time, not build time, so
# `--no-run` never triggers it. This gate stays disk-cheap by construction.
#
# Why a SEPARATE doctest leg: `--no-run` compiles every *binary* test target but
# does NOT build doctests — doctests are only built during the `--doc` phase. So
# a doctest that stops compiling (e.g. #2107: an app.rs `no_run` example doctest
# broke after a struct gained a field) sails past `--no-run` locally yet fails
# CI's `cargo test --workspace`, which DOES run the doc target. Cargo has no
# stable compile-only doctest mode (`cargo test --doc --no-run` errors "can't
# skip running doc tests with --no-run" on the current toolchain), so the only
# way to catch a doctest break is to actually run them. Fortunately that is
# cheap and infra-free here: almost every doctest is `no_run` or `ignore` (which
# still COMPILE — exactly the #2107 signal), so `cargo test --workspace --doc`
# needs no Postgres/MediaMTX, never hangs, and — because `--doc` selects only the
# doc target — never touches the trybuild integration binaries, so it adds no
# meaningful disk. It reuses the libs the steps above already built.
#
# NOT run here (need Docker / a backend-flip feature / a browser — out of scope
# for a fast, disk-cheap compile-only gate; CI runs them in dedicated jobs):
#   - the Docker/testcontainer `#[ignore]`d sweep (ci.yml "Run Docker-dependent tests")
#   - the `sqlite-runtime` lane (`--features sqlite`, a backend-flip feature)
#   - the `system-tests` (Chromium) browser suite
#
# Usage (runnable from anywhere in the tree):
#   ./scripts/pre-push-check.sh
#
# WARNING: do NOT run a bare `cargo test --workspace` (without `--no-run`) on a
# disk-constrained machine — that RUNS trybuild and expands scratch by ~17GB.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

step() {
  echo
  echo "==> $*"
}

# --- 1. Formatting -----------------------------------------------------------
# Mirrors ci.yml `lint` job: `cargo fmt --all -- --check`.
step "cargo fmt --all -- --check"
cargo fmt --all -- --check

# --- 2. Clippy (whole workspace, all targets) --------------------------------
# Mirrors ci.yml `lint` job: `cargo clippy --workspace --all-targets -- -D warnings`.
# `--all-targets` also compiles examples/benches, so together with the test
# compile below this covers the full set of targets CI builds on every PR.
step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

# --- 3. Compile every workspace test target (compile-only) -------------------
# Mirrors ci.yml `test` job (`cargo test --workspace`), but `--no-run` so it
# compiles — and never executes — every test binary, including the autumn-web
# consolidated `integration_tests` binary that a `-p autumn-cli` loop skips.
# This is the line that catches cross-package compile breaks locally.
step "cargo test --workspace --no-run   (compile-only; skips ~17GB trybuild run)"
cargo test --workspace --no-run

# --- 4. Doctests (workspace, doc target only) --------------------------------
# `--no-run` above skips doctests (they build only in the `--doc` phase), so a
# doctest compile break — like #2107's app.rs example — would pass locally but
# fail CI's `cargo test --workspace`. There's no stable compile-only doctest
# mode, so run them: it mirrors CI's workspace doctest scope, stays infra-free
# (doctests are overwhelmingly no_run/ignore, so nothing hits a DB or hangs),
# and — because `--doc` selects only the doc target — never triggers trybuild,
# so it adds no meaningful disk.
step "cargo test --workspace --doc   (doctests; --no-run above does not build them)"
cargo test --workspace --doc

echo
echo "pre-push-check: OK — workspace test targets compile clean, doctests pass."
echo "note: steps 1-3 are COMPILE-ONLY; step 4 RUNS doctests (cheap: mostly"
echo "      no_run/ignore, no DB, and --doc never triggers the ~17GB trybuild run)."
echo "      A bare \`cargo test --workspace\` (without --no-run / --doc) additionally"
echo "      RUNS trybuild, expanding scratch by ~17GB — avoid it on a small disk."
