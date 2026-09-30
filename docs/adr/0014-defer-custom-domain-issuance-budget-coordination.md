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
originally described. `check` (`tenant_domains.rs:851`) runs before the
per-hostname fleet lease is even acquired, and `record_attempt`
(`tenant_domains.rs:931`) runs after two subsequent awaited calls
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
   plus a `Vec<i64>`), rebuilt empty on every process start — `app.rs`
   constructs a fresh `IssuanceLimiter` at both of its call sites
   (`autumn/src/app.rs:10126,10200`, one for the one-shot retention pruner,
   one for the running web app's custom-domain task).
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
   `CustomDomainRegistry::remove_if` (`custom_domain.rs:1670`) deletes a
   domain's record outright, while `IssuanceLimiter::forget`
   (`custom_domain.rs:2010`) only clears that domain's *per-domain* history
   and intentionally leaves its attempts in the global vector — so a global
   window reconstructed from per-domain records would lose an offboarded
   domain's still-counting attempts on the next restart, letting the
   deployment-wide budget run over exactly the case it exists to catch.
4. **This is not a new architecture question — ADR 0004 already answered
   it in the abstract — and this ADR reconciles with, rather than silently
   contradicts, ADR 0004's Category 3 "must."** ADR 0004 ("Externalize
   Distributed Runtime State", 2026-04-09, Status: Proposed) classifies
   "rate-limit counters" — which `IssuanceLimiter` structurally is — as
   Category 3 ("Shared Mutable Runtime State"), and its Decision section
   states plainly: "This must use pluggable external backends in
   production-safe deployments." Read on its own, that "must" is in direct
   tension with this ADR's default path, which leaves the limiter
   per-process — that tension is real and is not resolved by citing ADR
   0004's risks in isolation, as an earlier draft of this ADR did. Read in
   the context ADR 0004 itself gives that sentence, the tension resolves
   instead of just being cited past: the same document names "Supporting
   every possible cache or session backend in Phase 1" a Non-Goal, names
   "adding too many backends too early" and "over-externalizing" as Risks
   to actively avoid, and stages its own rollout one target at a time —
   "the first mandatory externalization target is session state," with the
   rest of Category 3 following as evidence of real need arrives, not as a
   single simultaneous mandate the ADR's date already forces.
   `autumn/src/security/rate_limit.rs` is the proof that rollout happens:
   its `"redis"` backend, documented as "share the budget across all pods,"
   *is* ADR 0004's Category-3 answer, built once a real fleet-wide need
   existed for that specific counter — durable-state option 3 for
   `IssuanceLimiter` would be the same answer, not a cheaper substitute for
   it (see Default path below, which does not claim otherwise). No
   comparable evidenced need has reached `IssuanceLimiter` yet (Evidence
   items 6-7): the default value is miscalibrated, not the architecture
   proven wrong. **This ADR is therefore a scoped, evidence-gated exception
   to ADR 0004's Category 3, not a supersession of it**: it defers *when*
   `IssuanceLimiter` gets a pluggable external backend, using exactly the
   criterion — a real, evidenced fleet-wide need — that ADR 0004's own
   Risks and Non-Goals sections say should gate that decision. It does not
   dispute that the limiter eventually belongs on that backend. The Trigger
   section below is what turns "eventually" into "now."
5. **The seam for fleet-wide coordination, if it is ever needed, already
   exists and needs nothing new built to stay open.** ADR 0010 ("Expose an
   App-Facing Distributed Lock", accepted 2026-07-09) already generalized
   the framework's internal Postgres-advisory-lock primitive
   (`try_pg_advisory_lock`) into an app-facing "run this exactly once across
   the cluster" seam. A future option-3 implementation would reach for that
   primitive rather than inventing a fourth coordination mechanism.
6. **No evidence of multi-replica custom-domain deployment exists in this
   repository — and item 7 shows that is stronger than an absence of
   evidence.** Per ADR 0012's Tier-2 finding, 89.4% of this repository's
   commits are from one human author with no second team; nothing in the
   issue tracker, CI matrix, or example apps demonstrates an operator
   running ≥2 replicas of this feature concurrently, for the same domain or
   different ones (overlap does not matter — see item 7 and the Trigger
   section). Item 2 in #2644 is a real gap in what the code *claims*
   (`global_per_hour` "enforced per process-lifetime-hour, not per hour"),
   and item 7 below shows the exact arithmetic of that gap — but also
   surfaces documented, positive evidence (not merely an absence) that this
   deployment shape does not work at all today, for a reason unrelated to
   the budget. That reclassifies this from "thinnest link, watch for a
   report" to "not currently reachable" — see item 7 and Default path for
   what that changes; the revisit trigger below still fires on the first
   report regardless, since an operator can misconfigure past a
   documented recommendation.
7. **The limiter's own "well inside [the limit]" claim holds only for one
   process, not for the account — but one process is the only topology this
   feature supports today.** The shipped defaults are
   `issuance_per_domain_per_day = 5` and `issuance_global_per_hour = 50`
   (`config.rs:10228-10235`). At 50/hour, one process can attempt 150 orders
   in any rolling 3-hour window — the same window Let's Encrypt measures
   its 300-orders-per-account limit over. Two replicas, each independently
   enforcing that same per-process ceiling (the durable-state-option-1 fix
   makes each one *reliable*, not smaller), together reach 150 + 150 = 300
   orders in 3 hours: the account's entire quota, before counting a single
   order from any other source. Three replicas reach 450 — 1.5× over. "Well
   inside the limit... at the defaults" is true at N=1 and false starting
   at N=2 — but `docs/guide/tls.md:901-907` and `:426-427` already document,
   independently of this ADR, that custom domains are **single-host only**:
   "the HTTP-01 token map and the certificate store are per-process, so
   behind a load balancer the CA's validation request usually reaches a
   replica that never published the token. Run custom domains on a single
   host." A deployment cannot run this feature correctly on ≥2 replicas
   today for a reason that has nothing to do with the issuance budget —
   HTTP-01 validation itself breaks first, independent of `global_per_hour`.
   N=2's arithmetic failure is real and worth recording, but it describes a
   topology the framework does not yet support, not a live exposure in a
   supported one (see Impact floor and Default path below for what that
   changes).

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
would get up to N× the advertised `global_per_hour` budget, same as today,
where N is replica count — and per Evidence item 7, N=2 already *equals*
Let's Encrypt's outer limit (300 orders/3h/account) rather than staying
safely under it. That arithmetic is real, but "would" is the operative
word: `docs/guide/tls.md` already documents custom domains as single-host
only, for the independent reason that HTTP-01 validation itself does not
survive a load balancer today. Nothing in the record shows a deployment
currently running ≥2 replicas of this feature (Evidence item 6), and
nothing *could* run it successfully today even if it tried — so the
12-month baseline for the issuance-budget gap specifically is: nothing,
because the prerequisite feature (multi-replica custom domains at all)
does not exist yet. That is not true of items 1, 3, and 4, which are live
single-instance bugs today regardless. The N=2 arithmetic is recorded here
so that whoever eventually externalizes the HTTP-01 token map and
certificate store — the actual multi-replica prerequisite,
`docs/guide/tls.md:901-907`'s documented gap, with no tracked issue found
for it in this pass — does not also have to rediscover that the issuance
budget default needs the same trip.

## Impact floor check

Five of the six clearing conditions are not met for *building durable-state
option 3*: no Tier-1 incident data (no reported case of multi-replica
budget overrun); no cross-team change count to reduce (single-maintainer
repository, per ADR 0012); no dated Tier-4 fact — Let's Encrypt's limit is
real, but per Evidence item 7 the configuration that would consume it
(≥2 replicas) is not one this feature supports today, and nothing assigns
a date or an owner to closing that anyway; no removed cost exceeding
a migration cost (no cost is currently being paid); no ≥3-data-point
asymptotic trend (this is the first and only instance of a hand-rolled,
non-pluggable "shared mutable runtime state" counter found in this pass —
see Reproduce for the negative search across
`plugin_sandbox::capability::quota` and other `*Limiter`/`*Budget`/`*Quota`
structs, all of which are correctly request- or process-scoped by design,
not mis-scoped copies of this same problem).

**The sixth — a Tier-3 spike, or equivalent computed proof, showing the
design fails a committed requirement — deserves a real look, and a first
pass through this review concluded it was met. On closer reading it is
not, which is worth recording rather than quietly dropping.** Evidence item
7's arithmetic (N=2 replicas exactly consume Let's Encrypt's 3-hour limit)
looked like exactly the proof condition 4 asks for: not "might struggle
with," but an exact failure of the limiter's own documented guarantee at a
specific, named N. But a "committed requirement" a design can fail has to
describe a configuration the design is actually committed to supporting —
and `docs/guide/tls.md:901-907` already documents, independently of this
ADR and for an unrelated reason (HTTP-01 token-map and certificate-store
locality), that custom domains run single-host only. The limiter's "well
inside the limit... at the defaults" guarantee is implicitly scoped to the
one topology this feature supports, N=1, where the arithmetic holds
exactly as claimed. Item 7's N=2 case is real arithmetic about an
unreachable configuration, not a proof the shipped, supported design fails
its own requirement. Condition 4 is therefore **not** met after all — no
Tier-3-equivalent proof exists against the design this feature is
committed to today. Combined with the other five conditions above, none of
the six are met: this does not clear the floor for durable-state option 3,
and per Evidence item 4 that non-clearance is an explicit, evidence-gated
exception to ADR 0004's Category 3 "must," not a claim that
`IssuanceLimiter` never needs a pluggable backend — only that no evidenced,
*reachable* need has reached it yet.

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

No fifth action item is added. An earlier pass through this review proposed
lowering `default_custom_domains_global_per_hour` (`config.rs:10233`,
currently `50`) to cover a multi-replica margin — but per Evidence item 7,
custom domains are documented single-host only for a reason (HTTP-01
token-map and certificate-store locality) this ADR's scope does not touch,
so a deployment running ≥2 replicas of this feature does not exist to
protect. Lowering the default today would only shrink issuance throughput
for the one topology this feature actually supports, in exchange for
guarding against a topology that cannot run it regardless — the same
"narrows options for a benefit you could not measure" mistake this
framework's own banned-changes list warns against. The right trigger for
recalibrating `global_per_hour` is not this ADR's trigger below; it is
whoever externalizes the HTTP-01 token map and certificate store to make
multi-replica custom domains real, who should read Evidence item 7 before
shipping that and pick a default that accounts for it then.

## Seam kept open

One seam already exists; one small seam is new but ships as part of the
default path's ordinary bug fix, not as something option 3 must add later:

- `CustomDomainStore` already abstracts *per-domain* persistence behind a
  trait with swappable implementations (`load_all`/`save`/`delete` on one
  `CustomDomain` at a time), so a future backend for the per-domain half of
  the attempt log is additive, not a rewrite of `IssuanceLimiter`'s call
  sites. It cannot serve the global window, though: it has no operation for
  a single deployment-wide value, only per-record ones. The default path's
  own small durable store for the global window (see Default path above) is
  therefore a new seam — but it is built now, as part of the ordinary fix
  for items 1/3/4, not deferred alongside option 3. Once it exists, a later
  option-3 implementation extends it rather than inventing persistence from
  scratch.
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
any of the following occurs:

- **The HTTP-01 token map and certificate store are externalized**, making
  multi-replica custom domains a supported topology for the first time
  (`docs/guide/tls.md:901-907` is the documented gap this closes). At that
  point Evidence item 7's N=2 arithmetic stops being about an unreachable
  configuration and starts being about the one operators can actually run —
  recalibrate `global_per_hour`'s default and re-open this decision before
  declaring that support production-safe, not after.
- An operator reports running ≥2 replicas that concurrently issue
  custom-domain certificates today, despite the documented single-host
  requirement — for the same hostname or different ones. The fleet lease in
  `tenant_domains.rs::issue_one` is keyed per-hostname
  (`format!("custom-domain:{hostname}")`, "one replica per hostname
  orders"), so it excludes a second replica from racing the *same* domain
  but does nothing to stop two replicas issuing for *different* domains at
  the same time, each checked against its own process-local
  `global_per_hour` allowance. Overlapping domain sets are not required for
  the per-process budget gap (#2644 item 2) to produce observed
  over-issuance — disjoint domains issued concurrently across replicas
  already multiply the advertised budget by replica count, on top of
  whatever HTTP-01 validation failures that unsupported topology already
  causes.
- A deployment is documented approaching Let's Encrypt's outer
  300-orders-per-3-hours account limit some other way, making this
  framework's own budget (rather than the vendor's) the thing that needs to
  hold exactly, not just approximately.

## Reproduce

```bash
# The staged decision and its three options, in the maintainer's own words
# (gh not available in this environment; fetched via the GitHub MCP tool)
# https://github.com/autumn-foundation/autumn/issues/2644

# The in-process-only limiter state
sed -n '1890,1920p' autumn/src/custom_domain.rs

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

# The shipped default: 50/hour means 150 orders per process per 3-hour
# window — two replicas already sum to Let's Encrypt's entire 300 limit
grep -n "fn default_custom_domains_global_per_hour" -A 3 autumn/src/config.rs

# But custom domains are documented single-host only, for a reason (HTTP-01
# token-map/cert-store locality) unrelated to the issuance budget — the N=2
# arithmetic above describes a topology this feature does not support today
grep -n "single-host\|single host\|token map" docs/guide/tls.md

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
