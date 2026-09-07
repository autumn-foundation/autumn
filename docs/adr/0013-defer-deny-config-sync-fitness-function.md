# ADR 0013 [deferred]: A Fitness Function for the deny.toml / deny-sqlite.toml Sync Invariant

- Status: Deferred
- Date: 2026-09-07
- Deciders: Keystone (architecture review agent)
- Tags: supply-chain, cargo-deny, fitness-function, deferral

## Decision under review

Whether to add an automated check (a fitness function) enforcing the
already-documented invariant that `deny.toml` and `deny-sqlite.toml`'s
`[advisories].ignore`, `[licenses].allow`, and `[sources]` sections stay
identical, versus continuing to rely on the human process the docs currently
describe.

## Door class and reversal cost

**Two-way door.** Adding one repo-hygiene test (`autumn-cli/tests/integration/`
or a root-level script check) that parses both TOML files and asserts the three
sections are structurally equal is additive and reversible by deleting the test
— under a day either way. Per this framework's own rule, a door this cheap is
normally a PR-level call, not an ADR. It is recorded here only because the
Keystone review that found it turned up a concrete, dated near-term test of the
invariant (see Trigger) worth writing down rather than silently noting and
moving on.

## Evidence (Tier 2 — repository record)

1. **The invariant is explicit and repeated in three places**, none of them
   code: `deny-sqlite.toml`'s own header ("KEEP `[advisories].ignore`,
   `[licenses].allow`, and `[sources]` IN SYNC with deny.toml — only the
   `[graph]` features and the `unused-ignored-advisory` knob below
   intentionally differ"), `deny.toml`'s header, and CONTRIBUTING.md §"Supply
   chain (cargo-deny)" ("The two configs share the same advisories/licenses/
   sources policy — keep them in sync").
2. **No test checks it.** The one test that touches both files,
   `the_frameworks_own_waivers_carry_a_reason`
   (`autumn-cli/tests/integration/dependency_audit.rs:898-914`), loops over
   `["deny.toml", "deny-sqlite.toml"]` independently and asserts each waiver
   has a `reason =`; it never compares the two files' entries to each other.
   No other test, script, or CI step (`.github/workflows/ci.yml`,
   `scripts/check-advisories.sh`) diffs or cross-checks them either.
3. **Today the invariant holds**, checked directly rather than assumed: all
   three shared waiver entries (`RUSTSEC-2023-0071`, `RUSTSEC-2024-0384`,
   `RUSTSEC-2026-0253`) have byte-identical `id =` and `reason =` fields in
   both files. Only the surrounding `#` comments differ, and deliberately so
   — `deny-sqlite.toml` says "kept identical to deny.toml on purpose... full
   rationale lives there" and gives a shorter version. That is intentional
   deduplication of prose, not drift.
4. **A concrete, dated test of that reliance is already on the calendar**: all
   three shared entries carry the same `review-by: 2026-10-01`. That date
   requires a human to open and edit both files in the same PR, by hand, with
   nothing that fails a build if only one is updated.
5. **The pattern is already visible one step over.** CONTRIBUTING.md's own
   supply-chain section names a second, already-real gap in the same
   subsystem: `fuzz/`'s `Cargo.lock` is "currently out of sync with its
   manifest," and `fuzz/` and `examples/island-flock` (separate declared
   workspaces) are entirely outside both `deny.toml` and `deny-sqlite.toml`'s
   coverage. That is a different, already-acknowledged, already-open gap —
   cited here only as evidence that this subsystem's sync invariants do not
   enforce themselves absent a check, not as part of this decision.

## Do nothing / decide later — 12-month baseline

No incident has ever been caused by this gap — the invariant has held since
`deny-sqlite.toml` was added (2026-09-03) through today. The honest cost of
leaving it alone is not zero, though, and it is not indefinite either: on or
before 2026-10-01, whoever triages the three review-by waivers has to touch
both files by hand, and nothing catches a partial edit. If both files are
updated together, as they have been, nothing changes. If they are not, the
first symptom is a false-negative CI pass — the sqlite backend graph, or the
default graph, silently keeping a waiver for an advisory the other graph has
already resolved — which is invisible until someone reads both files
side-by-side, exactly as this review just did.

## Impact floor check

None of the six clearing conditions are met: no Tier-1 incident (nothing has
gone wrong yet); no cross-team change count to reduce (single-maintainer
repository, per ADR 0012); no dated Tier-4 fact forcing the door (2026-10-01
is a self-imposed waiver review date, not an external commitment); no Tier-3
spike showing a committed requirement fails (the invariant currently holds
under direct inspection); no removed cost exceeding a migration cost (there is
no cost being paid today); no ≥3-data-point asymptotic trend (this would be
the first instance, not a pattern). This does not clear the floor — it is a
PR-level fix for whoever next touches either file, not an RFC.

## Default path

Leave the current process in place: two files, a documented "keep in sync"
convention, and human diligence at each waiver review. The maintainer adds the
cross-file equality check opportunistically, in the same ordinary PR that next
edits either file for the 2026-10-01 waiver review, or sooner if convenient —
that is routine hygiene work and needs no architecture review.

## Seam kept open

No new seam is needed. Both files already funnel through the same reproducible
entry points (`scripts/check-advisories.sh`, the `supply-chain` CI job,
`autumn doctor`'s `dependencies` check added in #1633), so a future equality
check has an obvious home — a repo-hygiene test parsing both TOML files and
asserting `[advisories].ignore`, `[licenses].allow`, and `[sources]` match
field-for-field — without restructuring anything that exists today.

## Trigger to revisit

Revisit this decision if either occurs:

- The 2026-10-01 review-by date passes with the two files' shared waiver
  entries left inconsistent (checked directly, the way this ADR did).
- A third cargo-deny config is added for another mutually-exclusive backend
  graph, making "keep N files in sync by hand" a 3-way (or more) coordination
  problem rather than a 2-way one.

## Reproduce

```bash
# The documented invariant, in both files' own words
grep -n "IN SYNC\|share the same advisories" deny-sqlite.toml CONTRIBUTING.md

# The only test touching both files checks reasons, not cross-file equality
sed -n '896,914p' autumn-cli/tests/integration/dependency_audit.rs

# No script or CI step diffs the two files against each other
grep -rn "deny-sqlite" .github/workflows/ci.yml scripts/check-advisories.sh

# Today the shared waivers are in fact identical (id + reason fields)
for id in RUSTSEC-2023-0071 RUSTSEC-2024-0384 RUSTSEC-2026-0253; do
  echo "== $id =="
  grep -n "$id" -A1 deny.toml | grep -E "id =|reason ="
  grep -n "$id" -A1 deny-sqlite.toml | grep -E "id =|reason ="
done

# The shared, dated trigger
grep -n "review-by" deny.toml deny-sqlite.toml

# The adjacent, already-admitted gap cited as corroborating evidence
grep -n "out of sync with its manifest" CONTRIBUTING.md
```
