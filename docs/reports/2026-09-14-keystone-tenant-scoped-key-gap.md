# 🏛️ Keystone [findings]: three shipped cross-tenant leaks, three different fixes, no shared primitive for "fold the ambient tenant into a derived key"

- Status: Findings memo (not an RFC — see Reversibility)
- Date: 2026-09-14
- Author: Keystone (architecture review agent)

## 🎯 Scope

System boundary examined: framework subsystems that compute a **derived
key** from ambient request context — an idempotency storage slot, a
`#[cached]` memoization key, a rate-limit bucket, a plugin KV namespace —
to decide "is this the same logical operation/entity as last time," under
Autumn's opt-in, retrofitted row-level multi-tenancy
(`autumn/src/tenancy.rs`, ADR-less feature landed 2026-05-22, #876).

**Explicitly out of scope, and not an omission from this inventory:**
`autumn/src/cache/layer.rs`'s `CacheResponseLayer` also derives a key from
request context (URI path + query, `layer.rs:90-110,188-218`) — but by
documented, load-bearing contract, not by accident: its own rustdoc says
*"that last rule is the whole contract: the key is the URI and nothing
else... [wrapping a handler whose body varies per-visitor] leaks one
visitor's page to the next... So either keep per-visitor routes out of
this layer entirely, or give the cache a key that includes what the
response varies on."* It is the caller's job to never wrap a
tenant-varying handler in it, not the layer's job to fold in tenant — the
opposite design from the four sites below, which all promise tenant
isolation *automatically*. The 2026-09-02 idempotency report already
checked and excluded this exact surface for this exact reason
(`docs/security/2026-09-02-idempotency-tenant-scope/README.md:111-114`,
"Checked and **not** affected"); this memo does not re-litigate that call.

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
class of bug — no check caught any of them *before* that audit found
them and its fix shipped a dedicated regression test (see 🔧
Recommendation, item 2: all four now have one, running in CI). The gap is
narrower than "no CI check catches this shape" — it is "nothing catches
this shape in a derived-key builder that has not yet been through that
cycle." Separately, `autumn cache audit`
(`autumn-cli/src/cache_audit.rs`) proves cache *invalidation* coverage,
never key composition, for any cached read either way — and that
invalidation gate already reaches further than `#[cached]` alone, via
`declare_cached_read!` for the fragment and read-through caches
(`autumn/src/cache/coherence.rs:864-899`,
`docs/guide/cache-coherence.md:259-267`). Idempotency and rate-limiting
have no equivalent invalidation gate of any kind, but that is a distinct
point from tenant-key coverage, which no gate here or elsewhere checks.
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

**Reversibility: both items below turn out to need less doing than earlier
drafts of this memo claimed, not more — item 1 is a documentation
nice-to-have and item 2 is withdrawn outright, once existing test coverage
is accounted for.** Per this framework's own rule — *"if reversal costs
under ~2 engineer-weeks, the implementing team decides it in a PR
description"* — neither clears the bar for an RFC, and neither is even a
PR-sized ask by the time the corrections below are applied. Recorded as a
findings memo anyway because the connection across three
separately-audited-and-fixed CVE-shaped defects and one shared root cause
had not been made anywhere before this pass — the value here is in the
pattern-naming (📈 Evidence, 💡 Hypothesis), not in either recommendation
below.

1. **No single shared *encoding* primitive is actually viable across all
   four sites — downgraded to a documentation nice-to-have, not a fix.**
   The four sites' physical key formats are incompatible in kind, not just
   in tag scheme: `idempotency.rs`'s `build_storage_key` folds the tenant
   into a SHA-256 digest input (`push_storage_key_component`,
   `autumn/src/idempotency.rs:180-200`); `#[cached]`'s `make_cache_key`
   folds the `Option<String>` tenant (discriminant and value both) into a
   `DefaultHasher` hash of a Rust tuple (`autumn/src/cache/mod.rs:517-521`)
   — non-cryptographic, but not merely in-process either: when a shared
   Redis backend is registered, `autumn-cache-redis/src/lib.rs:307-318`'s
   `insert_raw_bytes` is "the primary write path for `#[cached]`-annotated
   functions," persisting this exact key string (with an optional,
   `#[cached]`-declared TTL — no expiry at all when none is set) as a real
   Redis key. Changing `make_cache_key`'s output format has the same
   stranded-data risk as idempotency's and plugin KV's byte-compatibility
   constraints below, not a weaker one; `rate_limit.rs`'s
   `tenant_qualify_bucket_key` emits a tagged plain string (`t<len>:`/`n:`
   prefixes); `plugin_sandbox`'s `namespaced_key` emits a colon-delimited
   string with each segment passed through `escape_segment`/
   `tenant_segment`. A function returning one canonical `Option<String>`
   for all four to fold in can't simultaneously produce four different
   physical shapes — the disjointness guarantee is exactly the part that
   has to stay bespoke per site. What's actually shareable shrinks to the
   trivial step of *obtaining* the raw tenant value, which is already
   close to a one-liner at three of the four sites
   (`CURRENT_TENANT.try_with(Clone::clone).ok().flatten()`) and an
   explicit parameter at the fourth (`plugin_sandbox`, which deliberately
   does **not** read `CURRENT_TENANT` — see below). De-duplicating one
   line has little value on its own. There is also a public-API asymmetry
   worth naming for whoever revisits this: `idempotency.rs`,
   `rate_limit.rs`, and `plugin_sandbox/capability/kv.rs` all live inside
   the `autumn` crate itself, so anything shared only among those three
   could stay `pub(crate)` — zero public-API cost, a genuine two-way door.
   `#[cached]`, however, is a proc macro (`autumn-macros`) whose *generated
   code* is inserted into whatever downstream crate calls it — any helper
   that generated code invokes must be Rust-visible as `pub` from there.
   That does *not* force it into the stable, SemVer-covered API, though:
   `STABILITY.md:78-82` explicitly excludes anything marked
   `#[doc(hidden)]` from stability guarantees, and `STABILITY.md:240` lists
   "tightening `#[doc(hidden)]` items or removing them entirely" as a
   non-breaking change outright — no deprecation ramp required.
   `autumn/src/counter_cache.rs:715,840,863,899,1204` already uses exactly
   this `#[doc(hidden)] pub fn`/`pub enum` pattern for macro-generated-code
   seams, so a new helper for `#[cached]` to call could follow the same
   precedent and stay a genuine two-way door. (An earlier draft of this
   memo claimed the opposite — that any symbol `#[cached]`'s expansion
   calls is necessarily gated by the deprecation ramp; that was wrong,
   corrected here rather than repeated.) The reason item 1 is still
   downgraded is the encoding-incompatibility point above, not this one.
