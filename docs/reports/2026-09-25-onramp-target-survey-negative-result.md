# 🛣️ Onramp: this cycle's target survey — nothing cleared the bar (negative result)

## 🎯 Journey

All three journeys Onramp tracks — first run, first real integration, upgrade
— surveyed for a target meeting the Hard Gate (committed clean-room harness,
weighted evidence, pre-change baseline, a specific mechanism, an
after-measurement plus compatibility check) before opening any PR. No source
or docs change accompanies this report: per the process, "If you cannot
produce these, the correct outcome is a findings issue or a harness PR, not a
rewrite," and per the impact floor, "Shaving one step off a nine-step setup
nobody complained about is indistinguishable from churn. Do not ship it."

Reproduce this survey: see **🔬 Reproduce** below.

## 📈 Evidence / Prior art

**Prior Onramp work — correction (caught by Codex review on this PR): an
earlier draft cited `git log --oneline --all | grep -iE "onramp|🛣️"` and
found only 7 prior PRs.** That command undercounts badly in this checkout —
`git rev-parse --is-shallow-repository` confirms this is a **shallow clone**
(52 commits of local `trunk-dev` history), so a plain local `git log` misses
anything older than that window. The correct source is GitHub's own PR
search (`is:merged "Onramp" in:title`), which returns **13 merged prior
PRs**, not 7: #2426 (stale `createdb` step in the CRUD-generators fast
path), #2459 (source-built-CLI vs. published-`autumn-web` drift, local-dev
quickstart uncovered→red), #2494 (`autumn setup` retries a dropped Tailwind
download), #2641 (getting-started's overclaim about `version_compat` drift
detection), #2707 (compile the getting-started snippets in CI, 0 harness → 5
fences checked), #2758 (SQLite unique-violation resolves by column, not just
constraint name), #2798 (route-attr typo cascading through `routes!`, 3
errors→1), #2817 (gating test/sim behind test-support: negative result),
#2829 (dev-profile debuginfo cold-start lever, findings, needs a human
decision — still undecided, see below), #2840 (local-dev-quickstart
permadrift, job red→continue-on-error), #2876 (Todo Tutorial's 8 stub
chapters, dead-end→redirect), #2915 (scaffold's own generated CI failed
clippy -D warnings), #2937 (Tailwind install-dir mismatch across
setup/dev/build.rs). (#2755, same title as #2840, is closed-but-unmerged —
superseded by it, not a distinct fourteenth PR.) Between them: setup/build
reproducibility (several rounds), generated-CI health, cold-start compile
time (twice), snippet/quickstart harness construction, macro error UX, one
tutorial dead-end fix, and one SQLite-in-production correctness fix are
already covered ground — a wider swath than the undercounted list first
suggested, which only reinforces this survey's conclusion that the easy,
uncovered targets are scarcer than a first pass over this repository would
suggest.

**Question log (Tier 2).** This repository's issue tracker is almost
entirely internal automated-analysis reports (🪞 Echo, ⚡ Bolt, 🪝 Snag, 📐
Capacity-contract, 🛡 Warden, etc.), not organic user complaints. Searches for
"confusing", "unclear", "doesn't work", "error message", "first time",
"getting started" returned no matches. **No question class meets the ≥3-
occurrence bar this cycle** — the task's own escape valve for this case.

**First run.** `scripts/check-quickstart.sh` already runs the full README
quickstart (install → new → setup → build → serve → scaffold →
scaffold-build → scaffold-migrate → scaffold-serve) against the *published*
crates.io artifacts in CI, not just the in-tree workspace — this is already
the Tier-1 clean-room harness the Hard Gate asks for, and it is green.
Issue #2795 (open, cold-start compile time) is the one known live gap on this
journey; see below for why it isn't actionable this cycle.

**First real integration — correction (caught by Codex review on this PR):**
an earlier draft of this paragraph claimed `examples/wiki` and
`examples/cms` "now exercise `autumn-search`/`autumn-billing`" and that this
closed issue #2320's plugin-coverage gaps. **That was wrong** — sourced from
a background research pass this report failed to independently verify before
publishing. Direct check: neither `examples/wiki/Cargo.toml` nor
`examples/cms/Cargo.toml` depends on `autumn-search` or `autumn-billing` as
a real dependency; both crate names appear only in an unrelated comment
about a shared testcontainer convention
(`grep -n "autumn-billing\s*=\|autumn-search\s*=" examples/*/Cargo.toml` →
no matches). No crate in the workspace depends on `autumn-billing` at all
(`grep -rl "autumn-billing\s*=" --include=Cargo.toml .` → only the root
workspace manifest, which just declares it as a member, not a consumer).

