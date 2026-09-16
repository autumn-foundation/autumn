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
needing `test-support` in `[dependencies]` at all — **and this mechanism is
already proven inside this repo**: `autumn-cli/src/starters/{cms,saas}/
Cargo.toml.tmpl` already declare exactly `[dev-dependencies] autumn-web =
{ version = "...", features = ["test-support"] }`.

**Caveat, corrected from an earlier draft of this report (thanks to review):**
the base `autumn new` template (`autumn-cli/src/templates/Cargo.toml.tmpl`)
and the `--api` template (`Cargo.api.toml.tmpl`) do **not** do this — their
`[dev-dependencies]` lists only `tokio`, yet their generated
`tests/integration_test.rs.tmpl` already does `use
autumn_web::test::TestApp;`. So shipping this gate without also updating
those two templates would break `cargo test` for **every brand-new project**
generated by the plain/`--api` shapes, not just already-published apps as an
earlier version of this paragraph claimed — a strictly bigger blast radius
than first stated. The mechanism is still real (cms/saas already demonstrate
it works), but a real fix needs the two missing templates updated *in the
same change*, plus the already-published-app breakage above, which template
edits can't fix retroactively. Moot either way given the verdict below, but
recorded accurately for whoever revisits this.

**Falsifiable question:** does removing these 12,337 always-on lines produce
a measurable compile-time reduction for the no-DB daemon feature set
(`maud,htmx,tailwind,reporting` — `DAEMON_NO_DB_FEATURES`,
`autumn-cli/src/new.rs`)?

## 🧪 Apparatus

Same box as this session (4 vCPU / 15GiB, rustc/cargo 1.94.1), same-day —
never a cross-machine or cross-day percentage. The first 3 samples per
condition were run batched (all baseline, then all gated); runs 4-5 per
condition were run interleaved (baseline, gated, baseline, gated) specifically
to rule out an ordering/drift confound after an outlier appeared — see
**📊 Assay**. Two conditions, uncommitted local experiment only (no code
shipped):