2. **Withdrawn: the regression coverage this item proposed to add already
   exists, is stronger than what was proposed, and already runs in CI.**
   Each of the three fixes shipped its own dedicated behavioral test file
   alongside the code change — the same house practice, not something
   this memo needs to ask for: `idempotency_tenant_scope.rs`'s
   `header_tenancy_does_not_replay_across_tenants`
   (`autumn/tests/integration/idempotency_tenant_scope.rs:60-100`, opening
   comment: *"Warden 2026-09-02"*) drives two real requests as two tenants
   through the actual router and asserts tenant B never receives tenant
   A's cached body; `rate_limit_tenant_scope.rs` covers *both* call sites
   named above — `global_limiter_principal_bucket_isolated_by_tenant`
   (line 105) for `resolve_key_and_params` and
   `per_route_throttle_principal_bucket_isolated_by_tenant` (line 159) for
   `__check_throttle` — each asserting tenant B is never denied service by
   tenant A's exhausted bucket; `cached_tenant_scope.rs:133-187` does the
   same for `#[cached]`, gated `#[ignore = "requires Docker
   (testcontainers)"]` and swept by ci.yml's documented bare `--ignored`
   Docker sweep (see CLAUDE.md), not skipped; and
   `plugin_sandbox_capabilities.rs`'s
   `the_tenant_a_mounted_plugin_binds_to_is_the_requests_own` (line
   735-787) is, in its own words, *"the wiring nothing else in this suite
   reaches... delete that line and every other test here still passes
   while every tenant silently shares one namespace"* — asserting exactly
   one KV key per tenant, not one shared. That claim about "every other
   test" was correct only for the acquisition site it names
   (`plugin.rs:579-583`, reached through `serve`/`mounted_router`): it
   does **not** extend to `SandboxedPlugin::render_slot`
   (`plugin.rs:291-297`), which reads `CURRENT_TENANT` through its own,
   independent capture rather than the one `serve` uses. Every
   `render_slot` call in this test file (lines 859, 872, 875, 887, 933,
   943) runs outside a `with_tenant` scope — `with_tenant` appears exactly
   once in the whole file, in the `mounted_router` test — so no existing
   test would notice if `render_slot`'s capture were deleted, even though
   its output feeds the same `namespaced_key` encoder the mounted-router
   path does. Corrected claim: three of the four subsystems (idempotency,
   rate limiting at both call sites, `#[cached]`) and *one of
   `plugin_sandbox`'s two acquisition sites* have real, behavioral,
   end-to-end regression coverage running in CI today — strictly stronger
   proof than a structural hygiene test would have given, for the paths
   it actually covers. `render_slot`'s capture is a real, currently
   uncovered gap this memo did not know about until this review caught
   it; item 2 is still withdrawn as originally scoped (a source-shape
   hygiene test would not have caught this gap either — it isn't a
   missing call site, it's an existing call site with no behavioral
   test), but this specific gap is worth a targeted test in its own
   right, independent of anything this memo recommends.

What's left, once both items are corrected, is not a fix but an honest
gap statement: nothing in this framework prompts a *new* derived-key
builder to get the same behavioral-test treatment these four received
*after* being found vulnerable — that treatment is applied per-incident,
by the audit that found each bug, not by any standing requirement. A
fifth builder, written from scratch tomorrow, starts with zero coverage
of this shape until someone thinks to add it, the same way these four
did before 2026-09-02. Closing that requires recognizing *new* derived-key
*construction* — a function whose return value backs a cache, dedup, or
rate-limit lookup — as a class needing this treatment by default, which
is a semantic property no grep or call-site allow-list can detect. This
memo does not design that check; naming it as the actual unsolved half of
the gap, rather than proposing a hygiene test that duplicates existing
coverage, is the corrected conclusion.

