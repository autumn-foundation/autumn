# 🛣️ Onramp: dev-profile debug info is a real cold-start lever, but it collides with documented backtrace behavior (findings, needs a human decision)

## 🎯 Journey

**First run** (`autumn new` → `cargo build` → first HTTP 200), the no-DB daemon
shape the `Cold-Start Onboarding Gate` (`.github/workflows/cold-start-latency.yml`)
measures — the same highest-weighted journey the two prior reports on issue
#2795 investigated (`docs/reports/2026-09-02-prospect-cold-start-db-gate-verify.md`,
`docs/reports/2026-09-03-prospect-cold-start-post-fix-bisect.md`) and the most
recent follow-up closed as a negative result
(`docs/reports/2026-09-16-onramp-test-sim-compile-gate-negative-result.md`, PR
#2817).

Reproduce: see **🔬 Reproduce** below.

## 📈 Evidence / Prior art

Issue #2795 (open) names `autumn-web`'s own hand-written source as the current
bottleneck (~43-55s of the no-DB daemon build) and, after PR #2817's negative
result on gating `test`/`sim`, its own "what this leaves for #2795's next
attempt" section calls for `-Z self-profile` plus a query-time summarizer to
get real per-item attribution inside the ~22s "frontend" (type-check/MIR/
borrowck) phase, rather than another line-count-driven module-removal guess.

This report attempted exactly that tool and could not run it in this
environment: `-Z self-profile` needs the `measureme` crate's `summarize`/`crox`
tool to decode its binary trace format, and this sandbox's egress policy
returns `403` for `crates.io` (`curl -sS -o /dev/null -w '%{http_code}' https://crates.io` →
`403`), so `cargo install` cannot fetch it, and no pre-installed copy exists.
`rustup toolchain list` also shows only `stable`; a full nightly is not
installed either, though that turned out not to matter — `-Z` flags work on
the installed stable `rustc 1.94.1` via `RUSTC_BOOTSTRAP=1`, the trace
*decoding* step is what's actually blocked. Recorded here so the next attempt
doesn't re-discover the same dead end: **`-Z self-profile` needs network
access to `crates.io` (or a vendored copy of `measureme`) to be useful in this
sandbox; it is not simply a compiler flag away.**

Falling back to `-Z time-passes` (also stable-compatible via
`RUSTC_BOOTSTRAP=1`, no decoder needed — it prints per-pass wall time directly
to stderr) gave real, if coarser, signal on one run of
`cargo build -p autumn-web --no-default-features --features
maud,htmx,tailwind,reporting`:

| pass | time |
|---|---|
| `LLVM_passes` | 12.75s |
| `codegen_crate` | 11.86s |
| `codegen_to_LLVM_IR` | 11.45s |
| `MIR_borrow_checking` | 9.86s |
| `generate_crate_metadata` | 8.73s |
| `type_check_crate` | 8.66s |
| `monomorphization_collector_graph_walk` | 6.29s |
| `coherence_checking` | 3.34s |
| `macro_expand_crate` | 3.86s |

(total unit time 46.15s on this run). This contradicts the framing in #2795
and PR #2817 that frontend (type-check/borrowck) time dominates over codegen
for this crate: backend work (`LLVM_passes` + `codegen_crate` +
`codegen_to_LLVM_IR` + `monomorphization_collector_graph_walk` ≈ 42.4s of
overlapping/parallel work) is at least as large as frontend work
(`type_check_crate` + `MIR_borrow_checking` + `coherence_checking` +
`macro_expand_crate` ≈ 25.7s) on this box, for this feature set. That
redirected this report toward the backend/codegen side rather than more
per-module frontend attribution, since `-Z self-profile` (frontend-focused,
and blocked anyway) was the wrong tool for that half regardless of network
access.

`LLVM_passes` + debug-info emission inside `codegen_to_LLVM_IR` are both
driven by how much debug metadata rustc asks LLVM to generate, which is
controlled by `-C debuginfo` (Cargo's `[profile.dev] debug` key). Neither this
workspace's root `Cargo.toml` nor `autumn-cli`'s generated-project templates
(`autumn-cli/src/templates/Cargo.toml.tmpl`, `Cargo.api.toml.tmpl`) set
`[profile.dev]` at all, so every `cargo build` — the real gate's included —
runs at Cargo's own default, `debug = true` (`-C debuginfo=2`, full debug
info: line tables plus variable/type info for a real debugger).

## 💡 Hypothesis

Full debug info (`-C debuginfo=2`) is measurably more expensive for LLVM to
emit than reduced levels, and since the no-DB daemon template sets no
`[profile.dev]` override, every `autumn new` project — including the one
`cold-start-latency.yml` scaffolds — pays the most expensive level by default
purely because nothing ever chose a cheaper one.

**Falsifiable question:** does lowering `-C debuginfo` for `autumn-web`'s own
build reduce wall-clock time by an amount that would matter for the cold-start
gate?

## 🧪 Apparatus

Same box as this session (4 vCPU, rustc/cargo 1.94.1). Built
`cargo build -p autumn-web --no-default-features --features
maud,htmx,tailwind,reporting` (same feature set `DAEMON_NO_DB_FEATURES` maps
to, per the same caveat the 2026-09-16 report already recorded: this omits
the `flash` feature the real generated-project template's `default = ["flash"]`
carries, and builds `-p autumn-web` directly rather than the actual scaffolded
project `cold_start_driver.rs` builds — so absolute numbers here are not
directly comparable to the gate's own reported figures, only the *relative*
delta between conditions is). `CARGO_INCREMENTAL=0`, fully-cold `autumn-web`
artifacts every run (`target/debug/deps/libautumn_web*`,
`.fingerprint/autumn-web-*`, `incremental/autumn_web-*` removed before each
build), dependencies left warm. Wall-clock via `date +%s.%N` around the
`cargo build` invocation (this sandbox has no `/usr/bin/time`).

Three conditions: **baseline** (no `RUSTFLAGS`, i.e. Cargo's default
`debug = true` / `-C debuginfo=2`), **debuginfo=1** (`RUSTFLAGS="-C
debuginfo=1"`, line-tables-only — keeps file:line resolution for backtraces
and panics, drops variable/type info), **debuginfo=0** (`RUSTFLAGS="-C
debuginfo=0"`, no debug info at all).

Each condition's first run in its own block was consistently ~1.7-2x its
later runs (baseline 79.66s vs. 40-42s; debuginfo=1 76.81s vs. 36.9-37.2s;
debuginfo=0 67.04s vs. 32.3-33.6s) — the same kind of box-noise/warm-up
outlier the 2026-09-16 report flagged, not a per-condition effect (it hit
every condition's first run alike). Excluded from the medians below. Two
further baseline/debuginfo=0 runs were interleaved (not batched) specifically
to check for an ordering confound, per the methodology lesson the 2026-09-16
report drew after finding one; direction and magnitude held.

## 📊 Assay

Wall clock, steady-state runs only (first run of each batched block excluded
as a warm-up outlier; see 🧪 Apparatus):

| Condition | runs (s) | median |
|---|---|---|
| baseline (`debug=true`, current default) | 40.03, 41.60 (batched) · 41.09, 38.97 (interleaved) | 40.56 |
| `-C debuginfo=1` (line-tables-only) | 36.88, 37.23 (batched) | 37.05 |
| `-C debuginfo=0` (none) | 32.30, 32.72 (batched) · 33.63, 33.56 (interleaved) | 33.14 |

Pooling all four baseline samples against all four debuginfo=0 samples (the
only pair run both batched and interleaved): median 40.56s → 33.14s, a
**18.3% reduction**. The batched-only pair alone reads closer to 20-21%; the
interleaved pair alone reads 15-18%. Read the honest number as ~18%, not the
more flattering batched-only slice — this report is not going to repeat the
2026-09-16 report's own first-draft mistake of citing the number that looks
best.

`-C debuginfo=1` alone reduces time by ~8.7% — real, but well short of the
20% floor on its own.

## 🔧 Why this is a findings report, not a fix PR

`-C debuginfo=0`'s ~18% reduction is close to, but on the honest pooled
number, just under, Onramp's own 20% impact-floor line — and line-count noise
this small on a shared 4-vCPU sandbox (the 2026-09-16 report measured
σ≈1.0-1.5s on this same class of box) means the true effect could sit on
either side of 20%. That alone would call for one more round of measurement
before shipping, on a dedicated or CI-caliber box rather than this shared
sandbox.

But the bigger reason this isn't a change to ship autonomously: **Autumn's
own documented error-reporting behavior depends on debug info being present.**
`docs/guide/error-reporting.md` documents that `panic.backtrace` (via
`RUST_BACKTRACE=1`) is a first-class field of the framework's structured error
payload — this is not an incidental capability, it's advertised developer-
facing behavior. Backtrace symbolication (resolving each frame to a
file:line) depends on `-C debuginfo` being at least line-tables-only;
`-C debuginfo=0` would visibly degrade the exact debugging signal that guide
documents Autumn surfacing, for every developer, on every build, forever —
not just the one-time cold-start build the gate measures. (Rust's separate
`#[track_caller]`-based panic-location string — the `"thread panicked at
src/foo.rs:42"` line itself — is unaffected by `-C debuginfo`; what's lost is
the *rest* of the call stack a backtrace shows, which `error-reporting.md`'s
documented `panic.backtrace` field is specifically about.)

That is squarely a value trade-off — compile speed vs. a documented debugging
feature — not a mechanical defect fix, so it goes to a human rather than
shipping as a default change. Two additional gaps would need closing before
anyone could safely turn either level into the templates' default regardless
of that decision:

1. **This apparatus measured `-p autumn-web` in isolation**, not the actual
   `autumn new` no-DB daemon project `cold_start_driver.rs` builds (same
   `flash`-feature gap the 2026-09-16 report already flagged for its own
   apparatus) — the real gate's absolute numbers could respond differently.
2. **Only the cold build was measured.** A `[profile.dev]` override in the
   generated-project templates changes every subsequent `cargo build`/`cargo
   run`/`cargo test` too, including the warm, incremental rebuilds
   `dev-loop-latency.yml` and `dev-loop-scaling.yml` gate — not measured here
   at all, and debug-info emission cost does not necessarily scale the same
   way for a small incremental diff as it does for a from-scratch build.

## Decision needed

If a human wants to pursue this: pick a debug-info level for the generated
project templates' `[profile.dev]` (currently unset, so `debug = true`/full):

- `debug = 0` — larger win (~18%, pending re-measurement above the noise
  floor and against the real harness), but removes backtrace file:line
  resolution entirely, directly regressing `error-reporting.md`'s documented
  `panic.backtrace` field.
- `debug = "line-tables-only"` — keeps backtrace resolution intact, but only
  ~8.7% on this apparatus, short of the impact floor on its own.
- Do nothing, and let #2795's next attempt keep looking at the frontend/
  per-module side instead (would need `measureme`/`summarize` either
  pre-installed in CI's runner image or vendored, since `-Z self-profile`'s
  decoder needs `crates.io` access this sandbox doesn't have).

Not a "config option" in Onramp's banned sense (no new user-facing toggle —
this is picking a value for an existing Cargo profile key the templates
already leave at its default), but it is a debuggability trade-off worth a
named decision rather than an autonomous default flip.

## 🔬 Reproduce

```bash
# From the repository root.
# Establish deps are warm (only needs doing once):
cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# baseline (current default: debug = true)
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# -C debuginfo=1 (line-tables-only)
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 RUSTFLAGS="-C debuginfo=1" cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# -C debuginfo=0 (none)
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 RUSTFLAGS="-C debuginfo=0" cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# Time each with `date +%s.%N` before/after (no /usr/bin/time in this sandbox).
# Repeat at least 4x per condition, interleaved rather than batched, before
# trusting a delta this close to the 20% floor.

# -Z time-passes breakdown (stable-compatible, no measureme/summarize needed):
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
RUSTC_BOOTSTRAP=1 CARGO_INCREMENTAL=0 RUSTFLAGS="-Z time-passes" cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting 2>&1 | grep -E '^\s*time:'
```
