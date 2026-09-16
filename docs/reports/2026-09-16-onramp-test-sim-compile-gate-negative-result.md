# 🛣️ Onramp: does gating `test`/`sim` behind `test-support` move the Cold-Start gate? (negative result)

## 🎯 Journey

**First run** (`autumn new` → `cargo build` → first HTTP 200), specifically the
no-DB daemon shape the `Cold-Start Onboarding Gate`
(`.github/workflows/cold-start-latency.yml`) measures. This is the highest-
weighted journey Onramp tracks: it has its own dedicated CI gate, a budget
(`ChangeClass::ColdStartHello` in `autumn-cli/src/dev_loop_bench.rs`), and two
prior investigation reports (issue #2309 and its follow-up, issue #2795)
already establishing that `autumn-web`'s own hand-written source — not its
macros, not its dependencies — is the current bottleneck, at roughly 43-55s of
the total build on the original author's box.

Issue #2795 names the follow-up directly: *"Profile `autumn-web`'s own source
... to find which internal modules dominate ... and whether any are already
conditionally useful only under specific features ... but are not yet
`#[cfg(feature = ...)]`-gated."* This report is exactly that profiling pass,
run against one concrete, plausible-looking candidate.

Reproduce: see **🔬 Reproduce** below.

## 📈 Evidence / Prior art

- Issue #2309 (2026-08-25): root-caused `autumn-macros`' monolithic db codegen
  (~40k lines) as the original bottleneck; fixed in PR #2360.
- `docs/reports/2026-09-02-prospect-cold-start-db-gate-verify.md`: confirmed
  the `autumn-macros` fix is real at the crate level (54.8s → 3.34s) but real
  CI trajectory only moved ~12.5%, establishing the methodology this report
  reuses (fully-cold target artifacts for the crate under test,
  `CARGO_INCREMENTAL=0`, deps left warm, multiple repeats, same-box
  comparison — never a cross-machine percentage).
- `docs/reports/2026-09-03-prospect-cold-start-post-fix-bisect.md`: an
  extensive bisection that ended **undetermined**, and flagged this repo's
  measurement noise floor at σ≈1,080-1,527ms per checkpoint — the number this
  report's verdict is judged against.
- Issue #2795 (2026-09-14, still open): names `autumn-web`'s own source as the
  new largest unit (43-55s) and explicitly suggests looking for internally
  modular-but-ungated subsystems (job scheduling, ledger, admin panel,
  sim/chaos testing utilities).

No open issue or PR has yet profiled a specific `autumn-web` module against
this gate since #2795 was filed.

## 💡 Hypothesis

`autumn/src/test.rs` (5,302 lines — `autumn_web::test::{TestApp, TestClient,
TestResponse, ...}`, this crate's own first-party integration-testing
harness) and `autumn/src/sim.rs` + `autumn/src/sim/{assert,chaos,crash,fault,
llm,op,substrate,sweep}.rs` (7,035 lines — the deterministic simulation/chaos
framework) are declared as plain `pub mod` in `autumn/src/lib.rs` with **no**
`#[cfg(feature = ...)]` gate at all — unlike `system_test`, `plugin_sandbox`,
`system_info`, `seed`, `stories`, and `inbound_mail`, which already are. That
means all 12,337 lines compile into *every* build of `autumn-web`, including a
production binary that runs no tests and drives no `Sim`. `test.rs`'s own
module doc says exactly what it is ("First-party integration-testing
utilities for Autumn applications... Import it in your integration tests").

The two files are mutually and directly coupled through real (non-doc) code —
not just intra-doc links — e.g. `sim.rs`:

```rust
pub fn build(&mut self, app: crate::test::TestApp) -> &crate::test::TestClient
```

and `test.rs`:

```rust
fault_plan: Option<crate::sim::fault::FaultPlan>,
```

— so any gate has to cover both together as one unit. This repo already has
exactly the right feature for the job: `test-support` (`Cargo.toml`,
`dep:testcontainers` etc.), already used the same way for other test-only
code, already wired into CI's test invocations (`--features "test-support,
offline-sync"` in the Docker sweep, `cargo test -p autumn-web --features
test-support`).

Mechanism proposed: gate `pub mod test;` / `mod test_html;` / `pub mod sim;`
behind `#[cfg(feature = "test-support")]`. Verified by inspection this would
be additive-safe for generated projects (not a default-feature change): Cargo
already unifies dev-dependency-only feature requests per target kind (a
`[dev-dependencies] autumn-web = { features = ["test-support"] }` entry in the
generated `Cargo.toml.tmpl` activates `test-support` only when building test/
bench/example targets, never the plain `cargo build`/`autumn dev` binary a
production deploy runs) — so the generated project's own
`tests/integration_test.rs.tmpl`, which already does `use
autumn_web::test::TestApp;`, keeps compiling under `cargo test` without
needing `test-support` in `[dependencies]` at all. **Caveat, not resolved by
this report:** this is still a breaking change for any *already-published*
downstream app upgrading `autumn-web`, whose existing `Cargo.toml` predates
this hypothetical fix and does not yet declare `test-support` in
`[dev-dependencies]` — out of scope for a change actually shipped without
maintainer sign-off ("ask before" on breaking changes), and moot given the
verdict below.

**Falsifiable question:** does removing these 12,337 always-on lines produce
a measurable compile-time reduction for the no-DB daemon feature set
(`maud,htmx,tailwind,reporting` — `DAEMON_NO_DB_FEATURES`,
`autumn-cli/src/new.rs`)?

## 🧪 Apparatus

Same box as this session (4 vCPU / 15GiB, rustc/cargo 1.94.1), same-day,
interleaved runs — never a cross-machine or cross-day percentage. Two
conditions, uncommitted local experiment only (no code shipped):

- **baseline**: `autumn/src/lib.rs` unmodified.
- **gated**: `#[cfg(feature = "onramp-experiment")]` (a scratch, undeclared
  feature — `cfg` on an unknown feature name is a warning, not an error) added
  above `pub mod sim;`, `pub mod test;`, and `mod test_html;`. Verified this
  compiles **clean, zero errors**, with `cargo build -p autumn-web
  --no-default-features --features maud,htmx,tailwind,reporting` — every other
  reference to `crate::test::`/`crate::sim::` found by
  `grep -rn 'crate::test::\|crate::sim::' autumn/src/` outside the pair itself
  is either a rustdoc intra-doc link (`[TestApp](crate::test::TestApp)`,
  inert for `cargo build`) or inside a `#[cfg(test)]` block (stripped before
  name resolution on a non-test build regardless).

Both conditions built with `cargo build -p autumn-web --no-default-features
--features maud,htmx,tailwind,reporting`, `CARGO_INCREMENTAL=0`, dependencies
left warm (only `autumn-web`'s own `target/debug/deps/libautumn_web*`,
`.fingerprint/autumn-web-*`, and `incremental/autumn_web-*` cleared between
every run) — isolating the crate's own recompile cost, the same technique the
2026-09-02 report used for `autumn-macros`. `--timings` additionally captured
the frontend/codegen split cargo attributes to the `autumn-web` unit.

## 📊 Assay

**Wall clock, `cargo build -p autumn-web ...` (3 repeats each, fully-cold
`autumn-web` artifacts every run, deps warm):**

| Condition | run 1 | run 2 | run 3 |
|---|---|---|---|
| baseline (test.rs + sim present) | 38.31s | 35.26s | 35.41s |
| gated off (test.rs + sim removed) | 35.09s | 35.85s | 43.74s |

**`--timings` unit breakdown for the `autumn-web` compilation unit (1 run
each, same clean-artifact protocol):**

| Condition | unit duration | frontend | codegen |
|---|---|---|---|
| baseline | 34.36s | 22.68s | 11.68s |
| gated off | 34.59s | 22.04s | 12.55s |

The two conditions are indistinguishable: every delta above is well inside
this repo's own previously-measured noise floor for this exact gate
(σ≈1,080-1,527ms per checkpoint, 2026-09-03 report) — several deltas here are
*negative* (gated-off slower on one run and on the codegen split), which on
its own rules out a real effect at this sample size in either direction.

## 🏁 Verdict: negative result

Removing all 12,337 lines of `test.rs`/`sim.rs` (and its 7 submodules) —
currently the largest **unconditionally-compiled, unambiguously test-only**
source in `autumn-web`, confirmed compile-clean to remove with zero other
call sites — produced **no measurable change** to the crate's own compile
time on this box. The hypothesis that these modules are a meaningful
contributor to the ~43-55s issue #2795 measured is **not supported**.

Caveat on precision, not a hedge on the conclusion: at roughly 4-5% of
`autumn-web`'s line count, even a perfectly line-proportional contribution
from these modules would sit close to this box's own noise floor, so this
result rules out a **large** contributor but cannot rule out a small one
below single-digit-percent. It does not change the recommendation: this is
not where the next round of #2795 should spend its time.

This does **not** ship a code change — reverted the local experiment,
nothing committed to `autumn/src/lib.rs`. Per Onramp's impact floor, a change
that doesn't move the counter doesn't ship, so filing this as a negative
result (with the ruled-out hypothesis and the measurement) rather than a PR.

**What this does leave for #2795's next attempt:** the always-on core modules
by line count — `job.rs` (21,003), `app.rs` (19,695), `config.rs` (19,686),
`router.rs` (15,224), `widgets.rs` (8,882), `actuator.rs` (9,006), `mail.rs`
(7,948), `form.rs` (7,662), `db.rs` (6,710), `auth.rs` (5,596), `migrate.rs`
(5,762) — are the honest remaining candidates by size, but a further pass
needs real per-module attribution inside the 22s "frontend" (type-check/MIR/
borrowck) phase specifically, since that dominates over codegen here and a
crude line-count/module-removal experiment (this report's own method) is
information-poor at that scale: it would mean editing always-load-bearing
framework internals, not a clean test-only module, so the cheap
`git stash`-and-remeasure trick this report used won't transfer.
`-Z self-profile` (`RUSTC_BOOTSTRAP=1`) plus a query-time summarizer is the
next tool to reach for, not another manual module-removal guess.

## 🔬 Reproduce

```bash
# From a clean checkout of this commit:
cd autumn

# Establish deps are warm (only needs doing once):
cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# --- baseline ---
rm -rf ../target/debug/deps/libautumn_web* ../target/debug/.fingerprint/autumn-web-* ../target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting --timings

# --- gated off: edit src/lib.rs, adding #[cfg(feature = "onramp-experiment")]
#     above `pub mod sim;`, `pub mod test;`, and `mod test_html;` ---
rm -rf ../target/debug/deps/libautumn_web* ../target/debug/.fingerprint/autumn-web-* ../target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting --timings
git checkout -- src/lib.rs

# Compare target/cargo-timings/*.html's embedded UNIT_DATA JSON, "autumn-web" entry,
# `duration` / `sections` (frontend vs codegen), between the two runs.
```