This does not require resolving whether other not-yet-audited subsystems
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

# No shared primitive exists today. Three of the four sites read the
# task-local directly; plugin_sandbox's namespaced_key does NOT (it takes
# tenant as an explicit parameter — see plugin.rs:573-580 above), so its
# absence from this grep is expected, not evidence of ambient capture.
# (A pattern anchored on ".try_with"/".with" undercounts even the three
# ambient-read sites: idempotency.rs and cached.rs both wrap the method
# call onto the next line, so a single-line grep misses them. Use plain
# "CURRENT_TENANT" and read each hit.)
grep -rn "CURRENT_TENANT" --include="*.rs" autumn autumn-macros
grep -rln "CURRENT_TENANT" --include="*.rs" autumn autumn-macros | wc -l   # 25 files
grep -rn "CURRENT_TENANT" --include="*.rs" autumn autumn-macros | wc -l   # 142 lines

# The independent second rate-limit call site (__check_throttle), and its
# own dedicated behavioral test (per_route_throttle_principal_bucket_isolated_by_tenant)
sed -n '1684,1693p' autumn/src/security/rate_limit.rs
grep -n "per_route_throttle_principal_bucket_isolated_by_tenant" -A2 autumn/tests/integration/rate_limit_tenant_scope.rs

# The byte-compatibility constraints recommendation 1 must preserve,
# including #[cached]'s own persisted-backend case
grep -n "storage_key_without_a_resolved_tenant_is_unchanged" -A 20 autumn/src/idempotency.rs
sed -n '95,111p' autumn/src/plugin_sandbox/capability/kv.rs   # namespaced_key is the physical stored key
sed -n '306,318p' autumn-cache-redis/src/lib.rs   # insert_raw_bytes persists make_cache_key's exact output

# #[doc(hidden)] avoids the deprecation ramp entirely — an existing pattern
# for macro-generated-code seams, not a hypothetical
sed -n '78,82p;238,241p' STABILITY.md
grep -n "doc(hidden)" -A1 autumn/src/counter_cache.rs | grep -A1 "pub fn\|pub enum" | head -12

# Acquisition (reading CURRENT_TENANT) is a separate call from encoding at
# 3 of the 4 sites — pinning only the encoder misses a regression here
sed -n '154,166p' autumn/src/idempotency.rs        # StorageKeyContext::from_parts
sed -n '618,628p' autumn-macros/src/cached.rs       # read, then make_cache_key, two steps
sed -n '573,583p' autumn/src/plugin_sandbox/plugin.rs   # capture, then namespaced_key elsewhere

# But that regression is already caught: each fix shipped its own
# behavioral cross-tenant test, registered in tests/integration/mod.rs,
# stronger proof than a structural hygiene test would have given
sed -n '58,100p' autumn/tests/integration/idempotency_tenant_scope.rs
sed -n '104,155p;157,215p' autumn/tests/integration/rate_limit_tenant_scope.rs   # both call sites
sed -n '130,190p' autumn/tests/integration/cached_tenant_scope.rs   # #[ignore], swept by the Docker step
sed -n '735,787p' autumn/tests/integration/plugin_sandbox_capabilities.rs
grep -n "idempotency_tenant_scope\|rate_limit_tenant_scope\|cached_tenant_scope\|plugin_sandbox_capabilities" \
  autumn/tests/integration/mod.rs

# But that mounted_router test does not cover render_slot's own,
# independent ambient-tenant capture — a real, currently uncovered gap
sed -n '280,297p' autumn/src/plugin_sandbox/plugin.rs   # render_slot's separate capture
grep -n "with_tenant" autumn/tests/integration/plugin_sandbox_capabilities.rs   # exactly 1 hit, not near render_slot
grep -n "render_slot" autumn/tests/integration/plugin_sandbox_capabilities.rs   # none inside that 1 hit's scope

# `autumn cache audit` proves invalidation coverage, not key composition —
# and reaches beyond #[cached] via declare_cached_read! for fragment/
# read-through caches; still has no equivalent for idempotency or rate-limiting
sed -n '864,899p' autumn/src/cache/coherence.rs
grep -n "idempotency\|rate_limit\|throttle" autumn-cli/src/cache_audit.rs   # zero hits

# CacheResponseLayer is out of scope by documented, load-bearing contract
# (visitor-invariant by design), already checked and excluded in the
# original idempotency report
sed -n '85,111p' autumn/src/cache/layer.rs
sed -n '108,115p' docs/security/2026-09-02-idempotency-tenant-scope/README.md

# The three write-ups this memo synthesizes
cat docs/security/2026-09-02-idempotency-tenant-scope/README.md
cat docs/security/2026-09-05-cached-tenant-key/README.md
cat docs/security/2026-09-09-rate-limit-tenant-key/README.md
```
