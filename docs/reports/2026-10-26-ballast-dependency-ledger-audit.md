# ⚓ Ballast: dependency ledger audit, 2026-10-26

Seventh Ballast pass, back on the weekly cadence (prior pass: 2026-10-19).
This pass's headline finding came from the harness itself, not from the
Dependabot queue: re-fetching the RustSec advisory database surfaced a
`cargo-deny` "duplicate items detected" warning that had been sitting
unnoticed in `deny.toml`. Chasing it down turned into a six-entry stale-waiver
cleanup across three policy files. The Dependabot queue was triaged but **no
PR was merged this pass** — the queue's own CI signal turned out to be
unreliable for two independent, pre-existing reasons unrelated to any single
dependency, documented below so the next pass doesn't have to re-derive this.

## 🎯 Class

Policy: stale/duplicate advisory-waiver cleanup across `deny.toml`,
`deny-sqlite.toml`, and `autumn-cli/src/templates/deny.toml.tmpl`
(RUSTSEC-2026-0381). Secondary: Dependabot queue triage (no merges — see
Evidence).

## 📈 Evidence

**The harness surfaced its own debt.** `cargo deny fetch db` followed by
`cargo deny --offline check advisories` on the root `deny.toml` printed:

```
warning: duplicate items detected
    ┌─ deny.toml:66:5
    │  { id = "RUSTSEC-2026-0381", reason = "fixed in the workspace copy (1.0.10); ...; review-by 2026-11-03" },
    ·
121 │  { id = "RUSTSEC-2026-0381", reason = "webpki-roots 0.26.11 only, via sqlx-core 0.8.6 ...; review-by 2026-11-20" },

warning[advisory-not-detected]: advisory was not encountered
    66:13  no crate matched advisory criteria
advisories ok
```

Two separate `[[advisories.ignore]]` entries for the same id, with different
`review-by` dates (2026-11-03 vs. 2026-11-20) — and cargo-deny additionally
reporting that **neither entry's target currently exists in the graph**.
`git blame` traced the second copy to commit `1fb27119` (2026-10-21,
"fix(cli): bind generated auth form labels to inputs (#3372)", an unrelated
PR whose tail end also waived RUSTSEC-2026-0381 for the bundled-pg/sqlite
graphs without noticing the root `deny.toml` already carried an entry for the
exact same residual `webpki-roots` 0.26.11 copy from an earlier pass).

`deny-sqlite.toml` had it **three times** (review-by 2026-11-03, 2026-11-20,
2026-11-04 — two duplicate-items warnings), and the `autumn new --bundled-pg`
scaffold template (`autumn-cli/src/templates/deny.toml.tmpl`) carried a fourth
copy, guarded by a test (`the_advisory_policy_resolves_its_flavor_anchors`)
that asserted it must ship.

**Why the advisory stopped matching.** Pulled the fetched advisory file
directly (`~/.cargo/advisory-dbs/.../webpki-roots/RUSTSEC-2026-0381.md`,
dated 2026-10-19 — one week before this pass):

```toml
[versions]
patched = [">= 1.0.10"]
unaffected = ["= 0.26.11"]
```

> The `webpki-roots` 0.26.11 compatibility package contains no independent
> trust store and re-exports the roots from its separately resolved 1.x
> dependency. That parent dependency must resolve to 1.0.10 or newer; an
> older parent remains affected.

`Cargo.lock` carries both `webpki-roots 0.26.11` *and* `webpki-roots 1.0.10`.
The advisory's own authors updated its metadata (this week) to say the 0.26
shim is unaffected whenever its resolved 1.x parent is patched — which this
workspace's is. So every one of the six ignore entries (2 in `deny.toml`, 3
in `deny-sqlite.toml`, 1 in the scaffold template) was waiving an advisory
that, as of this week's database update, no longer applies to this tree at
all. This is a **forcing fact**, not a style preference: the ignore entries
are not merely redundant text, they are dead weight the advisory database
itself retired.

**Dependabot queue — triaged, zero merges.** 9 open PRs
(`search_pull_requests repo:autumn-foundation/autumn is:open
author:app/dependabot`):