`plugin_add_first_party_scaffolds_cargo_check`'s own `FIRST_PARTY_PLUGINS`
list (`autumn-cli/tests/generate.rs:8886`) is `autumn-admin-plugin`,
`autumn-cache-redis`, `autumn-media-plugin`, `autumn-search`,
`autumn-storage-s3` — five entries, and **`autumn-billing` is not among
them**, so that gate doesn't even scaffold-check it. For the four plugins it
does cover, the gate only proves `autumn plugin add` + `cargo check`
succeeds on a freshly scaffolded project — not that any example actually
*uses* the plugin's features, which is what issue #2320's coverage matrix
scores as "Example."

Issue #2320 (closed 2026-08-30, docs/examples coverage audit) explicitly
named this as **T3 Gap 6**: *"`autumn-search` plugin (keyword + vector) —
`search.md` is one of the most detailed guides in the tree; no example
installs the crate. Fix: mount `autumn-search` on `wiki` (which already has
`#[searchable]` FTS) as the keyword-backend example, or add a vector-search
route."* Re-verified live: **this gap is still open** — `wiki` has no
`autumn-search` dependency today. `autumn-billing` isn't a named row in
#2320's matrix at all (likely added to the workspace after that audit ran)
and also has no real consumer anywhere in the tree.

This is a real, evidence-backed, previously-identified target for a future
Onramp cycle — mount `autumn-search` on `examples/wiki` per #2320's own
suggested fix, closing a T3 example gap on the "first real integration"
journey. **Not pursued in this cycle**: it's a real feature-integration
implementation (a new dependency, real search-index wiring, tests, and
`docs/guide/search.md` cross-linking), not a same-layer docs/error-message/
default fix, so it needs its own RED/GREEN cycle with its own baseline and
harness rather than being folded into this survey's correction pass. Left
open for the next cycle, now with the false "already resolved" claim
retracted so it doesn't mislead anyone re-reading this report.

The one severe, well-evidenced candidate on this journey — a `cache=shared`
SQLite pool (`docs/guide/sqlite-in-production.md`'s documented, supported
concurrent-pool configuration) deadlocking permanently under concurrent
read-then-write (issue #2885, repro 2/2, self-certifying per the hang/crash
class), plus its sibling issue #2881 (`busy_timeout` bypassed under table
contention, repro 2/30) — is **already claimed**: PR #2918
("Vesper/bugbash 2885 tx immediate", 601 additions, open since 2026-09-23)
targets #2885 directly, and PR #2930 ("bugbash-2881") targets #2881. Both
issues were filed by 🪝 Snag, whose own precedent (e.g. #2941, landed
2026-09-25) is to report rather than fix, and dedicated fix PRs already exist
for both reports in this cluster. Opening a second, independent PR against
either issue right now would duplicate in-flight work on a money-ledger-
adjacent locking path — exactly the kind of blast radius this charter asks
to route through "ask before" rather than parallel, uncoordinated fixes. Not
pursued this cycle for that reason, not for lack of evidence.

**Upgrade.** `docs/migrations/next.md` is the standard rolling-draft
template with no unresolved breaking-change section outstanding. No open
issue or fragment describes a broken or confusing upgrade step for the most
recent version bump.

