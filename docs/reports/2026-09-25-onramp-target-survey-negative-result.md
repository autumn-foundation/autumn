# 🛣️ Onramp: this cycle's target survey — nothing cleared the bar (negative result)

## 🎯 Journey

All three journeys Onramp tracks — first run, first real integration, upgrade
— checked for a *known or reported* gap before opening any PR.
**Correction (caught by Codex review on this PR): an earlier draft of this
line claimed all three were "surveyed for a target meeting the Hard Gate."**
That overstates the upgrade journey specifically — as the Evidence section
now states plainly, this survey checked only whether an open issue reports
upgrade confusion (none found); it did **not** audit `docs/migrations/
next.md`'s dozens of real entries for clarity, completeness, or codemod
coverage the way a genuine Hard-Gate-caliber pass would require. So: first
run and first real integration were surveyed at that depth (existing
harnesses, issue search, and direct code/doc verification); upgrade was
checked only for reported problems, not audited to the same depth — and
that gap is itself named below as unfinished work, not silently folded into
"surveyed." No source or docs change accompanies this report: per the
process, "If you cannot produce these, the correct outcome is a findings
issue or a harness PR, not a rewrite," and per the impact floor, "Shaving
one step off a nine-step setup nobody complained about is indistinguishable
from churn. Do not ship it."

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

**Correction (caught by Codex review on this PR): an earlier draft called
issue #2795 "the one known live gap" on this journey.** There's a second,
documented one: `.github/workflows/quickstart-gate.yml`'s
`local-dev-quickstart` job covers the *other* install path README.md and
`docs/guide/getting-started.md` document — `cargo install --path autumn-cli`
from a source checkout, then `autumn new` — which pairs a source-built CLI
(today's `trunk-dev`) against whatever `autumn-web` is currently *published*
(the version `autumn new` pins). Any commit that changes a public
`autumn-web` API the base scaffold calls, before the next release ships it,
breaks that pairing, and `autumn doctor`'s `version_compat` check can't see
it (it compares version strings, not API surface). PR #2840 (already in
this survey's prior-work list) didn't fix this — its own title says so
("job red→continue-on-error"): it downgraded the job's build step to
advisory, because the job's own extensive comment explains this gap is
*inherent* to the repo's release-only versioning policy (CLAUDE.md: never
bump the workspace version outside a deliberate release) — there is no
commit that makes it permanently green. That's exactly the process's own
escape valve for an inherent gap ("the fix is a map of the inherent steps,
not a crusade against them. Report it.") — already done, in the workflow's
own comment and its documented `[patch.crates-io]` workaround — so this
doesn't change the conclusion, but #2795 was never the *only* known gap.

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
no matches). **Correction (caught by Codex review on this PR): an earlier
draft claimed the next command "matches only the root workspace manifest."**
Run for real, it matches nothing at all —
`grep -rl "autumn-billing\s*=" --include=Cargo.toml .` exits 1 with no
output, because the root manifest names `autumn-billing` only inside its
`members = [...]` array (`"autumn-billing",`, a list element, not a
`key = value` line the pattern would match). So: no crate in the workspace
depends on `autumn-billing` as a real dependency; the only place its name
appears at all is that one `members` list entry, confirmed by
`grep -n "autumn-billing" Cargo.toml`.

`plugin_add_first_party_scaffolds_cargo_check`'s own `FIRST_PARTY_PLUGINS`
list (`autumn-cli/tests/generate.rs:8886`) is `autumn-admin-plugin`,
`autumn-cache-redis`, `autumn-media-plugin`, `autumn-search`,
`autumn-storage-s3` — five entries, and **`autumn-billing` is not among
them**, so that gate doesn't even scaffold-check it. **Correction (caught by
Codex review on this PR): the five plugins it does list are all five it
covers, not four** — for those five, the gate only proves `autumn plugin
add` + `cargo check`
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

**A more concrete finding on `autumn-billing` specifically (caught by Codex
review on this PR): "no real consumer" undersold it.** `autumn-billing` is
one of six production entries in `autumn-cli/src/plugin/catalog.rs`'s
`FIRST_PARTY` catalog (`autumn-admin-plugin`, `autumn-billing`,
`autumn-cache-redis`, `autumn-media-plugin`, `autumn-search`,
`autumn-storage-s3`), and `autumn-billing/README.md` documents `autumn
plugin add autumn-billing` as a real, public install path. But the CI
scaffold-and-`cargo check` coverage described above
(`plugin_add_first_party_scaffolds_cargo_check`'s `FIRST_PARTY_PLUGINS`,
`autumn-cli/tests/generate.rs:8886`) only has five entries — `autumn-billing`
is the one catalog plugin with a documented install command and **zero**
CI coverage proving that command still works. That's a harness gap, not
just a docs/example gap: unlike `autumn-search`'s T3 issue (a missing
*example*, on a path that at least compiles per the scaffold check),
nothing catches `autumn plugin add autumn-billing` breaking outright.
Per the process's own Acceptable Outcome #3 ("the journey wasn't
measurable, now it is ... a complete deliverable on its own"), extending
`FIRST_PARTY_PLUGINS` to include it is a small, well-bounded harness fix —
**not attempted in this pass** because it needs its own verification (does
the scaffold actually compile today, added carefully rather than assumed)
that this already-long correction cycle isn't the place to start, but it is
now this survey's single most concrete, most directly actionable lead for
a future cycle — more bounded than the `autumn-search`/`wiki` gap above,
since it's a test-list addition pending one real scaffold-compile check,
not new feature code.

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

