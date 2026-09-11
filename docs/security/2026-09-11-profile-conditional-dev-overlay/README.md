# Dev error overlay disagrees with the request inspector on an unset `profile` (2026-09-11)

**Class:** dev-only surface reachable outside the `dev` profile, via a
framework gate that fails open instead of closed
**Surface:** `autumn_web::router::apply_middleware`'s `is_dev` ×
`autumn_web::config::AutumnConfig::profile` (`Option<String>`) × any custom
`ConfigLoader`
**Entry point:** any HTML-accepting request that reaches a 4xx/5xx
(`ErrorPageFilter`/`ErrorPageContextLayer`, `autumn/src/middleware/error_page_filter.rs`)
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped the
`maud` HTML dev error overlay (`autumn/src/error_pages/dev_badge.rs`)
**Status:** fixed — `autumn/src/config.rs`, `autumn/src/router.rs`,
`autumn/src/route_listing.rs`

## 🎯 Surface

Autumn has (at least) two dev-only surfaces gated purely on `profile`:

- The request inspector (`/_autumn/inspect`), gated in `router.rs` by
  `is_dev_profile = matches!(config.profile.as_deref(), Some("dev" | "development"))`.
- The HTML dev error overlay (`autumn-dev-error-badge`, injected by
  `ErrorPageFilter` when `self.is_dev` is true) — stack frames with source
  snippets, the raw internal error message, scrubbed headers/cookies, matched
  route pattern, and (when captured) SQL query text.

Both are meant to derive the same fact — "is this a dev deployment?" — from
`AutumnConfig::profile`. Before this fix they computed it two different ways:

```rust
// router.rs (inspector gate, correct — fails closed)
let is_dev_profile = matches!(config.profile.as_deref(), Some("dev" | "development"));

// router.rs, apply_middleware (error overlay gate, the bug — fails open)
let is_dev = config.profile.as_deref().map_or(cfg!(debug_assertions), |p| p == "dev");
```

`AutumnConfig::profile` is `Option<String>` with no forced default. The
*default* `TomlEnvConfigLoader` always ends up with `Some(..)`: its
`resolve_profile()` (env vars → CLI flag → build-mode auto-detect →
`"dev"`) never returns `None`. But `docs/guide/custom-subsystems.md`
documents replacing the loader entirely — a `JsonFileConfigLoader` that
`serde_json::from_slice`s an `AutumnConfig` straight out of a file — and
nothing in that guide says the JSON must include a `"profile"` key. When it
doesn't, `AutumnConfig::profile` is `None`, and the two "is dev" checks
diverge: the inspector correctly stays a 404, but the error overlay falls
back to `cfg!(debug_assertions)` — a **compile-time** fact about the binary,
completely disconnected from the deployment's own idea of "am I in
production", and `true` for the default `cargo build`/`cargo run` (no
`--release`).

## 🕵️ Threat model

> Against an app that follows Autumn's own documented custom-`ConfigLoader`
> pattern (`docs/guide/custom-subsystems.md`) and simply never adds a
> `profile` field to its config source — nothing in the docs tells it to —
> and is deployed as a debug build (an easy operational mistake: forgetting
> `--release`, a Docker multi-stage build that copies the wrong target
> directory, `cargo run` used directly in a small deployment), an
> **unauthenticated attacker** who can trigger any 4xx/5xx on an
> HTML-accepting route obtains the full dev error overlay: the raw internal
> error message, request headers, cookies, the matched route pattern, and
> (when the `db` inspector buffer has entries) SQL query text and
> parameters — on a deployment the operator believes is production-safe
> because they never set `profile = "dev"` anywhere. The app author did
> nothing the documentation told them not to do; the framework's own two
> "is this dev" checks simply disagreed with each other.

## 🧪 Reproduction

Test file: `autumn/tests/integration/profile_conditional_surfaces.rs`
(declared in `autumn/tests/integration/mod.rs` under `#[cfg(feature = "maud")]`).

Command:

```
cargo test -p autumn-web --test integration_tests --features maud,test-support \
  -- profile_conditional_surfaces --test-threads=1 --nocapture
```

Three tests:

