# Pre-registration: debuginfo=1 cold-start, n=5 interleaved (re-charter of 2026-09-28 assay)

Committed BEFORE any measurement of this assay. Non-production; apparatus never merges.

**Question:** On the real harness (`autumn dev-loop-bench --cold-start --runs 1`), does
`[profile.dev] debug = 1` in the generated-project Cargo.toml template reduce median
cold-start time by >= 20% versus the current default (no override)?
**Decision / decider:** issue #2795, template debuginfo default; repo maintainer.
**Prior art:** docs/reports/2026-09-28-prospect-cold-start-debuginfo-real-harness-gap1.md
(undetermined: -14.48% registered, n=2/side), 2026-09-21 and Onramp 2026-09-17. Changed
information: this re-charter supplies the n>=5 that report named as the missing input.
Template unchanged on trunk (verified: no `[profile.dev]` in autumn-cli/src/templates/Cargo*.tmpl).

**Lines (fixed now):**
- Pursue: median relative reduction (debug=1 vs baseline, 5 samples each) >= 20% AND
  max(debug=1) < min(baseline) (non-overlap).
- Kill: median reduction < 10%.
- Between 10% and 20%, or overlap: undetermined (honest), no moving the line.
- Secondary (reported, not verdict-bearing): count of samples per condition exceeding the
  gate's p95 budget 130000ms.

**Conditions:** this sandbox (4 vCPU), current trunk-dev tip, two prebuilt `autumn` CLI
binaries (identical except the embedded template), scaffold pins its own toolchain via
rust-toolchain.toml. Samples interleaved B,D,B,D,... (B=baseline, D=debug=1) from one
warmed CARGO_HOME, each in a fresh tempdir/target. All runs reported, including any failed.
**Time box:** this session (~1h). **Riskiest assumption:** that run-to-run variance on this
box (~15% baseline spread per prior report) is small enough for n=5 to separate a 20% effect;
if baseline spread alone exceeds 20% the verdict is undetermined-by-noise.
**Control:** baseline template. **Containment:** template edit only in a scratch copy of the
repo outside the tree; no production data; no spend.