- **baseline**: `autumn/src/lib.rs` unmodified.
- **gated**: `#[cfg(feature = "onramp-experiment")]` (a scratch, undeclared
  feature — `cfg` on an unknown feature name is a warning, not an error) added
  above `pub mod sim;`, `pub mod test;`, and `mod test_html;`. Verified this
  compiles **clean, zero errors, for the `--lib` target**, with `cargo build
  -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting`
  — every other reference to `crate::test::`/`crate::sim::` found by
  `grep -rn 'crate::test::\|crate::sim::' autumn/src/` outside the pair itself
  is either a rustdoc intra-doc link (`[TestApp](crate::test::TestApp)`,
  inert for `cargo build`) or inside a `#[cfg(test)]` block (stripped before
  name resolution on a non-test build regardless). **Not checked, and a real
  gap found on review:** the crate's `[[bin]]` targets. `autumn/src/bin/
  sim_sweep.rs` (`sim-sweep`, `required-features = ["sim-testing"]`) imports
  `autumn_web::sim::sweep` from outside the crate (so a `crate::`-scoped grep
  inside `autumn/src/` can't see it), and `.github/workflows/ci.yml`'s
  "Sim sweep (sim-testing)" job runs it with `--features sim-testing` and no
  `test-support`. Gating `sim` behind `test-support` as proposed would break
  that existing CI lane outright unless `sim-testing` is also made to imply
  `test-support` (or the CI invocation adds it) — a real, uncounted item in
  the mechanism's blast radius, on top of the two generator templates already
  corrected above. This gate is only ever discussed as a proposal in this
  report, never shipped, but the proposal's stated scope was still incomplete
  as written.

Both conditions built with `cargo build -p autumn-web --no-default-features
--features maud,htmx,tailwind,reporting`, `CARGO_INCREMENTAL=0`, dependencies
left warm (only `autumn-web`'s own `target/debug/deps/libautumn_web*`,
`.fingerprint/autumn-web-*`, and `incremental/autumn_web-*` cleared between
every run) — isolating the crate's own recompile cost, the same technique the
2026-09-02 report used for `autumn-macros`. `--timings` additionally captured
the frontend/codegen split cargo attributes to the `autumn-web` unit.

## 📊 Assay

**Wall clock, `cargo build -p autumn-web ...` (fully-cold `autumn-web`
artifacts every run, deps warm):**

| Condition | run 1 | run 2 | run 3 | run 4 | run 5 |
|---|---|---|---|---|---|
| baseline (test.rs + sim present) | 38.31s | 35.26s | 35.41s | 59.92s | 38.66s |
| gated off (test.rs + sim removed) | 35.09s | 35.85s | 43.74s | 39.53s | 39.31s |

**`--timings` unit duration for the `autumn-web` compilation unit (same
clean-artifact protocol; this is the more precise number — it excludes
cargo's own resolve/startup overhead):**

| Condition | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| baseline | 34.36s | 53.98s | 38.23s | 38.23s |
| gated off | 34.59s | 39.15s | 38.92s | 38.92s |

**Correction from an earlier draft of this report** (thanks to review): the
first-drafted claim that "every delta is well inside σ≈1,080-1,527ms" was
wrong on its face — the original run's *gated* wall-clock sample of 43.74s
(vs. its own two ~35s neighbors) is already a ~8-9s outlier, and this
follow-up's *baseline* `--timings` sample of 53.98s (vs. its own ~34-38s
neighbors) is a ~15-20s outlier — both an order of magnitude past that
figure. That cited noise floor came from GitHub Actions' `ubuntu-latest`
runner in the 2026-09-03 report, measuring a different thing (3-sample p95 of
the *full* end-to-end cold-start build) — it was never this sandbox's own
noise floor, and citing it as if it were was the error, not the underlying
comparison.

Two follow-up interleaved runs per condition (baseline/gated alternated, not
batched) were added specifically to chase this down. The result: an outlier
of this size shows up on **both** conditions across the two sessions (once on
*gated*, once on *baseline*) — i.e. it moves with wall-clock time, not with
which condition ran, consistent with transient contention on this shared
multi-tenant sandbox (4 vCPU, no dedicated hardware) rather than anything
driven by the code change under test. Using the median of the 3 `--timings`
samples per condition (robust to exactly this kind of single-sample spike)
instead of the mean: baseline 38.23s vs. gated 38.92s — gated is marginally
*slower*, the opposite direction from the hypothesis, by an amount smaller
than the swing either condition shows on its own across runs.

## 🏁 Verdict: negative result

Removing all 12,337 lines of `test.rs`/`sim.rs` (and its 7 submodules) —
currently the largest **unconditionally-compiled, unambiguously test-only**
source in `autumn-web`, confirmed compile-clean to remove **for the `--lib`
target** (the measurement's own build target, so the timing numbers below are
unaffected — but see the `sim-sweep` `[[bin]]` gap noted in 🧪 Apparatus,
which means the *module itself* isn't as cleanly severable as this report
first claimed) — produced **no measurable, directional change** to the
crate's own compile time on this box across 5 runs per condition, 2 of them
interleaved specifically to rule out ordering/drift effects. The hypothesis
that these modules are a meaningful contributor to the ~43-55s issue #2795
measured is **not supported**.

Caveat on precision, not a hedge on the conclusion: this sandbox's own
measured run-to-run noise (up to ~20s on a single sample) is far coarser than
the GitHub Actions noise floor an earlier draft of this report mistakenly
borrowed, and at roughly 4-5% of `autumn-web`'s line count, even a perfectly
line-proportional contribution from these modules would be invisible under
noise this size. This result rules out a **large** contributor (the median
comparison shows no directional effect at all, let alone one of the
magnitude issue #2795's numbers would require) but cannot rule out a small
one. It does not change the recommendation: this is not where the next round
of #2795 should spend its time, and — per the correction above — nor is a
same-box wall-clock/`--timings` comparison on this particular sandbox a
precise-enough instrument to chase a small effect further; CI's own gate
(with its established, smaller noise floor) or a lower-noise box would be
needed for that.

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
# From the repository ROOT (the directory holding the workspace Cargo.toml
# and the `autumn/` package subdirectory) — every command below stays at this
# root and names paths explicitly, so there is no ambiguity between the repo
# root and the `autumn/` package dir it contains (both are plausible targets
# of a bare `cd autumn` from a checkout's parent directory; this reproduction
# never relies on that cd at all):

# Establish deps are warm (only needs doing once):
cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting

# --- baseline ---
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting --timings

# --- gated off: edit autumn/src/lib.rs, adding #[cfg(feature = "onramp-experiment")]
#     above `pub mod sim;`, `pub mod test;`, and `mod test_html;` ---
rm -rf target/debug/deps/libautumn_web* target/debug/.fingerprint/autumn-web-* target/debug/incremental/autumn_web-*
CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,reporting --timings
git checkout -- autumn/src/lib.rs

# Compare target/cargo-timings/*.html's embedded UNIT_DATA JSON, "autumn-web" entry,
# `duration` / `sections` (frontend vs codegen), between the two runs. Repeat
# each condition at least 3x, interleaved (not batched), before trusting a
# single-run delta on a shared/noisy box — see the outlier discussion in
# 📊 Assay above.
```