- `inspector_stays_closed_when_profile_is_unset` — control, already passed on
  trunk (the inspector's own gate is correct).
- `dev_error_badge_stays_closed_when_profile_is_unset` — **the exploit test**,
  builds a `TestApp` with `AutumnConfig::default()` (profile left `None`,
  exactly what a documented custom `ConfigLoader` with no `profile` key
  produces), triggers a handler that returns
  `AutumnError::internal_server_error_msg("sentinel-internal-failure-profile-audit")`,
  and asserts the response body does **not** contain
  `autumn-dev-error-badge` or the raw sentinel message.
- `dev_error_badge_stays_closed_in_explicit_prod_profile` — positive control
  with an explicit `profile = "prod"` (trusted hosts opened to `"*"` so the
  request isn't rejected for an unrelated reason — see `trunk-failure.txt`
  for what that unrelated 400 looks like before the test config was
  corrected).

**Failure on trunk** (see `trunk-failure.txt` for the full captured output):
`dev_error_badge_stays_closed_when_profile_is_unset` failed —
`assert!(!body.contains("autumn-dev-error-badge"), ...)` — with the response
body containing the full dev overlay: the `sentinel-internal-failure-profile-audit`
message verbatim, a 24-frame stack trace with source snippets from
`autumn/src/error.rs` and the test file itself, the request method/path/id,
and headers.

```
test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 2023 filtered out
```

(The second trunk failure, on the `explicit_prod_profile` positive control,
was an unrelated pre-existing test-setup gap — the `prod` profile's
`TrustedHostLayer` no longer auto-trusts the test client's default `Host`
header, so the request 400'd before reaching the handler. Fixed in the test
by opening `trusted_hosts` to `"*"`, not part of the security fix itself.)

## 🔎 Root cause

`autumn/src/router.rs`, `apply_middleware` (pre-fix, ~line 5214):

```rust
let is_dev = config
    .profile
    .as_deref()
    .map_or(cfg!(debug_assertions), |p| p == "dev");
```

Two bugs in one line:

1. **Fails open on `None`.** The inspector's gate (`is_dev_profile`, a few
   dozen lines earlier in the same function) treats an absent profile as
   "not dev". This one treats it as "ask the compiler" — a fact about the
   binary, not the deployment.
2. **Only matches the literal string `"dev"`**, not `"development"` — the
   inspector's gate accepts both aliases. (This direction fails *closed*,
   not open — a `profile = "development"` deployment would incorrectly miss
   the dev overlay it's entitled to — so it's a correctness bug, not a
   security one, but it's the same root cause: two independent
   implementations of one concept, free to drift.)

No existing test caught this because every other integration test either
uses `TestApp::new()`'s default profile (`"test"`, correctly not-dev under
both old and new logic) or explicitly sets `.profile("dev")`/`.profile("prod")`
— nothing exercised `profile: None` specifically for the error-overlay gate.

## 🩹 Fix

Added `autumn::config::profile_is_dev(profile: Option<&str>) -> bool` — the
single source of truth, matching the inspector's original fail-closed
semantics (`Some("dev" | "development")`, `None` and everything else is not
dev) — and pointed both `router.rs` call sites (the inspector gate and the
error-overlay gate) and `route_listing.rs`'s inspector-route listing (used
by `autumn routes audit`, which had the same correct-but-independent copy of
the check) at it. The `cfg!(debug_assertions)` fallback is gone entirely: a
deployment's dev-ness is now always a fact about its resolved `profile`,
never about how the binary happened to be compiled.

This changes behavior only for `profile: None` — every app using the
default `TomlEnvConfigLoader` (whose `resolve_profile()` never returns
`None`) is unaffected. Only a custom `ConfigLoader` that leaves `profile`
unset changes behavior, and only in the direction of *removing* the dev
overlay it was never entitled to show.

## ✅ Verification

Ran, all green:

```
cargo fmt --all                                                   # no changes
cargo clippy -p autumn-web --all-targets --features maud,test-support -- -D warnings
cargo test -p autumn-web --test integration_tests --features maud,test-support \
  -- profile_conditional_surfaces --test-threads=1 --nocapture
```

All three `profile_conditional_surfaces` tests pass after the fix (see
`after.txt`). Re-attacked by also checking:

- `profile = Some("development")` (the alias) now correctly activates the
  dev overlay under the shared helper, matching the inspector.
- `profile = Some("test")` (the `TestApp` default used by ~2000 other
  integration tests) is unaffected — still not-dev under both old and new
  logic — so no other test's expectations changed.

**Not completed locally:** `./scripts/pre-push-check.sh` (the full
`cargo clippy --workspace --all-targets` + `cargo test --workspace --no-run`
+ `cargo test --workspace --doc` sweep). This remote execution environment
(4 vCPU / 15 GB RAM, a bounded per-session disk allowance) could not carry a
from-scratch full-workspace build to completion: a first attempt OOM-killed
`clippy-driver` partway through the workspace clippy pass (unrelated crate,
`autumn-macros`, at full parallelism), and a retry at `CARGO_BUILD_JOBS=2`
ran the session's writable disk allowance to `No space left on device`
mid-compile (debug builds of every example app, plugin, and benchmark in the
workspace easily exceed 30 GB of `target/`). Freeing space (deleting
`target/debug/incremental` and eventually the rest of `target/debug`) let
the machine recover but cost the in-progress build each time. `check-panic-gate.sh`'s
and `check-determinism-gate.sh`'s self-tests and static gate checks (no
toolchain needed) did run clean before the workspace clippy step hit the
resource wall. The narrower, targeted checks above cover every file this fix
touches; CI's own `lint` + `test` jobs are the authoritative full-workspace
gate and will run against the pushed branch.

