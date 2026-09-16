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
  extensive bisection that ended **undetermined**, and flagged a
  σ≈1,080-1,527ms per-checkpoint noise floor — measured on GitHub Actions'
  `ubuntu-latest` runner, for the full end-to-end cold-start build. This
  report initially (wrongly) treated that figure as its own verdict's
  threshold too; per the correction in **📊 Assay**, this sandbox's own noise
  turned out far coarser, and this report's verdict is judged against noise
  actually measured on this box, not that borrowed figure.
- Issue #2795 (2026-09-14, still open): names `autumn-web`'s own source as the
  new largest unit (43-55s) and explicitly suggests looking for internally
  modular-but-ungated subsystems (job scheduling, ledger, admin panel,
  sim/chaos testing utilities).

No open issue or PR has yet profiled a specific `autumn-web` module against
this gate since #2795 was filed.

## 💡 Hypothesis

`autumn/src/test.rs` + `test_html.rs` (6,478 lines combined —
`autumn_web::test::{TestApp, TestClient, TestResponse, ...}`, this crate's
own first-party integration-testing harness, plus the dependency-free HTML
parser backing its structural assertions) and `autumn/src/sim.rs` + its 8
submodules (`assert,chaos,crash,fault,llm,op,substrate,sweep` — 5,859 lines
combined — the deterministic simulation/chaos framework) are declared as
plain `pub mod` in `autumn/src/lib.rs` with **no**
`#[cfg(feature = ...)]` gate at the `lib.rs` level — unlike `system_test`,
`plugin_sandbox`, `system_info`, `seed`, `stories`, and `inbound_mail`, which
already are gated there. That means the full 12,337-line pair is part of
every build that enables every one of *their own internal* features too
(`sqlite`, `sim-testing`) — but three of the eight sim submodules gate
themselves individually, one level down, inside `sim.rs`: `substrate.rs`
(336 lines) needs `sqlite`, `op.rs` (454) and `sweep.rs` (396) need
`sim-testing` — 1,186 whole-file lines neither the no-DB daemon build
measured below nor its default-feature counterpart pulls in at all. Three
files' worth of that gating was checked and subtracted; **11,151 lines is
that subtraction, not a claim that every remaining line compiles** — both
`test.rs` and the sim submodules that remain (`sim.rs` itself, `chaos`,
`fault`, `llm`, `crash`, `assert`) contain their own further internal
`#[cfg(test)]` sections and feature-gated branches (on `db`/`mail`/`ws`/etc.)
that this whole-file `wc -l` count does not exclude either — getting the
exact configured-source figure would need a real per-line cfg resolution
(effectively asking rustc, not `wc -l`), which this report does not attempt.
Read every line count in this report as a **whole-file physical-line total,
an upper bound on what's compiled, not an exact compiled-line count** — the
qualitative point (a lot of unconditionally-reachable test-only source with
no top-level feature gate) holds regardless of exactly how many of those
lines a real preprocessor would keep. `test.rs`'s own module doc says
exactly what it is ("First-party integration-testing utilities for Autumn
applications... Import it in your integration tests").

The two files are mutually and directly coupled through real (non-doc) code —
not just intra-doc links — e.g. `sim.rs`:

```rust
pub fn build(&mut self, app: crate::test::TestApp) -> &crate::test::TestClient
```

and `test.rs`:

```rust
fault_plan: Option<crate::sim::fault::FaultPlan>,
```

— so any gate has to cover both together as one unit. This repo already has a
feature named for the job, `test-support` (`Cargo.toml`, `dep:testcontainers`
etc.), used for other test-only code and wired into *some* CI invocations
that specifically need it (the Docker sweep's `--features "test-support,
offline-sync"`). Whether it's actually *sufficient* — i.e. whether routing
`test`/`sim` through it would work everywhere those modules are used — turned
out to be the wrong question to answer by inspection; see the escalating
correction under **Mechanism proposed** below.

Mechanism proposed, then substantially walked back over three rounds of
review (each round is left below rather than silently smoothed over, because
the walk-back is itself the finding): gate `pub mod test;` / `mod test_html;`
/ `pub mod sim;` behind `#[cfg(feature = "test-support")]`.

**Round 1 (this report's original claim): additive-safe for generated
projects.** Cargo unifies dev-dependency-only feature requests per target
kind (a `[dev-dependencies] autumn-web = { features = ["test-support"] }`
entry activates `test-support` only for test/bench/example targets, never the
plain `cargo build`/`autumn dev` binary a production deploy runs), and
`autumn-cli/src/starters/{cms,saas}/Cargo.toml.tmpl` already declare exactly
that. True, but incomplete, as the next two rounds found.

**Round 2 (review correction): the base and `--api` `autumn new` templates
don't do this.** `autumn-cli/src/templates/Cargo.toml.tmpl` and
`Cargo.api.toml.tmpl` list only `tokio` under `[dev-dependencies]`, yet their
generated `tests/integration_test.rs.tmpl` already does `use
autumn_web::test::TestApp;`. Shipping the gate without updating those two
templates too would break `cargo test` for every brand-new project generated
by the plain/`--api` shapes — a bigger blast radius than first stated, but
still just more templates to update.

**Round 3 (review correction): `autumn/src/bin/sim_sweep.rs`** imports
`autumn_web::sim::sweep` from outside the crate (a `crate::`-scoped grep
inside `autumn/src/` can't see a `use autumn_web::` import in a sibling `bin`
target) and is gated only by `sim-testing`, which `.github/workflows/ci.yml`'s
"Sim sweep (sim-testing)" job already runs **without** `test-support`. Still
containable — `sim-testing` could be made to imply `test-support`.

**Round 4 (found composing this section; corrected again below after
review): `autumn-web`'s own test suite is a much bigger consumer than the
templates or `sim-sweep`.** The dev-dependency-unification trick only ever
helps an *external consumer* of `autumn-web` — it does nothing for
`autumn-web`'s own test suite, which cannot "dev-depend on itself" to get a
different feature set for its own `cargo test` than its own `cargo build`.
`grep -rl 'autumn_web::test\b' autumn/tests/` turns up **123** files under
`autumn/tests/integration/` alone (the consolidated `integration_tests`
binary), plus `crate::test::TestApp` inside `#[cfg(test)]` unit-test modules
scattered through the crate's own source (e.g. `webhook_outbound.rs:1388`).
`.github/workflows/ci.yml`'s `test` job — the one that runs on all three OSes
and gates every PR — invokes exactly `cargo test --workspace -- --skip
compile_fail:: --skip sim_` (line 786), with **no `--features` at all**, i.e.
default features only, which do not include `test-support`. So does
`AGENTS.md`'s own documented `cargo test -p <pkg>`.

**Round 5 (review correction): that's a real gap, but not the 123-file one
this report first claimed.** `--features <crate>/<feature>` (e.g. `cargo test
--workspace --features autumn-web/test-support`) activates a feature for
every target that single invocation builds — none of the 123 importing files
would need editing individually; Cargo features are activated at the
invocation/manifest level, not per call site. So the real remaining work is
narrower than "100+ test files": update `ci.yml`'s `test` job invocation
(line 786) and `AGENTS.md`'s documented `cargo test -p <pkg>`/`cargo test
--workspace` commands to add `--features autumn-web/test-support` (or
equivalent), on top of the two generator templates (Round 2) and the
`sim-sweep` bin's `sim-testing` gate (Round 3) already found. That is a real,
coordinated, multi-file change across CI config, a contributor-facing doc,
and generator templates — not something to fold into this report as a
side-effect — but it is *not* the "no clean fix exists" claim an earlier
draft of this paragraph made.

**Round 6 (review correction): rustdoc/docs.rs.** `autumn/Cargo.toml`'s
`[package.metadata.docs.rs]` feature list deliberately **excludes**
`test-support` (comment: *"only meaningful in test binary contexts"*), yet
several always-compiled public doc comments link into the modules this
report proposes gating — e.g. `lib.rs:1555`'s
`` [`crate::test::TestResponse::assert_max_queries`] ``, plus links in
`entropy.rs` and `state.rs`. `scripts/check-docs.sh` builds exactly that
docs.rs feature posture with `-D rustdoc::broken_intra_doc_links`. Gating
`test`/`sim` would turn every one of those into a broken intra-doc link and
fail the docs gate, on top of dropping both modules' public API pages
entirely from published docs.

**Round 7 (review correction): the lint gate and the benches.**
`.github/workflows/ci.yml` runs `cargo clippy --workspace --all-targets -- -D
warnings` — the documented local lint command in `AGENTS.md` too — and
`autumn/benches/{request_pipeline,csrf_verify,captcha_check,throttle_check}.rs`
import `autumn_web::test::TestApp` with no `required-features` on their
`[[bench]]` entries. `--all-targets` compiles benches, so this would fail the
lint gate as well.

**Bottom line on the mechanism**, independent of the verdict below: what
looked, at first read of `test.rs`'s module doc and one existing feature
flag, like an obviously-available and already-proven fix keeps turning out
not to be, and — after seven rounds of finding a new required touch point
roughly every time this section was re-read (five from outside review, two
self-corrections) — this report stops treating its own enumeration as
complete. Every round found the *same shape* of gap: some Cargo target kind
(a generator template, a `[[bin]]`, the primary test invocation, docs.rs,
now benches/clippy) uses `test`/`sim` unconditionally and isn't yet
accounted for. There is no reason to believe examples, doc-tests, or the
`autumn-cli`/other workspace crates' own test suites are clean either — they
were never systematically checked, only whatever review happened to name.
**The actual prerequisite for ever attempting this gate is a full audit
across every Cargo target kind** (lib, every `[[bin]]`, every `[[test]]`,
every `[[bench]]`, doc-tests, the docs.rs feature list, and every workspace
member that depends on `autumn-web`) for `test`/`sim` usage — not another
round of ad hoc discovery, and not this report. None of the seven gaps found
so far is individually a blocker — each has a known fix — but the pattern
across all seven is the finding: **this was never the additive, one-line,
low-risk gate its first description claimed.** Moot either way given the
timing verdict below (there is no compile-time win to chase), but recorded
in full because the mechanism's actual scope, not just this report's
numbers, is worth getting right before anyone tries it.

**Falsifiable question:** does removing the ~11,151 whole-file lines that
apply under the no-DB daemon feature set (`maud,htmx,tailwind,reporting` —
`DAEMON_NO_DB_FEATURES`, `autumn-cli/src/new.rs`) produce a measurable
compile-time reduction for that build?

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

Removing the ~11,151 whole-file lines of `test.rs`/`test_html.rs`/`sim.rs`
(and its 8 submodules, 3 of which — `substrate`/`op`/`sweep`, 1,186 lines —
were never compiled into this particular build to begin with, and the
remainder still an upper-bound whole-file count, not an exact configured-
source figure — see the correction in **💡 Hypothesis**) — the largest
whole-file, unambiguously test-only source with no top-level feature gate in
`autumn-web` for this feature set, confirmed compile-clean to remove **for
the `--lib`
target** (the measurement's own build target, so the timing numbers below are
unaffected — but see the `sim-sweep` `[[bin]]` gap noted in 🧪 Apparatus,
which means the *module itself* isn't as cleanly severable as this report
first claimed) — produced **no measurable, directional change** to the
crate's own compile time on this box across 5 runs per condition, 2 of them
interleaved specifically to rule out ordering/drift effects. The hypothesis
that these modules are a meaningful contributor to the ~43-55s issue #2795
measured is **not supported**.

**Correction, second round (review):** an earlier draft of this paragraph
claimed the result "rules out a large contributor." On reflection (and per
review) that overstates what 3 samples per condition, one of them a
~15-20s outlier, can support: a standard deviation or a median is not a hard
exclusion bound, and this apparatus's own noise is large enough that a real
effect of several seconds — which is smaller than the swing either condition
showed on its own across runs — could not be reliably distinguished from
that noise at this sample size, in either direction. The honest statement is
narrower: **this apparatus did not detect an effect**, the medians show no
consistent direction (if anything, slightly opposite the hypothesis), and
resolving whether a smaller real effect exists needs a lower-noise
environment (CI's own gate, with its established and much smaller noise
floor, or a dedicated non-shared box) and more than 3 samples per condition
— not a stronger claim from the data already in hand. It does not change the
recommendation: this is not where the next round of #2795 should spend its
time chasing a *large* effect (issue #2795's own numbers, 43-55s attributed
to `autumn-web`'s hand-written source, would require one), but this specific
apparatus is the wrong tool to go looking for a small one here.

This does **not** ship a code change — reverted the local experiment,
nothing committed to `autumn/src/lib.rs`. Per Onramp's impact floor, a change
that doesn't move the counter doesn't ship, so filing this as a negative
result (with the ruled-out hypothesis and the measurement) rather than a PR.

**What this does leave for #2795's next attempt:** filtered to modules that
actually compile under `DAEMON_NO_DB_FEATURES` (`maud,htmx,tailwind,
reporting`) — **not** `mail.rs`, `db.rs`, or `migrate.rs`, each gated behind
`#[cfg(feature = "mail")]`/`#[cfg(feature = "db")]` in `lib.rs` and so absent
from this build entirely, a wrong candidate an earlier draft of this list
included — the honest remaining candidates by line count are `job.rs`
(21,003), `app.rs` (19,695), `config.rs` (19,686), `router.rs` (15,224),
`widgets.rs` (8,882), `actuator.rs` (9,006), `form.rs` (7,662), and `auth.rs`
(5,596), all confirmed unconditional `pub mod` declarations. But a further pass
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
