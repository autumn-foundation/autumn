# 🏛️ Keystone [findings]: three shipped cross-tenant leaks, three different fixes, no shared primitive for "fold the ambient tenant into a derived key"

- Status: Findings memo (not an RFC — see Reversibility)
- Date: 2026-09-14
- Author: Keystone (architecture review agent)

## 🎯 Scope

System boundary examined: every framework subsystem that computes a
**derived key** from ambient request context — an idempotency storage slot,
a `#[cached]` memoization key, a rate-limit bucket — to decide "is this the
same logical operation as last time," under Autumn's opt-in, retrofitted
row-level multi-tenancy (`autumn/src/tenancy.rs`, ADR-less feature landed
2026-05-22, #876).

Reproduce every claim below with the commands in 🔬 Reproduce.

## 📈 Evidence (Tier 2 — repository record)

**Three independent, shipped cross-tenant defects, same shape, found one at
a time by manual security audit over 7 days, each fixed with its own
one-off encoding:**

| Date fixed | Subsystem | Built (pre-tenancy) | Commit | Bespoke fix shape |
|---|---|---|---|---|
| 2026-09-02 | `idempotency` (`AppBuilder::idempotent()`) | 2026-05-18, #779 (`75bc830a`) | `465229fa` (#2447) | `StorageKeyContext` struct gains a `tenant` field, captured once from the `CURRENT_TENANT` task-local (`autumn/src/idempotency.rs:144-180`) |
| 2026-09-07 | `#[cached]` (`autumn-macros::cached`) | 2026-03-29, #53 (`7e8d2763`) | `96e7353f` (#2528) | Key becomes a tuple `(Option<String>, #key_args)`, the tenant read inline at macro-expansion time (`autumn-macros/src/cached.rs:620-627`) |
| 2026-09-09 | rate limiter (`AuthenticatedPrincipal` bucket) | 2026-04-20 (`68ccadab`) | `ebf4a837` (#2653) | New free function `tenant_qualify_bucket_key`, a hand-rolled tagged-string encoding (`t<len>:`/`n:` prefixes) to keep tenant-present and tenant-absent keyspaces disjoint (`autumn/src/security/rate_limit.rs:784`) |

All three are documented in their own write-ups
(`docs/security/2026-09-02-idempotency-tenant-scope/README.md`,
`.../2026-09-05-cached-tenant-key/README.md`,
`.../2026-09-09-rate-limit-tenant-key/README.md`) as: *"Affected: `autumn-web`
0.7.0 and every earlier release"* — this is not hypothetical exposure, it
shipped in every release from each subsystem's introduction until its fix,
3-4 months in the oldest case.

**A fourth, independent data point — this time correct on the first try —
confirms there is still no shared primitive to reach for.** `autumn/src/plugin_sandbox/capability/kv.rs:104`,
added `847bd875` (2026-09-06, mid-week between the idempotency and cached
fixes), defines `pub fn namespaced_key(plugin: &str, tenant: Option<&str>, key: &str) -> String` —
a *fourth* distinct encoding of "fold an optional tenant into a derived
key," built from scratch rather than reused, because nothing in
`autumn_web::tenancy` offers a canonical one to call.

**All three broken subsystems predate tenancy itself**, which is the
mechanism, not coincidence: `#[cached]` (2026-03-29) and rate limiting
(2026-04-20) existed before `[tenancy]` did (2026-05-22); idempotency
(2026-05-18) landed four days before it. None was revisited when tenancy
shipped. Each was found only when a dedicated audit pass (the "Warden"
persona) happened to point at it — one every ~5 weeks on average since
tenancy landed, three in the last 7 days once that audit cycle reached this
class of bug. No compile-time or CI check catches this shape today: `autumn
cache audit` (`autumn-cli/src/cache_audit.rs`) proves cache
*invalidation* coverage, not key composition, and covers only `#[cached]`;
idempotency and rate-limiting have no equivalent gate at all.
`grep -rn "CURRENT_TENANT.try_with\|CURRENT_TENANT.with"` finds 50 call
sites across 24 files today — most are the correct, declarative
`#[repository(..., tenant_scoped)]` path; the four discussed here are the
ad hoc, imperative ones that middleware/macros hand-roll for a derived key,
which is exactly the shape with no gate.

## 🧭 Do nothing / decide later — 12-month baseline

The three known instances are fixed. The cost still being paid is the
*mechanism* that produced them: nothing stops a fifth. Autumn ships new
cross-cutting, request-scoped subsystems regularly — `#[endpoint]`/wire
contracts landed this week (`6e71bfb`, #2729) with an explicitly
self-flagged open edge of its own (rolling-deploy version skew,
unrelated to this finding but the same *pattern* of "ship first, audit the
cross-cutting property later"). If the next one memoizes or buckets by a
derived key under `[tenancy]`, it will be found the same way these three
were: a manual audit pass, sometime later, after shipping in at least one
release. That is a real, currently-paid recurring cost (audit-and-patch
cycles, ~1 per week during an active sweep), not a projected one — but it
is bounded and has caused no known production incident (no Tier-1 data
exists either way; this is a framework, not an operated service). Leaving
it alone costs nothing catastrophic; it costs one more audit-and-patch
cycle whenever the next such subsystem ships, same as the last three.

## 💡 Hypothesis

Multi-tenancy was added as an opt-in retrofit four months into the
project, after several request-scoped derived-key subsystems already
existed with no tenancy-aware contract. Nothing in the framework requires
a *new* request-scoped derived-key builder to declare, and be checked on,
whether it needs to fold in the ambient tenant — so each one is discovered
by a human (or audit persona) reading the code fresh, independently
reinventing both the discovery and the fix. This is the same shape as the
already-published `#[query_budget]` coverage-gap finding
(`docs/reports/2026-09-05-keystone-query-budget-coverage-gap-findings.md`):
a real property, no systematic enforcement, found piecemeal.

## 🔧 Recommendation — not a decision, and deliberately not an RFC

**Reversibility: two-way door, low-single-digit days.** Both items below
are additive to existing, already-shipped code; neither touches a public
API, a data-ownership boundary, or an external contract. Per this
framework's own rule — *"if reversal costs under ~2 engineer-weeks, the
implementing team decides it in a PR description"* — this does not clear
the bar for an RFC. Recorded as a findings memo because the connection
across three separately-audited-and-fixed CVE-shaped defects, one shared
root cause, and a live fourth reinvention had not been made anywhere before
this pass.

Concrete, PR-sized items for whoever picks this up next (maintainer, or the
Warden/Ledger personas):

1. **Collapse the four independent encodings onto one shared, documented
   primitive** in `autumn_web::tenancy` (e.g. a `fold_ambient_tenant(key:
   &str) -> String` or a small `TenantScopedKey` newtype), and have
   `idempotency.rs`, `cached.rs`, `rate_limit.rs`, and
   `plugin_sandbox/capability/kv.rs` call it instead of each keeping its own
   encoding. This does not change any of the three fixes' external
   behavior — it only removes the duplication that let them diverge.
2. **Add a repo-hygiene test** — the same idiom ADR 0013 already proposed
   for `deny.toml`/`deny-sqlite.toml` — that enumerates known
   `CURRENT_TENANT.try_with`/`.with` call sites (today: the ones named
   above, plus the legitimate declarative ones in `repository.rs`,
   `model.rs`, `tenant_cell.rs`, `sharding.rs`, `commentable.rs`) against an
   explicit allow-list, and fails when a *new* call site appears outside
   it. A new site isn't necessarily wrong, but it should force a conscious
   "does this need the shared primitive?" decision at review time instead
   of shipping silently, the way all four discussed here did.

Neither item requires resolving whether other not-yet-audited subsystems
have the same gap today — that would be a fresh audit, not an
architecture decision, and this memo does not claim to have performed one.

## 🔬 Reproduce

```bash
# The three fixes and their bespoke shapes
git show --stat 465229fa 96e7353f ebf4a837   # #2447, #2528, #2653
sed -n '144,180p' autumn/src/idempotency.rs
sed -n '610,630p' autumn-macros/src/cached.rs
sed -n '780,800p' autumn/src/security/rate_limit.rs

# The fourth, independent, correct-on-first-try encoding
git show -s --format='%h %ad %s' --date=short 847bd875
sed -n '95,115p' autumn/src/plugin_sandbox/capability/kv.rs

# Each broken subsystem predates tenancy (2026-05-22, #876)
git show -s --format='%h %ad %s' --date=short 15029e72 75bc830a 7e8d2763 68ccadab

# No shared primitive exists today; every call site reads the task-local directly
grep -rn "CURRENT_TENANT.try_with\|CURRENT_TENANT.with" --include="*.rs" autumn autumn-macros

# `autumn cache audit` proves invalidation coverage, not key composition,
# and has no equivalent for idempotency or rate-limiting
grep -n "idempotency\|rate_limit\|throttle" autumn-cli/src/cache_audit.rs   # zero hits

# The three write-ups this memo synthesizes
cat docs/security/2026-09-02-idempotency-tenant-scope/README.md
cat docs/security/2026-09-05-cached-tenant-key/README.md
cat docs/security/2026-09-09-rate-limit-tenant-key/README.md
```