| PR | Title | Type | Disposition |
| --- | --- | --- | --- |
| #3353 | `css-inline` 0.21.2→0.22.1 | minor, security (GHSA-jh54-c7gc-mvcc) | Left open — reachability still undetermined (unchanged from 2026-10-19); CI signal unreliable this pass (see below) |
| #3356/#3355/#3354 | `opentelemetry`/`-sdk`/`-otlp` 0.33.1 | 0.x breaking | Left open — supersede last week's 0.33.0 PRs; not re-diagnosed beyond CI-signal note |
| #3150 | `getrandom` 0.2.17→0.4.3 | major | Left open — no forcing fact |
| #3145 | `wasmi` 0.40.0→2.0.0 | major | Left open — real breakage confirmed 2026-10-05 (E0599/E0603, `autumn-edge/src/host.rs`), no forcing fact |
| #3147 | `croner` 3.0.1→4.0.1 | major | Left open — real breakage per changelog (`CronDateTime` trait, stricter step validation), no forcing fact |
| #3142 | `brotli` 8.0.4→9.0.0 | major | Left open — not re-diagnosed |
| #3144 | `tracing-opentelemetry` 0.33.0→0.34.0 | 0.x breaking | Left open — not re-diagnosed |

**Why no merges: the queue's CI signal is unreliable right now, for two
independent reasons, neither caused by any dependency in it.** Pulled the
actual job log for the "Test suite" (`test-gate`) aggregator on #3144's run
(job 118247420124):

```
##[group]Run results='skipped skipped skipped skipped'
shard results: skipped skipped skipped skipped
##[error]a test shard did not succeed (skipped skipped skipped skipped)
```