**Upgrade — correction (caught by Codex review on this PR): an earlier draft
claimed `docs/migrations/next.md` is "the standard rolling-draft template
with no unresolved breaking-change section outstanding."** That's false —
checked only the file's boilerplate header, not its body. `next.md` is
1,743 lines and, past the `{X.Y.Z}`-placeholder template section, carries
dozens of real, in-flight breaking-change entries for the next release: TLS
mTLS config, audit metadata, OpenAPI parameters, SSG manifest types, failure
capsules, two config additions, media-room `RoomStore`, admin-plugin
timestamps, capacity contracts, job admin records, DB scrub trigger
refusal, ACME DNS-01, an `#[authorize]` aliasing security fix, a
`#[feature_flag]`/`static_get` interaction fix, a `#[repository]`
owner/policy security fix, `#[lifecycle]` graph soundness, `OpenApiSchema`
serde interaction, and the `autumn-macros` crate split, among others — this
is a real, substantial upgrade journey, not an empty draft.

**Narrowing the claim to what was actually checked — correction (caught by
Codex review on this PR four times on the same paragraph now; retreating to
the most conservative accurate statement rather than trying again to
characterize the exact boundary):** this gate has enough moving parts —
two separate mechanisms, each scoped differently (the shell script's
Check 5 requires a label only for rename-class entries but scans every
`## ` section; a separate Rust test in `autumn-cli` requires a label on
every entry but, per its own `breaking_entries` helper, only under the
literal `## Breaking changes` heading, so an unlabeled non-rename entry
filed under `## Behavior changes` — `next.md` has two, "HTTPS: the
listener's connect-info type changed" and the CI dependency-audit entry —
passes both) — that "Automation is gate-enforced across every entry" is
**not** an accurate summary either, and this report has now spent five
correction rounds failing to state the real boundary precisely from memory.
So: no *open issue* reports a broken or confusing upgrade step for the
pending release (a targeted search found none); *some* entries carry a
gate-enforced Automation label (rename-class ones, and every literal
`## Breaking changes` entry) and *some* don't need to; entry-level
**Why/Before/After** prose is gated by neither mechanism, though most
sampled entries follow that shape anyway by convention. Whether the
`## Behavior changes`/`## Configuration changes` entries that escape both
label checks are themselves well-labeled in practice, and whether the
entries otherwise hold up for clarity, completeness, or
`autumn upgrade` codemod coverage generally, is exactly what this survey did
**not** audit — that would be its own Tier-1 journey
arithmetic pass (concepts, steps, and misuse-compile checks for a real
version bump), not something a few minutes of issue-search can stand in
for. So "the upgrade journey has no gap" is retracted; the honest statement
is narrower: no *reported* gap, and a real content audit of `next.md`
remains undone and is a legitimate candidate to scope for a future cycle
given how much breaking-change surface has accumulated there, including two
security-motivated changes.

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
backtrace file:line resolution. **Fix (caught a second time by Codex review
on this PR, after an intervening edit still left it wrong): `limited` does
not carry the backtrace-quality cost at all** — that cost is `debug=0`'s
alone (see the correction further below). So the 2026-09-21 report doesn't
just add a warm-edit number to the same trade-off; it strengthens `limited`
specifically into a cold-start win *and* a large recurring per-edit win
*with no backtrace cost*, which is a materially stronger case for picking it
over `debug=0` than the 2026-09-17 report alone showed, not just a
restatement of the same open question.

**This still doesn't clear the bar for autonomous action here.** The
2026-09-21 report is explicit that it feeds, rather than resolves, issue
#2795's decision: gap 1 (re-measuring against the real `autumn
new`-scaffolded project via `cold_start_driver.rs`, not `examples/hello`) is
still open in both reports, and `dev-loop-latency.yml`'s own measurement
driver isn't wired up yet (`build_placeholder_results` always passes every
budget with zero samples — see that report's "Cost to productionize").
**Correction (caught by Codex review on this PR): an earlier draft said the
2026-09-17 report's backtrace-quality trade-off "still applies regardless of
how much stronger the compile-time case has gotten." That overstates it for
`limited` specifically** — this report's own correction two paragraphs up
already says `debug=1`/`limited` preserves file:line resolution, so that
particular cost only attaches to `debug=0`, not to every level. For
`limited`, what's actually still missing is simply that no maintainer
decision is recorded on either report as of this cycle, on top of the two
open measurement gaps above — not an unresolved backtrace cost. The
backtrace trade-off remains the live reason `debug=0` specifically needs a
named human call rather than an autonomous flip; it isn't a reason `limited`
does too. The `-Z self-profile`/`measureme` tooling gap the
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

- **Most concrete lead:** add `autumn-billing` to
  `FIRST_PARTY_PLUGINS` (`autumn-cli/tests/generate.rs:8886`), verifying
  first that its scaffold genuinely compiles via
  `plugin_add_first_party_scaffolds_cargo_check`. It's the one catalog
  plugin (`autumn-cli/src/plugin/catalog.rs`) with a documented public
  install command (`autumn-billing/README.md`: `autumn plugin add
  autumn-billing`) and no CI coverage proving that command still works — a
  harness gap, not a feature-implementation one, so it's a small, bounded
  fix rather than the multi-part work the `autumn-search`/`wiki` item below
  needs.
- Mount `autumn-search` on `examples/wiki` per issue #2320's own T3 Gap 6
  fix suggestion — a real, still-open, previously-identified gap on the
  "first real integration" journey (see above). This is a real,
  well-evidenced candidate; it needs its own implementation + harness
  cycle, not a fold-in here.
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
# Prior Onramp work — NOT this (undercounts in a shallow clone, see the
# correction above — `git rev-parse --is-shallow-repository` first to check):
#   git log --oneline --all | grep -iE "onramp|🛣️"
# Use GitHub's own PR search instead, which isn't limited by local clone depth
# (via the GitHub MCP server / API, or the web UI):
#   search_pull_requests(owner, repo, query: 'Onramp in:title is:merged')

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
