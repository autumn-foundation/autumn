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

**Prior Onramp work (`git log --oneline --all | grep -iE "onramp|🛣️"`).** Seven
prior PRs, most recent first: #2937 (Tailwind install-dir mismatch across
setup/dev/build.rs), #2915 (scaffold's own generated CI failed clippy -D
warnings), #2840 (local-dev-quickstart permadrift, job red→continue-on-error),
#2829 (dev-profile debuginfo cold-start lever, findings, needs a human
decision — still undecided, see below), #2817 (gating test/sim behind
test-support: negative result), #2798 (route-attr typo cascading through
routes!, 3 errors→1), #2758 (SQLite unique-violation resolves by column, not
just constraint name). Between them: setup/build reproducibility, generated-
CI health, cold-start compile time (twice), macro error UX, and one
SQLite-in-production correctness fix are already covered ground.

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

**First real integration.** Issue #2320 (closed 2026-08-30) audited every
first-party plugin's guide-vs-example coverage. Re-checked live: `autumn
plugin add` for every first-party plugin (`autumn-billing`, `autumn-search`,
`autumn-storage-s3`, `autumn-media-plugin`) is covered by
`plugin_add_first_party_scaffolds_cargo_check` in CI; `examples/wiki` and
`examples/cms` now exercise `autumn-search`/`autumn-billing`,
`examples/reddit-clone` exercises `autumn-storage-s3`, and the media-room
example exercises `autumn-media-plugin`. #2320's one remaining "no doc, no
example" row (inbound mail) is already closed by the in-flight
`changelog.d/inbound-mail-docs.md` fragment. This journey is thoroughly
covered; no fresh gap found.

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
than an autonomous default flip). Checked this cycle: issue #2795 is still
open, the debuginfo report's PR (#2829) has no maintainer decision recorded
beyond an automated Codex review pass, and no new commits or reports have
landed against #2795 since. Nothing new to act on without either (a) the
still-pending human decision, or (b) the `-Z self-profile`/`measureme`
tooling the 2026-09-17 report found blocked by this sandbox's `crates.io`
egress policy (`curl -sS -o /dev/null -w '%{http_code}' https://crates.io` →
`403`, re-confirmed this cycle) — an environment constraint, not something a
docs/API-layer change can route around.

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

- If a human decides #2829's debuginfo trade-off, that unblocks a real
  ~18% cold-start win pending one more round of above-noise-floor
  measurement against the real `cold_start_driver.rs` harness (not just
  `-p autumn-web` in isolation) — see that report's "Decision needed"
  section for the three options.
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