Every "Test (${{ matrix.os }})" shard is reported `skipped`, and the
aggregator — which only accepts `success` — fails because of that, not
because any test ran and failed. This is consistent across every Dependabot
PR checked (#3150, #3145, #3147, #3353, #3354/#3355/#3356): the "Test suite"
gate is structurally red on Dependabot-triggered runs (`Secret source:
Dependabot` in the job's own "Prepare workflow directory" group), independent
of what the PR bumps.

Separately, an **"App-variant conformance"** workflow (a different workflow
entirely, not behind the same skip) shows real, multi-minute job runs that
fail — but across unrelated dependency bumps at base commits spanning three
weeks (2026-10-05 croner/wasmi, 2026-10-19 opentelemetry/css-inline,
2026-10-21 wasmi-rebase), with shifting sets of failing variant names and
shifting total shard counts (71, 76, 79, 92, 97 across different PRs' runs —
the matrix itself is growing week over week as app-variant coverage expands).
That spread, across packages with nothing in common, points at `trunk-dev`
itself carrying intermittent or evolving App-variant-conformance breakage,
not at any one Dependabot bump. Confirming that properly (pinning which
`trunk-dev` commits are actually green on this suite) is a CI-health
question outside this pass's scope and budget — flagged as a follow-up
rather than guessed at.

Net effect: for every PR above, the one aggregate signal GitHub shows
("checks failed") cannot be trusted at face value, and the gate-relevant
signals that *do* still mean something — `Feature combination compile gate`
(success on #3144, #3150) and `Fuzz (*)` (success on #3354/#3355/#3356) —
are not a substitute for the full-suite rehearsal the hard gate requires.
Merging any of these on an unverifiable signal would violate "a rehearsal
describes what *did* happen, never what should" — so none were merged.
#3353's upstream fix (GHSA-jh54-c7gc-mvcc, HTML-escaping of `class` attribute
values) is worth carrying into the next batch once reachability is actually
determined or CI health is restored, whichever comes first — noted so next
pass doesn't re-research it.

**License and source checks**: clean, both graphs (`cargo deny check
licenses sources` on root and `deny-sqlite.toml`).

**Duplicate crate versions** (`cargo deny check bans`, warn-level, not
CI-blocking): 91 — unchanged from 2026-10-19 (expected: this pass touched no
dependency versions, only waiver text).

## 💡 Mechanism / forcing fact

The advisory database's own version metadata, updated 2026-10-19, now marks
`webpki-roots` 0.26.11 **unaffected** whenever its resolved 1.x parent is
`>=1.0.10`. This workspace's 1.x copy has been at 1.0.10 since commit
`1fb27119` (2026-10-21). `cargo-deny` confirms zero crates in either graph now
match RUSTSEC-2026-0381. An ignore entry for an advisory that cannot fire is
pure debt: it adds nothing (nothing is being waived), it cost a
"duplicate items" warning that could mask a future *real* duplicate, and the
scaffold copy was shipping an identical dead entry into every
`autumn new --bundled-pg` app. Removing dead weight the moment the evidence
confirms it's dead is exactly the "pin without a trigger is the big-bang
upgrade's down payment" principle run in the other direction — a stale
*removal* trigger (the advisory DB update) fired, so it got acted on now
rather than left for someone to trip over later.

## 🔧 Change

- `deny.toml`: removed both `RUSTSEC-2026-0381` ignore entries (one at the
  original `review-by: 2026-11-03` slot, one added alongside the
  RUSTSEC-2026-0387 waiver at `review-by: 2026-11-20`).
- `deny-sqlite.toml`: removed all three `RUSTSEC-2026-0381` ignore entries
  (`review-by` 2026-11-03, 2026-11-20, 2026-11-04).
- `autumn-cli/src/templates/deny.toml.tmpl`: removed the scaffold's copy
  (inside the `>>> autumn:bundled-pg-waiver` / `<<< ...` anchor block, which
  the generator strips wholesale for non-`--bundled-pg` apps — removing one
  entry from inside it is safe and doesn't touch the anchor mechanism).
- `autumn-cli/src/new.rs`: dropped the `assert_eq!(policy.contains("RUSTSEC-2026-0381"), opts.with_bundled_pg, ...)` branch of
  `the_advisory_policy_resolves_its_flavor_anchors` — the waiver this
  asserted no longer exists in any flavor.
- `autumn-cli/tests/integration/dependency_audit.rs`: dropped the matching
  assertions in `the_scaffold_waives_only_what_its_own_flavor_can_hit` (now
  vacuous — the waiver it checked for leaking is gone entirely) and
  `the_scaffold_policy_covers_the_bundled_postgres_flavor` (asserted the
  waiver must be present; it correctly no longer is).
- `docs/guide/supply-chain.md`: updated the "ships with a waiver already in
  it" paragraph — `--bundled-pg` now gets one extra waiver
  (`RUSTSEC-2024-0384`), not two.
- `changelog.d/3438-deny-stale-webpki-waiver.md`: new fragment (user-visible:
  a freshly scaffolded `--bundled-pg` app's `deny.toml` ships one fewer
  waiver).
- No PR opened against the Dependabot queue this pass; no dependency version
  changed.

## 📊 Measurement

| Check | 2026-10-19 | 2026-10-26 (before) | 2026-10-26 (after) |
| --- | --- | --- | --- |
| `cargo deny --offline check advisories` (root) | ok | ok, 1 duplicate-items + 1 advisory-not-detected warning | **ok, zero warnings** |
| `cargo deny --offline --config deny-sqlite.toml check advisories` | ok | ok, 2 duplicate-items warnings | **ok, zero warnings** |
| `RUSTSEC-2026-0381` ignore entries in the tree | 4 (2+1+1, not yet counted as duplicates) | 6 (2 root, 3 sqlite, 1 scaffold — the 10-21 PR had silently added 2 more) | **0** |
| Advisory gate self-test (4 policies) | OK | not yet re-run | **OK** (RUSTSEC-2020-0071 blocked, then waived, all 4) |
| Root/SQLite licenses + sources | clean | — | clean |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 91 | 91 | 91 (unchanged — no version touched) |
| Open Dependabot PRs | 8 (post-triage) | 9 | 9 (unchanged — no merges this pass; see Evidence) |
| Unwaived advisories, all graphs | 0 | 0 | 0 |

Rehearsal: `cargo test -p autumn-cli --bin autumn
new::tests::the_advisory_policy_resolves_its_flavor_anchors` (1 passed) and
`cargo test -p autumn-cli --test cli_tests
the_scaffold_waives_only_what_its_own_flavor_can_hit
the_scaffold_policy_covers_the_bundled_postgres_flavor` (2 passed, run
separately — `cargo test` takes one name filter at a time).

Generated-app sweep (`audit_generated_apps`, all 9 scaffold flavors) and the
satellite-graph sweep (`fuzz/`, `examples/island-flock/`) were **not**
re-run this pass (budget; neither touches `webpki-roots` or this advisory —
`fuzz/deny.toml` and `island-flock/deny.toml` never referenced
RUSTSEC-2026-0381, confirmed by grep). The scaffold template's own unit and
integration tests, which do exercise the `--bundled-pg` flavor's policy
rendering directly, are the rehearsal of record for that file.

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2
cargo fetch --locked
cargo deny fetch db
cargo deny --offline check advisories
cargo deny --offline --config deny-sqlite.toml check advisories
cargo deny --offline check licenses sources
cargo deny --offline --config deny-sqlite.toml check licenses sources
cargo deny check bans
./scripts/check-advisories.sh --self-test

cargo test -p autumn-cli --bin autumn new::tests::the_advisory_policy_resolves_its_flavor_anchors
cargo test -p autumn-cli --test cli_tests the_scaffold_waives_only_what_its_own_flavor_can_hit
cargo test -p autumn-cli --test cli_tests the_scaffold_policy_covers_the_bundled_postgres_flavor

# Dependabot queue (via GitHub MCP)
# search_pull_requests: repo:autumn-foundation/autumn is:open author:app/dependabot
# pull_request_read get/get_check_runs on each tracked PR number
# get_job_logs on the "Test suite" job id to see the skipped-shard pattern
```

## Follow-ups still open

1. **CI-health prerequisite for Dependabot triage.** Established this pass,
   not resolved: the "Test suite" aggregator is structurally red on every
   Dependabot-triggered run (skipped matrix shards under Dependabot's
   restricted token), and a separate "App-variant conformance" workflow has
   real, spreading failures across `trunk-dev` unrelated to any one
   dependency. Until one of these is fixed (or a maintainer manually re-runs
   a PR's workflow with full permissions to get a trustworthy signal),
   Ballast cannot safely merge *any* Dependabot PR from a green check alone,
   and "checks failed" cannot be read as "this bump broke something" either.
   This blocks the next pass's merges as much as it blocked this one's —
   worth a dedicated CI-health pass (Semaphore's lane, not Ballast's) before
   the next scheduled-batch merge.
2. **`css-inline` (#3353) reachability**: still undetermined (unchanged
   since 2026-10-19). The upstream fix is confirmed (GHSA-jh54-c7gc-mvcc,
   `class`-attribute HTML-escaping) and the PR is otherwise unremarkable; it
   is ready to merge the moment reachability is settled or CI health is
   restored.
3. **`wasmi` 2.0.0 / `croner` 4.0.1 migrations**: unchanged, real engineering
   work, no forcing fact, not attempted this pass (now third pass noting
   this without scoping further).
4. **GitHub-native Dependabot alert count moved: 16 → 18 (4 high, 11
   moderate, 3 low)**, first change in three passes, observed on this pass's
   own `git push` (no tool in this session lists individual alerts, so
   still no breakdown by id — fourth pass flagging the tooling gap, but
   first pass with an actual delta to report). Plausibly includes new GHSA
   findings opened this week (`css-inline` GHSA-jh54-c7gc-mvcc, tracked as
   #3353 above, is one candidate per last pass's note that GHSA-only
   findings never surface in `cargo deny`'s RustSec-backed count) plus
   whatever newly-dated advisories (RUSTSEC-2026-0386/0387, handled by the
   2026-10-25 Semaphore pass) GitHub's own scanner counts differently than
   `cargo-deny` does. Not confirmed against individual ids this pass.
5. `fuzz/` and `examples/island-flock/` scheduled-batch ownership: still
   unaddressed, unchanged across seven passes now.
6. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run, seventh pass
   running — this is the oldest open item on the ledger and deserves to be
   the next pass's primary focus rather than another queue triage.
