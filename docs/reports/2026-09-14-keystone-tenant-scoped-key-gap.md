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

**Reversibility: two-way door, low-single-digit days for item 2; item 1 is
downgraded below to a documentation nice-to-have, for reasons that also
bear on its own reversibility.** Per this framework's own rule — *"if
reversal costs under ~2 engineer-weeks, the implementing team decides it
in a PR description"* — even the more expensive reading below does not
clear the bar for an RFC. Recorded as a findings memo because the
connection across three separately-audited-and-fixed CVE-shaped defects,
one shared root cause, and a live fourth reinvention had not been made
anywhere before this pass.

1. **No single shared *encoding* primitive is actually viable across all
   four sites — downgraded to a documentation nice-to-have, not a fix.**
   The four sites' physical key formats are incompatible in kind, not just
   in tag scheme: `idempotency.rs`'s `build_storage_key` folds the tenant
   into a SHA-256 digest input (`push_storage_key_component`,
   `autumn/src/idempotency.rs:180-200`); `#[cached]`'s `make_cache_key`
   folds the `Option<String>` tenant (discriminant and value both) into a
   `DefaultHasher` hash of a Rust tuple — non-cryptographic, never
   persisted (`autumn/src/cache/mod.rs:517-521`); `rate_limit.rs`'s
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
   that generated code invokes must be `pub` in `autumn_web` (`tenancy` is
   already `pub mod`, `autumn/src/lib.rs:639`), and removing a `pub` symbol
   later is a breaking change gated by STABILITY.md's deprecation ramp
   (a full minor cycle), not an instant revert. So the "neither item
   touches a public API" claim in an earlier draft of this memo was wrong
   for any version of item 1 that wires `#[cached]` to a shared helper;
   corrected here rather than repeated.
2. **Add a repo-hygiene test that pins the four *known* derived-key call
   sites — all four of them — to tenant-folding, and name the gap it does
   not close.** The same idiom ADR 0013 proposed for
   `deny.toml`/`deny-sqlite.toml` — a test asserting each of
   `build_storage_key`, `generate_cache_body`'s key expression,
   `resolve_key_and_params` (the global tower layer, calling
   `tenant_qualify_bucket_key` at its own call site), `__check_throttle`
   (the per-route `#[throttle(key = "principal")]` guard, an *independent*
   second call to `tenant_qualify_bucket_key` at
   `autumn/src/security/rate_limit.rs:1692`, not reachable through
   `resolve_key_and_params`), and `namespaced_key` still folds in tenant —
   catches a *regression* in one of these five call sites (two of them in
   rate limiting alone). It does **not** catch a new, sixth derived-key
   builder that omits tenant-folding entirely: such a builder calls
   nothing this test looks for, so it adds no call site for an allow-list
   to flag — which is exactly the mechanism that let all of today's
   builders ship unflagged in the first place. Closing that half of the
   gap needs enumerating derived-key *construction* (a new function whose
   return value backs a cache/dedup/rate-limit lookup), not
   `CURRENT_TENANT` reads — a harder, semantic check this memo does not
   design. This item, not item 1, is the one with real teeth.

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

# The independent second rate-limit call site recommendation 2 must also pin
sed -n '1684,1693p' autumn/src/security/rate_limit.rs   # __check_throttle

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
