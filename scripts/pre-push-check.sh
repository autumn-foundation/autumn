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
# jobs build, so a compile break is caught here instead of on the PR. That
# includes the `lint` job's two #1611 panic-gate steps — the manifest checker
# and the gated-features clippy run — which a default-feature local loop misses
# for exactly the same reason.
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
#   - the `sqlite-runtime` lane (`--features sqlite`, a backend-flip feature).
#     Note what that costs since #1905: that lane now runs a BARE `--lib`, so a
#     fixture that only panics under the flip (an inline `postgres://` target,
#     say) is invisible here and surfaces on the PR. Reproduce it with
#     `cargo test -p autumn-web --features sqlite --lib` when touching a test
#     fixture that names a database target.
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

# --- 1. Request-path panic gate (#1611) --------------------------------------
# Mirrors ci.yml `lint` job: `./scripts/check-panic-gate.sh`. Deliberately
# FIRST: it needs no toolchain and finishes in a couple of seconds, so a
# dropped gate header, a manifest drift, or a module-wide `allow`/`expect` spoof
# is reported before the multi-minute compile legs below start. The script runs
# its own `--self-test` first, so a checker that has stopped catching things
# fails here too.
step "./scripts/check-panic-gate.sh   (self-test + manifest gate; no toolchain)"
./scripts/check-panic-gate.sh

# --- 1b. Determinism seam gate (#1797) ---------------------------------------
# Mirrors ci.yml `lint` job: `./scripts/check-determinism-gate.sh`. Same
# rationale as the panic gate above — no toolchain, seconds, self-testing — and
# it catches the class of break the clippy legs cannot report on their own: a
# dropped gate header, an emptied `disallowed-methods` array, a module-wide
# `allow` spoof, or a crate-local `clippy.toml` shadowing the root config.
step "./scripts/check-determinism-gate.sh   (self-test + seam gate; no toolchain)"
./scripts/check-determinism-gate.sh

# ---------------------------------------------------------------------------
# 1c. Plugin API surface gate (issue #1601)
#
# Mirrors ci.yml `migration-guides` job: `./scripts/check-plugin-surface.sh`.
# Same reasoning as the two gates above — seconds, no toolchain, self-testing —
# and it catches the two things a `cargo test -p <one-crate>` loop never will:
# the docs table drifting from `PLUGIN_SURFACES`, and a plugin-surface change
# landing with no "Plugin authors" section in `docs/migrations/next.md`.
# ---------------------------------------------------------------------------
step "./scripts/check-plugin-surface.sh   (self-test + plugin API contract; no toolchain)"
./scripts/check-plugin-surface.sh

# --- 1d. SQLite feature-unification gate (issue #1905) -----------------------
# Mirrors ci.yml `lint` job: `./scripts/check-sqlite-unification.sh`. Same shape
# as the gates above — seconds, no toolchain, self-testing — and it covers the
# one invariant this script otherwise cannot: the legs below never enable
# `sqlite`, so a dependency edge that turns the backend flip on for the whole
# graph would compile here and break the Postgres lane in CI.
step "./scripts/check-sqlite-unification.sh   (self-test + manifest gate; no toolchain)"
./scripts/check-sqlite-unification.sh

# --- 2. Formatting -----------------------------------------------------------
# Mirrors ci.yml `lint` job: `cargo fmt --all -- --check`.
step "cargo fmt --all -- --check"
cargo fmt --all -- --check

# --- 3. Clippy (whole workspace, all targets) --------------------------------
# Mirrors ci.yml `lint` job: `cargo clippy --workspace --all-targets -- -D warnings`.
# `--all-targets` also compiles examples/benches, so together with the test
# compile below this covers the full set of targets CI builds on every PR.
step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

# --- 4. Clippy (autumn-web, gated request-path features) ---------------------
# Mirrors ci.yml `lint` job's second clippy step. The default-feature run above
# never compiles the feature-gated request-path modules (channels→ws,
# mail→mail, inbound_mail→inbound-mail, sync/*→offline-sync,
# session_redis→redis, storage/*→storage), so their
# `#![cfg_attr(not(test), deny(clippy::…))]` panic-gate blocks never fire and an
# unannotated `.unwrap()` / `x + 1` / `&s[1..]` in those production paths sails
# past a local run straight into a red CI.
#
# Feature list kept IDENTICAL to ci.yml's — `check-panic-gate.sh` verifies every
# gated module's feature is covered by a CI clippy invocation, so the two lists
# drifting apart is a gate failure, not a silent hole.
#
# `--lib` rather than CI's `--all-targets`: the gate is `cfg_attr(not(test), …)`,
# so the lib target is where the denials actually apply, and skipping the test /
# example / bench targets keeps this leg minutes shorter. CI still runs the
# `--all-targets` form.
step "cargo clippy -p autumn-web --features \"<gated request-path set>\" --lib -- -D warnings"
cargo clippy -p autumn-web \
  --features "ws,mail,offline-sync,redis,markdown,inbound-mail,inbound-mailgun,inbound-ses,storage,tls,acme" \
  --lib -- -D warnings

step "cargo clippy -p autumn-web --features \"plugin-sandbox,test-support\" --lib -- -D warnings"
# ci.yml runs `plugin-sandbox` as its own clippy lane rather than folding it into
# the list above, so a `wasmi`-linking build is not forced on every gated-feature
# run. Mirrored here for the same reason the lane above is: a gate you cannot
# reproduce locally is one you find out about on the PR.
cargo clippy -p autumn-web --features "plugin-sandbox,test-support" --lib -- -D warnings

# --- 5. Compile every workspace test target (compile-only) -------------------
# Mirrors ci.yml `test` job (`cargo test --workspace`), but `--no-run` so it
# compiles — and never executes — every test binary, including the autumn-web
# consolidated `integration_tests` binary that a `-p autumn-cli` loop skips.
# This is the line that catches cross-package compile breaks locally.
step "cargo test --workspace --no-run   (compile-only; skips ~17GB trybuild run)"
cargo test --workspace --no-run

# --- 6. Doctests (workspace, doc target only) --------------------------------
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
echo "note: step 1 (panic gate) needs no toolchain; steps 2-5 are COMPILE-ONLY;"
echo "      step 6 RUNS doctests (cheap: mostly"
echo "      no_run/ignore, no DB, and --doc never triggers the ~17GB trybuild run)."
echo "      A bare \`cargo test --workspace\` (without --no-run / --doc) additionally"
echo "      RUNS trybuild, expanding scratch by ~17GB — avoid it on a small disk."
