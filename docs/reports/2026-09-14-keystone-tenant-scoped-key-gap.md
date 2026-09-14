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

| Date fixed | Subsystem | Vulnerable code introduced | Commit | Bespoke fix shape |
|---|---|---|---|---|
| 2026-09-02 | `idempotency` (`AppBuilder::idempotent()`) | 2026-05-18, #779 (`75bc830a`) — 4 days *before* tenancy | `465229fa` (#2447) | `StorageKeyContext` struct gains a `tenant` field, captured once from the `CURRENT_TENANT` task-local (`autumn/src/idempotency.rs:144-180`) |
| 2026-09-07 | `#[cached]` (`autumn-macros::cached`) | 2026-03-29, #53 (`7e8d2763`) — ~2 months *before* tenancy | `96e7353f` (#2528) | Key becomes a tuple `(Option<String>, #key_args)`, the tenant read inline at macro-expansion time (`autumn-macros/src/cached.rs:620-627`) |
| 2026-09-09 | rate limiter (`AuthenticatedPrincipal` bucket) | 2026-05-30, #1001 (`77284d73`) — 8 days *after* tenancy | `ebf4a837` (#2653) | New free function `tenant_qualify_bucket_key`, a hand-rolled tagged-string encoding (`t<len>:`/`n:` prefixes) to keep tenant-present and tenant-absent keyspaces disjoint (`autumn/src/security/rate_limit.rs:784`) |

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