**Cold-start compile time (issue #2795, open).** Two prior Onramp reports
already invested here without a shippable result:
`docs/reports/2026-09-16-onramp-test-sim-compile-gate-negative-result.md`
(gating `test`/`sim` behind `test-support`: no measurable effect, and the
gate itself turned out to have a wide, incompletely-mapped blast radius) and
`docs/reports/2026-09-17-onramp-devprofile-debuginfo-cold-start-findings.md`
(`-C debuginfo=0` measured ~18% faster cold builds — just under the 20%
floor on the honest pooled number — but degrades panic-backtrace file:line
resolution for every locally-compiled frame in every generated project,
forever, and the report explicitly asks for a named human decision rather
than an autonomous default flip).

**Correction (caught by Codex review on this PR): an earlier draft of this
paragraph claimed "no new commits or reports have landed against #2795"
since 2026-09-17. That's false** —
`docs/reports/2026-09-21-prospect-debuginfo-warm-edit-rebuild-cost.md` (PR
#2882, filed 2026-09-21, already on `trunk-dev` before this survey started)
closes exactly the gap the 2026-09-17 report left open: it measured the same
`debug = 1`/`limited` level on the *warm*, incremental edit loop (not just
the cold build) and found a properly-replicated, order-reversal-checked
**~26-28% reduction** in compile-and-link wall time — well clear of its own
pre-registered 10% materiality line — and independently confirmed (the
2026-09-17 report only asserted this in prose) that `limited` preserves
backtrace file:line resolution. That changes the shape of the pending
decision from "one-time cold-start win vs. a permanent backtrace-quality
cost" to "a cold-start win *and* a large recurring per-edit win vs. that same
cost" — a materially stronger case for picking `limited`, not just a
restatement of the same open question.

**This still doesn't clear the bar for autonomous action here.** The
2026-09-21 report is explicit that it feeds, rather than resolves, issue
#2795's decision: gap 1 (re-measuring against the real `autumn
new`-scaffolded project via `cold_start_driver.rs`, not `examples/hello`) is
still open in both reports, `dev-loop-latency.yml`'s own measurement driver
isn't wired up yet (`build_placeholder_results` always passes every budget
with zero samples — see that report's "Cost to productionize"), and the
2026-09-17 report's own reason for deferring to a human — a permanent,
every-build backtrace-quality trade-off is a named-decision call, not an
autonomous default flip — still applies regardless of how much stronger the
compile-time case has gotten. No maintainer decision is recorded on either
report as of this cycle. The `-Z self-profile`/`measureme` tooling gap the
2026-09-17 report hit is unrelated to this and remains blocked by this
sandbox's `crates.io` egress policy
(`curl -sS -o /dev/null -w '%{http_code}' https://crates.io` → `403`,
re-confirmed this cycle) — an environment constraint, not something a
docs/API-layer change can route around. Flagging the 2026-09-21 report's
existence here so the next cycle (or the human deciding #2795) starts from
the full picture rather than re-discovering it.

## 💡 Conclusion

No target this cycle clears the Hard Gate: the two strongest hard-failure
candidates are already being fixed elsewhere, the one open findings report
explicitly awaits a human decision, and no organic question-log class meets
the occurrence bar. Per the process's own escape valve ("If the top entries
are inherent... that is a legitimate finding — the fix is a map of the
inherent steps, not a crusade against them. Report it.") and per Acceptable
Outcome #4, recording this as a negative result is the correct action rather
than manufacturing a cosmetic change to have something to ship.

**Left for whoever picks either up next:**

- Mount `autumn-search` on `examples/wiki` per issue #2320's own T3 Gap 6
  fix suggestion — a real, still-open, previously-identified gap on the
  "first real integration" journey (see above). This is the most
  concretely-scoped candidate this survey found; it needs its own
  implementation + harness cycle, not a fold-in here.
- If a human decides #2829's debuginfo trade-off, that unblocks a real win,
  the size depending on which level they pick — **correction (caught by
  Codex review on this PR): an earlier draft attached both the ~18%
  cold-start figure and the ~26-28% warm-edit figure to `debug=1`/`limited`.
  They're for different levels.** `debug=0` (no debug info) is the one that
  measured ~18% cold-start / ~36-39% warm-edit, but it drops file:line
  resolution from every locally-compiled backtrace frame. `debug=1`/
  `limited` — the level that the 2026-09-21 report confirmed *preserves*
  file:line resolution — measured a thinner ~8.7% cold-start win but a
  properly-replicated ~26-28% warm-edit win. Either way, the pending
  decision needs one more round of above-noise-floor cold-build measurement
  against the real `cold_start_driver.rs` harness (not just `-p
  autumn-web`/`examples/hello` in isolation) and the `dev-loop-latency.yml`
  live measurement driver actually being wired up — see the 2026-09-17
  report's "Decision needed" section and the 2026-09-21 report's "Cost to
  productionize" for the full list.
- Once PR #2918 and/or #2930 land, `docs/guide/sqlite-in-production.md` and
  `docs/guide/money.md` should be revisited: both currently describe
  `cache=shared` concurrency behavior (`busy_timeout` bounding lock waits,
  "the second of two contenders" losing) that #2881/#2885 showed is
  inaccurate for table-lock contention. That's a natural, low-risk Onramp
  docs-layer follow-up — deferred here only because landing it against
  still-changing underlying behavior would need rewriting again the moment
  the fix PRs merge.

## 🔬 Reproduce

```bash
# Prior Onramp work:
git log --oneline --all | grep -iE "onramp|🛣️"

# Question-log check (Tier 2) — repeat periodically, not just this cycle:
#   search open issues for "confusing", "unclear", "doesn't work",
#   "error message", "first time", "getting started"; bucket by class;
#   act only once a class hits >=3.

# First-run harness (already exists, already green — the Tier-1 clean-room
# harness this Hard Gate asks for):
scripts/check-quickstart.sh install
scripts/check-quickstart.sh new
# ...through scaffold-serve; see the script's own header for phase order.

# Cold-start egress constraint, re-check before relying on -Z self-profile:
curl -sS -o /dev/null -w '%{http_code}' https://crates.io
```
