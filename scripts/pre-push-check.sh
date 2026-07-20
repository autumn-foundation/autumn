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

echo
echo "pre-push-check: OK — workspace test targets compile clean (compile-only)."
echo "note: this gate is COMPILE-ONLY. A full \`cargo test --workspace\` (without"
echo "      --no-run) additionally RUNS trybuild, expanding scratch by ~17GB."
