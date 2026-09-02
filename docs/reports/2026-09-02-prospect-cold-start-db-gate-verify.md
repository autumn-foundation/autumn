# ⛏️ Prospect: Did feature-gating `autumn-macros`' db codegen close the cold-start gate? (kill: 105,371ms vs 60,000ms p95 line, ledger #1)

## 🎯 Question

Issue #2309 named `autumn-macros`' monolithic, always-compiled `repository.rs`/
`model.rs` codegen (~40k of the crate's ~55k lines) as the root cause of the
Cold-Start Onboarding Gate's chronic budget miss (p95 ≤ 60s / max ≤ 90s for
`autumn new → first HTTP 200` on a no-DB "hello" app), and explicitly deferred
a fix as "a large, deliberate refactor" needing its own scoped PR.

PR #2360 (merged 2026-08-28, `651929b`) shipped exactly that refactor: it put
the `#[model]`/`#[repository]` codegen behind a `db` feature on
`autumn-macros`. That feature is default-**on** in `autumn-macros`' own
`Cargo.toml` (`default = ["db"]`, so a direct dependant or `cargo test -p
autumn-macros` still gets the full macro surface) — the no-DB saving is
specific to `autumn-web`, which depends on it with `default-features = false`
and forwards its own `db` feature.

**Falsifiable question:** does the merged fix actually bring the no-DB
cold-start build under budget — or at least close most of the gap — on the
next real scheduled run? **Decision:** whether issue #2309 is resolved and
the gate can be trusted, or whether the compile-time problem needs further
work. **Decider:** repo maintainer (the Cold-Start Onboarding Gate is a
scheduled CI check with no other owner).

## ⚖️ Pre-registration

- **Pursue/close line:** next scheduled run after `651929b` merges shows p95
  materially closer to the 60,000ms budget than the pre-fix baseline
  (120,422ms on 2026-08-24) — i.e., most of the previously-measured
  `autumn-macros` self-compile cost (83.65s per issue #2309; the crate is on
  the serial critical path) shows up as wall-clock savings.
- **Kill line:** the gate still fails by a wide margin, meaning the fix's
  real-world impact was much smaller than the isolated crate-level
  measurement predicted.
- **Conditions:** measured by the existing `Cold-Start Onboarding Gate`
  workflow (`.github/workflows/cold-start-latency.yml`), 3 runs, no-DB
  `autumn new --daemon` shape, `ubuntu-latest` GitHub-hosted runner. The job
  logs show `CARGO_INCREMENTAL=0` in the runtime environment; neither the
  workflow file nor `cold_start_driver::cold_build` sets it directly, but the
  preceding `Swatinem/rust-cache@v2.7.8` step documents exporting it. The
  local control below sets `CARGO_INCREMENTAL=0` explicitly and clears
  `target/debug/incremental/` for the same reason, to avoid crediting either
  measurement with reused incremental work products.
- **Time box:** same day (one investigation session); riskiest assumption
  first — whether the crate-level fix actually reduces end-to-end wall time,
  rather than building anything new.
- **Containment:** read-only GitHub API queries plus a local, uncommitted
  `cargo build -p autumn-macros` timing check in this sandbox; no CI or
  production changes made by this assay.

## 🔍 Prior art

- Issue #2309 (2026-08-25): full root-cause writeup, explicitly declines to
  include a fix, proposes feature-gating as the follow-up direction.
- PR #2360 / commit `651929b` (2026-08-28): implements exactly that —
  `autumn-macros/Cargo.toml` `db = []` feature gating `repository.rs`/
  `model.rs`, `autumn/Cargo.toml` `db = ["autumn-macros/db", …]` forwarding,
  and the no-DB `--daemon` scaffold (`autumn-cli/src/new.rs`,
  `DAEMON_NO_DB_FEATURES`) already omits `db`.
- No open issue or PR revisits whether the merged fix actually worked — this
  gap is what this assay closes.

## 🧪 Apparatus