**Two of the three were legacy code no one revisited; the third shows the
gap isn't just a legacy retrofit problem.** `#[cached]` (2026-03-29) and
idempotency (2026-05-18) both predate tenancy (2026-05-22) and were never
swept when it shipped — the retrofit story. But the rate limiter's
specific vulnerable code, the `AuthenticatedPrincipal` key strategy, was
added in `77284d73` (2026-05-30, #1001) — **8 days after** tenancy already
existed in the same codebase — and still built its bucket key with no
tenant component. (The rate limiter's *original* commit, `68ccadab`,
2026-04-20, shipped only per-IP limiting; `AuthenticatedPrincipal` is a
separate, later addition and is the one this memo's fix touches.) That
rules out "just needs one sweep the week tenancy landed" as a sufficient
fix: this is not only about code written before a cross-cutting concern
existed, but about nothing in the framework prompting a developer adding
a *new* derived-key feature, afterward, to ask whether tenancy applies to
it. Each of the three was found only when a dedicated audit pass (the "Warden"
persona) happened to point at it — one every ~5 weeks on average since
tenancy landed, three in the last 7 days once that audit cycle reached this
class of bug. No compile-time or CI check catches this shape today: `autumn
cache audit` (`autumn-cli/src/cache_audit.rs`) proves cache
*invalidation* coverage, not key composition, and covers only `#[cached]`;
idempotency and rate-limiting have no equivalent gate at all.
`grep -rn "CURRENT_TENANT"` finds 142 lines across 25 files today — most
are the correct, declarative `#[repository(..., tenant_scoped)]` path; the
four discussed here are the ad hoc, imperative ones that middleware/macros
hand-roll for a derived key, which is exactly the shape with no gate. (A
narrower single-line pattern anchored on `.try_with`/`.with` undercounts
this: `idempotency.rs` and `cached.rs` both wrap the method call onto its
own line, so a single-line grep misses two of the four examples in this
memo entirely — a real limitation of grep-as-enforcement mechanism, not
just of this reproduce command; see Recommendation 2.)

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

Multi-tenancy is opt-in and cross-cutting: it applies to a derived-key
builder only if that builder's author remembers it applies. Two of the
three fixes are explained by a retrofit gap — the builder predates
tenancy and nobody swept it afterward. The third is not: rate limiting's
`AuthenticatedPrincipal` strategy was written 8 days *after* tenancy
already existed, by someone who had every opportunity to consult it, and
still didn't. So the mechanism is broader than "legacy code missed a
retrofit" — nothing in the framework requires *any* request-scoped
derived-key builder, written before or after tenancy landed, to declare
and be checked on whether it needs to fold in the ambient tenant. Each
instance is discovered by a human (or audit persona) reading the code
fresh, independently reinventing both the discovery and the fix. This is
the same shape as the
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

1. **Share only the pure encoding step — not how each site obtains the
   tenant.** The four sites do not agree on *how* they get an `Option<&str>`
   tenant: `idempotency.rs`, `cached.rs`, and `rate_limit.rs` each read the
   `CURRENT_TENANT` task-local directly, because each runs inside the async
   request-handling task where that task-local is valid. `plugin_sandbox`
   deliberately does **not** — `plugin.rs:573-580`'s own comment explains
   why: `namespaced_key`'s caller runs on a `spawn_blocking` worker thread,
   where the task-local is absent, so the tenant is read once on the
   request-handling task *before* `spawn_blocking` and threaded through
   explicitly (`runtime.tenant() -> Option<&str>`, and embedders can also
   supply one via `CapabilityServices::for_tenant`). A shared primitive must
   not fold that read in — a version of `namespaced_key` that called
   `CURRENT_TENANT` itself would resolve `None` on every call and collapse
   every tenant into one namespace. What *is* shareable is narrower: a pure
   function taking the `Option<&str>` each site already has in hand (e.g.
   `autumn_web::tenancy::fold_tenant_component(tenant: Option<&str>) ->
   Option<String>`) and returning the disjoint-namespace component. Nor can
   the four physical encodings simply be canonicalized onto that shared
   output: `idempotency.rs` carries its own regression test
   (`storage_key_without_a_resolved_tenant_is_unchanged`,
   `autumn/src/idempotency.rs:2154`) asserting its key stays byte-identical
   pre/post-fix so a retry doesn't turn into a fresh miss that replays a
   mutation, and `plugin_sandbox/capability/kv.rs:104`'s `namespaced_key` is
   itself the physical key a persistent KV backend stores plugin data under
   — changing its output format orphans existing stored data. Each site
   keeps its own existing physical format and its own existing way of
   obtaining the tenant; only the small pure encoding step is shared.
2. **Add a repo-hygiene test that pins the four *known* derived-key
   builders to the shared primitive, and name the gap it does not close.**
   The same idiom ADR 0013 proposed for `deny.toml`/`deny-sqlite.toml` — a
   test asserting `build_storage_key`, `generate_cache_body`'s key
   expression, `extract_key`/`resolve_key_and_params`, and `namespaced_key`
   each call the shared encoding primitive with whatever `Option<&str>`
   tenant they already obtained — catches a *regression* in one of these
   four. It does **not** catch a new, fifth derived-key builder that omits
   tenant-folding entirely: such a builder calls nothing this test looks
   for, so it adds no call site for an allow-list to flag — which is
   exactly the mechanism that let all four of today's builders ship
   unflagged in the first place. Closing that half of the gap needs
   enumerating derived-key *construction* (a new function whose return
   value backs a cache/dedup/rate-limit lookup), not `CURRENT_TENANT`
   reads — a harder, semantic check this memo does not design.

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

# The fourth, independent, correct-on-first-try encoding, and why it
# captures tenant explicitly instead of reading CURRENT_TENANT itself
git show -s --format='%h %ad %s' --date=short 847bd875
sed -n '95,115p' autumn/src/plugin_sandbox/capability/kv.rs
sed -n '565,585p' autumn/src/plugin_sandbox/plugin.rs   # capture before spawn_blocking, and why

# Tenancy landed 2026-05-22 (#876). #[cached] and idempotency predate it;
# rate limiting's per-IP infra also predates it, but the vulnerable
# AuthenticatedPrincipal strategy (#1001) was added 8 days AFTER it —
# ruling out "predates tenancy" as the whole mechanism.
git show -s --format='%h %ad %s' --date=short 15029e72 75bc830a 7e8d2763 68ccadab 77284d73

# No shared primitive exists today; every call site reads the task-local directly.
# (A pattern anchored on ".try_with"/".with" undercounts: idempotency.rs and
# cached.rs both wrap the method call onto the next line, so a single-line
# grep misses them. Use plain "CURRENT_TENANT" and read each hit.)
grep -rn "CURRENT_TENANT" --include="*.rs" autumn autumn-macros
grep -rln "CURRENT_TENANT" --include="*.rs" autumn autumn-macros | wc -l   # 25 files
grep -rn "CURRENT_TENANT" --include="*.rs" autumn autumn-macros | wc -l   # 142 lines

# The byte-compatibility constraints recommendation 1 must preserve
grep -n "storage_key_without_a_resolved_tenant_is_unchanged" -A 20 autumn/src/idempotency.rs
sed -n '95,111p' autumn/src/plugin_sandbox/capability/kv.rs   # namespaced_key is the physical stored key

# `autumn cache audit` proves invalidation coverage, not key composition,
# and has no equivalent for idempotency or rate-limiting
grep -n "idempotency\|rate_limit\|throttle" autumn-cli/src/cache_audit.rs   # zero hits

# The three write-ups this memo synthesizes
cat docs/security/2026-09-02-idempotency-tenant-scope/README.md
cat docs/security/2026-09-05-cached-tenant-key/README.md
cat docs/security/2026-09-09-rate-limit-tenant-key/README.md
```
