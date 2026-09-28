# ADR 0014 [deferred]: Fleet-Wide Coordination For The Custom-Domain Issuance Budget

- Status: Deferred
- Date: 2026-09-28
- Deciders: Keystone (architecture review agent)
- Tags: acme, custom-domains, distributed-systems, state-externalization, deferral

## Decision under review

Issue #2644 ("Custom domains: issuance budgets reset on restart, so a crash
loop can outrun the advertised caps") lays out three implementation options
for fixing `IssuanceLimiter`'s durability gap, of increasing scope:

1. Persist the attempt log next to the existing per-domain record.
2. Derive an approximate window from already-durable certificate timestamps.
3. Coordinate the budget through a distributed backend, so `global_per_hour`
   is enforced fleet-wide rather than per process.

Only option 3 is an architecture question. Options 1 and 2 are ordinary
bug fixes against the bug's own acceptance criteria and need no review; this
ADR exists to decide whether option 3's fleet-wide coordination should be
built now, alongside the fix, or deferred.

## Door class and reversal cost

**Two-way door**, but reversal is not the bare storage swap this ADR
originally described. `check` (`tenant_domains.rs:483`) runs before the
per-hostname fleet lease is even acquired, and `record_attempt`
(`tenant_domains.rs:561`) runs after two subsequent awaited calls
(`try_acquire`, `record_issuing_for`) — a real gap, not a formality. The
per-hostname lease excludes a second replica from racing the *same*
hostname through that gap, but does nothing for two replicas each ordering
a *different* hostname: both can call `check` and see capacity before
either calls `record_attempt`, a classic check-then-act race. Swapping
`RwLock<Attempts>` for a durable or distributed store behind the same two
separate calls would not close that race; a correct option 3 needs an
atomic check-and-reserve operation (or a global guard spanning both calls),
which is a real interface change at the one call site, not only an
internal one. That is more work than a pure storage swap, but it is still
contained to `tenant_domains.rs`'s `issue_one` and `IssuanceLimiter`'s
public methods — reversal in either direction (add atomic fleet-wide
reservation, or strip it back to today's two-call shape) stays a
same-crate, few-function change, not a rearchitecture. Per this framework's
own rule, a door this cheap is normally a PR-level call, not an ADR; it is
recorded here only because #2644 already staged the fleet-wide option as
one of three named choices, and taking it without evidence would be
exactly the "deciding a two-way door by RFC" mistake this framework warns
against in the other direction — so this ADR's job is to say *no*, in
writing, rather than let the biggest option win by being listed last.

## Evidence (Tier 2 — repository and issue record)

Reproduce with the commands in **Reproduce** below.

1. **The correctness gap is real and already scoped**, not hypothetical.
   #2644 (filed 2026-09-09 by the maintainer, "filed rather than fixed")
   documents four concrete failure modes against `autumn/src/custom_domain.rs`'s
   `IssuanceLimiter`, which keeps both its per-domain and global attempt
   windows in a plain in-process `RwLock<Attempts>` (`HashMap<String, Vec<i64>>`
   plus a `Vec<i64>`), rebuilt empty on every process start
   (`autumn/src/app.rs:10047`).
2. **Three of the four failure modes are single-instance bugs, not
   multi-replica ones.** Restart-resets-the-window (item 1), tick-start
   timestamp skew (item 3), and budget-deferral misclassified as failure
   (item 4) all reproduce on exactly one running instance — they are
   ordinary correctness bugs against the limiter's own documented contract,
   independent of how many replicas exist. Only item 2 ("each replica has
   its own [budget]") is inherently about fleet size.
3. **The minimal fix already has a ready-built seam — for the per-domain
   half.** `CustomDomainStore` (`autumn/src/custom_domain.rs:666`) is
   already a pluggable trait with `MemoryCustomDomainStore` and
   `FsCustomDomainStore` implementations, writing one durable record per
   domain. Durable-state option 1 in #2644 — "the `CustomDomainStore` seam
   already writes per-domain JSON; per-domain attempt timestamps could ride
   on the record, **and the global window in one small file**" — requires
   no new abstraction for the per-domain half; it is additive data on a
   trait that already exists and is already exercised by tests. The global
   half needs its own small durable store, not a ride on `CustomDomainStore`:
   `CustomDomainRegistry::remove_if` (`custom_domain.rs:1606`) deletes a
   domain's record outright, while `IssuanceLimiter::forget`
   (`custom_domain.rs:1927`) only clears that domain's *per-domain* history
   and intentionally leaves its attempts in the global vector — so a global
   window reconstructed from per-domain records would lose an offboarded
   domain's still-counting attempts on the next restart, letting the
   deployment-wide budget run over exactly the case it exists to catch.
4. **This is not a new architecture question — ADR 0004 already answered
   it in the abstract**, and this repository's evidence bar (Step 7:
   "check for prior decisions") requires citing that rather than re-deciding
   it. ADR 0004 ("Externalize Distributed Runtime State", 2026-04-09)
   classifies "rate-limit counters" as Category 3 ("Shared Mutable Runtime
   State") that "must use pluggable external backends in production-safe
   deployments" — but the same ADR's own risk list names "over-externalizing"
   and "adding too many backends too early" as live failure modes to avoid,
   and its Coordination Strategy section prefers Postgres-backed leases
   "where that keeps the system simple" over building new distributed
   machinery on spec. `autumn/src/security/rate_limit.rs` shows what the
   ADR 0004-compliant shape looks like when the fleet-wide case is real: an
   explicit `"memory"` vs `"redis"` backend choice, config-selected, tested
   under `redis_job_admin`-style CI coverage per CLAUDE.md's CI notes. No
   comparable multi-backend need has been demonstrated for the issuance
   limiter.
5. **The seam for fleet-wide coordination, if it is ever needed, already
   exists and needs nothing new built to stay open.** ADR 0010 ("Expose an
   App-Facing Distributed Lock", accepted 2026-07-09) already generalized
   the framework's internal Postgres-advisory-lock primitive
   (`try_pg_advisory_lock`) into an app-facing "run this exactly once across
   the cluster" seam. A future option-3 implementation would reach for that
   primitive rather than inventing a fourth coordination mechanism.
6. **No evidence of multi-replica custom-domain deployment exists in this
   repository.** Per ADR 0012's Tier-2 finding, 89.4% of this repository's
   commits are from one human author with no second team; nothing in the
   issue tracker, CI matrix, or example apps demonstrates an operator
   running ≥2 replicas actively issuing certificates for the same domain
   set today. Item 2 in #2644 is a real gap in what the code *claims*
   (`global_per_hour` "enforced per process-lifetime-hour, not per hour"),
   but nothing shows it is a gap anyone is currently exposed to.

## Do nothing / decide later — 12-month baseline

"Do nothing" is not actually on the table here — items 1, 3, and 4 are
correctness bugs against the limiter's own documented guarantee ("well
inside Let's Encrypt's 300-new-orders-per-account-per-3-hours limit... a
permanently broken domain converges to one attempt per `max_backoff_secs`"),
and that guarantee is what stands between a misconfigured or crash-looping
deployment and Let's Encrypt rate-limiting the account for every tenant on
it, not just the broken one. Fixing those three — item 1 via durable-state
option 1, items 3 and 4 as their own small, independent fixes — is due
regardless of this ADR; all three already carry acceptance criteria in
#2644. The question this ADR answers is narrower: whether to *also* build
fleet-wide coordination (durable-state option 3) in the same pass.

If durable-state option 3 is deferred and only items 1, 3, and 4 are
fixed (via durable-state option 1 for item 1): a deployment running
multiple replicas that are all actively issuing custom-domain certificates
gets up to N× the advertised `global_per_hour` budget, same as today,
where N is replica count. That is a real gap, bounded by Let's Encrypt's
own outer limit (300 orders/3h/account) regardless of how many replicas
Autumn's own limiter believes exist — the vendor's hard cap is the actual
backstop, not this framework's budget. Nothing in the record shows a
deployment operating close enough to that outer limit for the gap to have
bitten anyone. If it does, revisit per the trigger below.

## Impact floor check

None of the six clearing conditions are met for option 3 specifically: no
Tier-1 incident data (no reported case of multi-replica budget overrun); no
cross-team change count to reduce (single-maintainer repository, per ADR
0012); no dated Tier-4 fact forcing fleet-wide coordination now (Let's
Encrypt's limit is real but is a backstop the single-instance fix already
keeps deployments well inside, per the existing budget defaults); no Tier-3
spike showing the single-instance-fixed design fails a committed
requirement; no removed cost exceeding a migration cost (no cost is
currently being paid — the bug is unshipped-fix, not unshipped-and-paid-for);
no ≥3-data-point asymptotic trend (this is the first and only instance of a
hand-rolled, non-pluggable "shared mutable runtime state" counter found in
this pass — see Reproduce for the negative search across
`plugin_sandbox::capability::quota` and other `*Limiter`/`*Budget`/`*Quota`
structs, all of which are correctly request- or process-scoped by design,
not mis-scoped copies of this same problem). This does not clear the
floor for option 3 — it is not RFC-worthy today.

## Default path

Ship durable-state option 1 from #2644 (for failure-mode item 1), together
with the independent fixes for items 3 and 4, as ordinary PR-level bug
fixes against that issue's existing acceptance criteria: persist per-domain
attempt timestamps on the existing `CustomDomainStore` record, persist the
global attempt window separately in its own small durable store rather than
deriving it from per-domain records (see Evidence item 3 above for why),
stamp attempts with their own time rather than the tick's start time, and
route a budget deferral through `retry_after_secs` instead of the generic
failure/backoff path. None of this needs architecture review — it is
additive state on an existing trait plus one small new durable store,
exactly the shape ADR 0012 and 0013 both found to be routine engineering
rather than a door worth an ADR. Leave the limiter's *coordination* model
per-process; do not add a Redis or Postgres-backed cross-replica layer for
it in this pass — durability and cross-replica coordination are separate
questions, and only the latter is what this ADR defers.

## Seam kept open

No new seam is needed to keep option 3 available later. Two already exist:

- `CustomDomainStore` already abstracts persistence behind a trait with
  swappable implementations, so a future backend for the attempt log
  (should one become necessary) is additive, not a rewrite of
  `IssuanceLimiter`'s call sites.
- ADR 0010's app-facing distributed lock already provides the "exactly once
  across the cluster" primitive a fleet-wide `global_per_hour` enforcement
  would coordinate through, so building option 3 later starts from an
  existing, tested primitive rather than a new one. Building it correctly
  means using that lock to make `check` and `record_attempt` an atomic
  check-and-reserve (or wrapping both calls in one held lock) — see Door
  class above — not merely pointing the existing two-call interface at a
  shared store.

## Trigger to revisit

Revisit the durable-state-option-3 (fleet-wide coordination) decision if
either occurs:

- An operator reports running ≥2 replicas that concurrently issue
  custom-domain certificates — for the same hostname or different ones.
  The fleet lease in `tenant_domains.rs::issue_one` is keyed per-hostname
  (`format!("custom-domain:{hostname}")`, "one replica per hostname
  orders"), so it excludes a second replica from racing the *same* domain
  but does nothing to stop two replicas issuing for *different* domains at
  the same time, each checked against its own process-local
  `global_per_hour` allowance. Overlapping domain sets are not required for
  the per-process budget gap (#2644 item 2) to produce observed
  over-issuance — disjoint domains issued concurrently across replicas
  already multiply the advertised budget by replica count.
- A deployment is documented approaching Let's Encrypt's outer
  300-orders-per-3-hours account limit, making this framework's own budget
  (rather than the vendor's) the thing that needs to hold exactly, not just
  approximately.

## Reproduce

```bash
# The staged decision and its three options, in the maintainer's own words
# (gh not available in this environment; fetched via the GitHub MCP tool)
# https://github.com/autumn-foundation/autumn/issues/2644

# The in-process-only limiter state
sed -n '1810,1840p' autumn/src/custom_domain.rs

# Where it is constructed fresh per process, at boot
grep -n "IssuanceLimiter::new" autumn/src/app.rs

# The already-pluggable persistence seam option 1 would extend
sed -n '666,700p' autumn/src/custom_domain.rs

# ADR 0004's Category 3 classification and its own over-externalization risk
grep -n "Shared Mutable Runtime State\|over-externalizing\|pluggable external backends" \
  docs/adr/0004-externalize-distributed-runtime-state.md

# The already-built fleet-wide coordination primitive (ADR 0010), the seam
# a future option 3 would reach for
sed -n '1,20p' docs/adr/0010-app-facing-distributed-lock.md

# The one correctly-pluggable precedent for a real fleet-wide need
grep -n '"memory"\|"redis"\|RedisStore' autumn/src/security/rate_limit.rs

# The fleet lease is keyed per-hostname, not per-tick: it stops two replicas
# racing the SAME domain, not two replicas issuing for DIFFERENT domains at
# the same time (why the revisit trigger below needs no overlap requirement)
grep -n "tick_key\|One replica per hostname" autumn/src/acme/tenant_domains.rs

# forget() clears only per-domain history; remove_if deletes the whole
# per-domain record outright, so a global window derived from per-domain
# records would silently drop an offboarded domain's still-counting attempts
grep -n "fn forget" -A 3 autumn/src/custom_domain.rs
grep -n "pub async fn remove_if" autumn/src/custom_domain.rs

# check() and record_attempt() are two separate calls with a real awaited
# gap between them (try_acquire, record_issuing_for) that the per-hostname
# lease does not close for two DIFFERENT hostnames — the check-then-act race
# a correct option 3 must close with an atomic reservation, not just storage
grep -n "self.limiter.check\|self.limiter.record_attempt\|record_issuing_for" \
  autumn/src/acme/tenant_domains.rs

# Negative search: no other hand-rolled, non-pluggable "shared state" limiter
# masquerading as cross-replica-safe was found in this pass. Each of these is
# correctly request- or process-scoped by its own doc comment, not a second
# instance of this problem:
grep -rln "struct.*Limiter\|struct.*Budget\|struct.*Quota" --include=*.rs . \
  | grep -v '/tests/\|test.rs\|target/'
sed -n '1,20p' autumn/src/plugin_sandbox/capability/quota.rs   # per-request/per-plugin, correctly process-scoped

# Sole-maintainer / no-second-team baseline this ADR reuses from ADR 0012.
# IMPORTANT: a shallow clone reports only its fetched slice — unshallow first,
# the same way ADR 0012's and ADR 0013's own reproduce steps do.
git rev-parse --is-shallow-repository   # if "true":
git fetch --unshallow origin
git log --format='%ae' | sort | uniq -c | sort -rn | head -5
```