- GitHub Actions history for `cold-start-latency.yml` (10 scheduled runs,
  weekly, 2026-06-29 → 2026-08-31 — every run since the workflow's creation).
- Job logs for the first post-fix run (33404849157, 2026-08-31) and the
  immediately preceding pre-fix run (32703733611, 2026-08-24).
- One local, uncommitted control measurement in this sandbox: `cargo build -p
  autumn-macros` timed with `--features db` vs `--no-default-features`, deps
  pre-warmed, `CARGO_INCREMENTAL=0`, with the `autumn-macros` unit's
  fingerprint *and* incremental work products cleared between runs — isolates
  the crate's self-compile cost the same way issue #2309 did.
- No stubs — this is a verification assay over existing CI history and a
  crate-level micro-benchmark, not a new prototype.

## 📊 Assay

**CI, full app build, no-DB `autumn new --daemon`, 3 runs/day:**

| Run (date, commit) | p50 | p95 | max | vs 60s/90s budget |
|---|---|---|---|---|
| 2026-08-24, `d1ecb361` (pre-fix) | — | 120,422ms | 120,422ms | FAIL (100% over p95) |
| 2026-08-31, `ef61ae44` (post-fix, first run after `651929b`) | 97,482ms | 105,371ms | 105,371ms | FAIL (75% over p95) |

Individual post-fix runs: 105,371 / 95,988 / 97,482 ms. Net change over the
pre-fix baseline: **~12.5%** (120,422 → 105,371ms p95). This is a comparison
across two different commits, a full week apart, on separate GitHub-hosted
runners — not a controlled before/after of the fix alone, so intervening
changes and runner variance are both live confounds; it should be read as
"the net change over that week," not as the fix's isolated impact.

The post-fix run's diagnosis line ("the first clean compile got heavier. A
new default dependency or feature likely bloated the from-scratch build") is
**not evidence of a specific cause** — it is static boilerplate that
`build_diagnostics` (`autumn-cli/src/dev_loop_bench.rs:329-334`) prints for
every `ColdStartHello`/`ColdStartDb` result that exceeds its budget,
regardless of cause. No historical or per-dependency comparison backs it.

**Control — isolated `autumn-macros` self-compile, this sandbox** (4-core,
15GiB, rustc/cargo 1.94.1, deps pre-warmed; `CARGO_INCREMENTAL=0` set
explicitly — matching what `Swatinem/rust-cache`'s documented behavior
exports in the CI workflow — and both the crate's fingerprint *and*
`target/debug/incremental/autumn_macros-*` cleared between the two
measurements, so neither run can reuse the other's rustc work products):

| Build | Self-compile time |
|---|---|
| `cargo build -p autumn-macros --features db` (repository.rs + model.rs included) | 54.80s |
| `cargo build -p autumn-macros --no-default-features` (db codegen excluded) | 3.34s |

The crate-level fix is real and large — a **~94% reduction (54.8s → 3.34s)**
in `autumn-macros`' own compile time, consistent with issue #2309's claim
that `repository.rs`/`model.rs` dominate the crate. Feature wiring was
verified correct by inspection: `autumn/Cargo.toml`'s `db` feature forwards
to `autumn-macros/db`, and `DAEMON_NO_DB_FEATURES` (the no-DB scaffold's
feature list) omits `db`, so the scaffolded app being benchmarked does build
`autumn-macros` with the codegen off.

**Worst case:** the fix's benefit had to survive real GitHub-hosted-runner
variance (weaker/shared cores vs the local sandbox) — it did directionally
(the post-fix run is faster than pre-fix), just far short of what the
crate-level number predicts.

## 🏁 Verdict

**Kill** — against the pre-set line: issue #2309 is **not resolved**. The
Cold-Start Onboarding Gate has now failed all 10 of 10 scheduled runs since
the workflow's creation, including the first run after the targeted fix
merged. p95 105,371ms is still 75% over the 60,000ms budget.

The interesting finding is *why*: the crate-level fix works exactly as
designed (~51.5s saved, verified directly and cleanly, in isolation), yet the
net change in the full end-to-end build over the same week was only ~15s
(120,422 → 105,371ms). That gap is real, but this assay cannot attribute it
to a specific cause — the two CI numbers span a week of unrelated commits and
runner-to-runner variance, and (per the correction above) CI's own diagnosis
line is generic boilerplate, not a finding. The honest statement is: net
improvement was far short of what the isolated fix predicts, and isolating
why requires a controlled comparison this assay did not run — e.g. building
the *same* commit with `autumn-macros` `db` on vs. off end-to-end, or a
`cargo build --timings` bisection over the *full* comparison interval,
`d1ecb361..ef61ae44` (2026-08-24 → 2026-08-31 — the two runs' actual commits,
not a narrower guess). Candidate commits worth checking first, because they
touch default features/deps in that window: `1fd6245` Ledgered entities
(Aug 27), `61bdd9c` Web Push (Aug 27), `fec5221` zero-downtime in-place
upgrades (Aug 29) — unconfirmed, not evidence of cause, and not necessarily
exhaustive over the full interval. That bisection is the follow-up, not this
verification.

No open issue or PR is currently tracking this gap; #2309 remains open and
accurately reflects unresolved state, but nobody has checked the merged fix
against real data since it landed four days before this assay.

## 🔬 Reproduce

```bash
# CI history (requires GitHub API access to autumn-foundation/autumn):
#   workflow: cold-start-latency.yml, runs 32703733611 (pre-fix) and
#   33404849157 (post-fix); job "Measure cold-start onboarding" logs contain
#   the p50/p95/max table.

# Local control (isolates autumn-macros' own compile cost; CARGO_INCREMENTAL=0
# and clearing target/debug/incremental/ matter — see Assay for why):
rm -rf target/debug/deps/libautumn_macros* target/debug/.fingerprint/autumn-macros-* target/debug/incremental/autumn_macros-*
CARGO_INCREMENTAL=0 cargo build -p autumn-macros --features db          # ~54.8s on 4c/15GiB
rm -rf target/debug/deps/libautumn_macros* target/debug/.fingerprint/autumn-macros-* target/debug/incremental/autumn_macros-*
CARGO_INCREMENTAL=0 cargo build -p autumn-macros --no-default-features  # ~3.3s
```