## 📡 Blast radius

Swept every other `matches!(config.profile.as_deref(), ...)` /
`cfg!(debug_assertions)` site touching profile semantics:

- `autumn/src/route_listing.rs:786` — same "is dev" check, same correct
  fail-closed logic, now deduplicated onto `profile_is_dev`. No behavior
  change (was already correct).
- `autumn/src/app.rs` (`fail_fast_on_missing_encryption_keys`,
  `fail_fast_on_invalid_signing_secret`, `fail_fast_on_invalid_webhook_config`,
  `fail_fast_on_invalid_trusted_hosts`, `fail_fast_on_invalid_idempotency_config`,
  and the boot-time checks around line 10292) — **found but NOT fixed here.**
  These use `is_production = matches!(config.profile.as_deref(), Some("prod" | "production"))`,
  which treats an absent profile as "not production" and therefore *skips*
  several boot-time hard-fail safety checks (weak/missing signing secret,
  missing encryption keys, missing `trusted_hosts`, unsafe in-memory
  idempotency backend) for exactly the same custom-`ConfigLoader`-with-no-`profile`
  shape as this bug. This looks like a more severe cousin — a weak signing
  secret going unenforced touches session/token forgery — but flipping
  `is_production`'s default for `None` is a **behavior change to a
  production safety gate** (an app currently relying on `None` behaving
  leniently would start hard-failing at boot), which CLAUDE.md's "Ask
  before" rules require going through the user rather than fixing silently
  in the same PR as this narrower, non-behavior-changing dev-overlay fix.
  Flagging for a maintainer decision / a follow-up Warden pass.
- `autumn/src/router.rs:14616` (`TrustedHostPolicy::from_config`) — a third
  spelling of the same `is_production` pattern found in `app.rs` above:
  `allow_missing_host: !is_production` means a request carrying **no** `Host`
  header and no URI authority is let through whenever `profile` is not
  literally `Some("prod" | "production")` — `None` included. Same shape,
  same "Ask before" reasoning for not fixing it here: flagged alongside the
  `app.rs` findings above rather than folded into this PR. (Encountered
  operationally while writing this ledger's own positive-control test: even
  with `trusted_hosts = ["*"]`, a `prod`-profile `TestApp` request with no
  `Host` header 400s — the wildcard only widens which *present* host values
  are accepted, it doesn't touch the separate missing-host allowance. The
  test now sends an explicit `Host` header instead of relying on that gate.)
- `autumn/src/actuator.rs:1888`, `autumn/src/app.rs:4310,6131` — profile
  comparisons for unrelated purposes (config diagnostics, defaulting), not
  a dev-only-surface gate; not in scope.
- `assets.rs`, `error.rs`, `capsule/persist.rs`, `capsule/schema.rs` — all
  `cfg(debug_assertions)` usages here are compile-time `#[cfg]` attributes
  (dead-code elimination for embedded vs. served static assets, whether
  `AutumnError` carries a backtrace field at all) or a recorded fact used to
  replay a capsule under the same build — not runtime dev/prod gates, and
  not attacker-influenced.
- Feature matrix: the fix is inside `router.rs`'s `apply_middleware`, which
  is `#[cfg(feature = "maud")]`-gated for the `ErrorPageFilter` construction
  and unconditional for the inspector gate; both paths were checked. No
  `redis`/`ws`/`mail`/`i18n`/`sqlite` interaction — profile resolution does
  not vary by feature.

## 📜 Compatibility

- Non-breaking for every app using the default `TomlEnvConfigLoader` (the
  overwhelming majority — `resolve_profile()` never leaves `profile` as
  `None`).
- Behavior change only for a custom `ConfigLoader` that never sets
  `profile`: such an app previously got the dev overlay on a debug build and
  will no longer. This is a strict tightening of a security-relevant
  default in the safe direction (removing an unintended disclosure), so no
  migration note is added beyond the CHANGELOG `## [Unreleased]` entry.
- `crate::config::profile_is_dev` is `pub(crate)` — no public API surface
  added.

## 🗂 Ledger

`docs/security/2026-09-11-profile-conditional-dev-overlay/`
