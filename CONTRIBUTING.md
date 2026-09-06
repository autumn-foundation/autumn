# Contributing to Autumn

## Before you push

Run the pre-push gate, which compiles the **same targets CI compiles** so a
cross-package break is caught locally instead of on the PR:

```sh
./scripts/pre-push-check.sh
```

It mirrors CI's always-on `lint` + `test` jobs (`.github/workflows/ci.yml`) —
`./scripts/check-panic-gate.sh` (the [#1611][issue-1611] request-path panic
gate; first because it needs no toolchain and finishes in a couple of seconds),
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, a `--lib` clippy run over the gated request-path features, a
**compile-only** `cargo test --workspace --no-run`, and a
`cargo test --workspace --doc` doctest leg. The compile-only step is the
important one: CI's blocking gate is `cargo test --workspace`, which links
**every** workspace test target including the autumn-web consolidated
`integration_tests` binary. A narrower loop like `cargo test -p autumn-cli`
never compiles that binary, so a cross-package compile break (e.g. the #1614
sqlite+mail `E0308`) passes locally and only shows up in CI, where it looks like
a flake.

`--no-run` compiles those targets without executing them, deliberately skipping
the trybuild suite (`autumn/tests/integration/compile_fail.rs`) whose cases each
spawn a nested `cargo build` at test-**run** time — expanding scratch by ~17GB
and risking `ENOSPC`. So the gate stays disk-cheap.

The catch: `--no-run` does **not** build doctests — they only compile during the
`--doc` phase — so a doctest that stops compiling (e.g. #2107, where an `app.rs`
`no_run` example doctest broke after a struct gained a field) passes `--no-run`
locally yet fails CI's `cargo test --workspace`, which runs the doc target.
Cargo has no stable compile-only doctest mode (`cargo test --doc --no-run`
errors on the current toolchain), so the gate simply **runs** the doctests with
`cargo test --workspace --doc`. That stays cheap: doctests are overwhelmingly
`no_run`/`ignore` (which still compile — exactly the #2107 signal), so it needs
no Postgres/MediaMTX and never hangs, and because `--doc` selects only the doc
target it never triggers trybuild.

The gate does **not** cover the Docker/testcontainer, `sqlite`-backend, or
Chromium `system-tests` lanes (those run in dedicated CI jobs); `cargo test -p
<pkg>` remains fine for iterating on a single crate — just run
`./scripts/pre-push-check.sh` before you push.

In CI that blocking gate is **sharded across runners**, so a red PR points at
one of several jobs rather than a single `Test (<os>)`: `test` runs the
workspace suite, `trybuild` runs `compile_fail::` in four shards,
`test-features` runs one job per non-default feature set, and `test-docker`
runs the Linux testcontainer sweep. `Test suite` (`test-gate`) is the
aggregate check that must be green. Nothing about how you *write* a test
changes — see CLAUDE.md "CI test sharding" for the two cases that do matter
(renaming a `compile_fail.rs` test function, and what branch protection should
require).

## Generator conformance gate

Autumn's headline DX promise is that `autumn new` and `autumn generate` emit
code that **compiles, boots, and serves**. The tests that prove this live in
`autumn-cli/tests/`:

| Test | File | What it checks |
|------|------|----------------|
| `generated_project_compiles_runs_and_serves` | `e2e.rs` | `autumn new` → `cargo build` + HTTP responses |
| `generated_scaffold_cargo_checks` | `generate.rs` | `generate scaffold` → `cargo check --tests` |
| `generated_scaffold_config_cargo_checks` | `generate.rs` | config-driven scaffold → `cargo check --tests` |
| `generated_scaffold_serves_posts_index_and_json_api` | `generate.rs` | scaffold + Postgres migrations + live HTTP |
| `generated_constrained_scaffold_enforces_validation_end_to_end` | `generate.rs` | scaffold DSL `{…}` constraints + Postgres + live HTTP: rendered HTML5 attributes, 422 + inline errors, nothing stored (#1388) |
| `console_bare_playground_target_compiles_untouched` | `integration/console.rs` | `autumn console` first-run scaffold → `cargo check --bin playground` |
| `console_playground_target_compiles_with_a_repository_round_trip` | `integration/console.rs` | playground + `repo.find_all()` → `cargo check --bin playground` |
| `console_run_exits_non_zero_when_the_database_is_unreachable` | `integration/console.rs` | `autumn console` propagates config/connection failures non-zero |
| `console_run_surfaces_a_compile_error_in_the_playground` | `integration/console.rs` | a broken playground edit surfaces cargo diagnostics, non-zero |
| `encrypted_scaffold_cargo_checks` | `integration/scaffold_encrypted.rs` | `{encrypted}` scaffold → `cargo check --tests` |
| `encrypted_api_scaffold_cargo_checks` | `integration/scaffold_encrypted.rs` | `{encrypted}` + `--api` → `cargo check --tests` |
| `encrypted_live_scaffold_cargo_checks` | `integration/scaffold_encrypted.rs` | `{encrypted}` + `--live` → `cargo check --tests` |
| `encrypted_nested_scaffold_cargo_checks` | `integration/scaffold_encrypted.rs` | `{encrypted}` + `--belongs-to` → `cargo check --tests` |
| `encrypted_admin_scaffold_cargo_checks` | `integration/scaffold_encrypted.rs` | `{encrypted}` + `generate admin` (wired in) → `cargo check --tests` |
| `constrained_scaffold_cargo_checks` | `integration/scaffold_validation.rs` | `{min,max}`/`{email}`/`{url}`/nullable-bound scaffold → `cargo check --tests` (#1388) |
| `plugin_add_first_party_scaffolds_cargo_check` | `generate.rs` | `autumn plugin add` for every first-party plugin into its own fresh scaffold → `cargo check --all-targets` (#1606) |
| `api_scaffold_cargo_checks` | `integration/api_scaffold.rs` | `--api` scaffold → `cargo check --tests` |
| `scaffolded_app_passes_routes_audit_gate` | `integration/cloud_native_scaffold.rs` | fresh `autumn new` app passes `autumn routes audit` unmodified (#2154) |
| `scaffolded_api_app_passes_routes_audit_gate` | `integration/cloud_native_scaffold.rs` | fresh `autumn new --api` app passes `autumn routes audit` unmodified (#2154) |
| `unscoped_position_generated_project_cargo_checks` | `integration/generate_position_scaffold.rs` | `{position}` scaffold → `cargo check --tests` |
| `scoped_position_generated_project_cargo_checks` | `integration/generate_position_scaffold.rs` | scoped `{position}` scaffold → `cargo check --tests` |
| `soft_delete_position_generated_project_cargo_checks` | `integration/generate_position_scaffold.rs` | `{position}` + soft-delete scaffold → `cargo check --tests` |
| `belongs_to_scaffold_cargo_checks` | `integration/scaffold_belongs_to.rs` | `--belongs-to` scaffold → `cargo check --tests` |
| `bulk_delete_generated_project_cargo_checks` | `integration/scaffold_bulk_delete.rs` | bulk-delete scaffold → `cargo check --tests` |
| `richtext_scaffold_cargo_checks` | `integration/scaffold_rich_text.rs` | `{rich_text}` scaffold → `cargo check --tests` |
| `searchable_scaffold_cargo_checks` | `integration/scaffold_search.rs` | `--searchable` scaffold → `cargo check --tests` |
| `trash_generated_project_cargo_checks` | `integration/scaffold_trash.rs` | soft-delete trash scaffold → `cargo check --tests` |
| `linked_seed_binary_cargo_checks` | `integration/seed_model_linking.rs` | scaffolded model linked into `src/bin/seed.rs` → `cargo check --tests` (#1718) |
| `serve_daemon_start_status_stop_over_unix_socket` | `integration/serve.rs` | fresh scaffold's `autumn serve --daemon` lifecycle over a Unix socket |
| `generated_form_for_scaffold_cargo_checks` | `integration/scaffold_form_for.rs` | `form_for` view scaffold → `cargo check --tests` |
| `generated_scaffold_with_missing_reference_target_cargo_checks` | `integration/scaffold_form_for.rs` | `{references}` with a missing target falls back to a number input → `cargo check --tests` |

The `console.rs`, `scaffold_encrypted.rs`, `scaffold_validation.rs`, and the 15
rows above compile into the consolidated `cli_tests` binary. `ci.yml`'s Docker
sweep explicitly `--skip`s each of their tests **by exact name** — never by
module prefix, which would also silently swallow any Docker test later added
to the same file — because they scaffold and cargo-check/build/run a fresh
project instead of touching Docker. They are therefore named **explicitly**
in `.github/workflows/generator-conformance.yml`; a new `#[ignore]`d,
non-Docker test in that binary needs BOTH a `--skip <exact name>` line in
`ci.yml`'s sweep AND its own step here, or it either runs in the wrong
(Docker) step or never runs at all (issue #1945).

### Why `#[ignore]`?

These tests carry `#[ignore]` annotations so that `cargo test --workspace`
(which runs in seconds) does not block on multi-minute compile cycles in
everyday development. **The `#[ignore]` label means "CI-gated, not
abandoned."**

The `.github/workflows/generator-conformance.yml` workflow runs each of these
tests explicitly via `-- --ignored --exact`. It fires on every PR or push
that touches:

- `autumn-cli/src/generate/**` (generator logic)
- `autumn-cli/src/plugin/**` (`autumn plugin add` catalog, mounts, install planning)
- `autumn-cli/src/templates/**` (scaffold/model/auth templates)
- `autumn-cli/src/new.rs` (project scaffolding)
- `autumn/src/lib.rs` or `autumn/src/prelude.rs` (public API surface)
- `autumn-macros/**` (proc-macro API surface)
- `autumn-admin-plugin/**`, `autumn-cache-redis/**`, `autumn-media-plugin/**`,
  `autumn-search/**`, `autumn-storage-s3/**` (the crates whose mount snippets
  the `plugin add` gate compiles)

A weekly scheduled run also catches breakage that arrives through transitive
dependency updates rather than direct file edits.

### Running them locally

```sh
# All ignored generator tests at once
cargo test -p autumn-cli -- --ignored

# Individual gates
cargo test -p autumn-cli --test e2e    generated_project_compiles_runs_and_serves    -- --ignored --exact
cargo test -p autumn-cli --test generate generated_scaffold_cargo_checks             -- --ignored --exact
cargo test -p autumn-cli --test generate generated_scaffold_config_cargo_checks      -- --ignored --exact
cargo test -p autumn-cli --test generate generated_scaffold_serves_posts_index_and_json_api -- --ignored --exact
cargo test -p autumn-cli --test generate generated_constrained_scaffold_enforces_validation_end_to_end -- --ignored --exact
cargo test -p autumn-cli --test cli_tests integration::scaffold_validation::constrained_scaffold_cargo_checks -- --ignored --exact
```

The two `--test generate` Postgres gates require Docker (for the Postgres
testcontainer) and the `diesel` CLI on `PATH`.

### What triggers a failure?

Any change to the `autumn-web` public surface that the generated templates
depend on — a renamed macro argument, a moved prelude re-export, an
`AppBuilder` signature change — will cause the compiled output to fail
`cargo check`. The generator conformance gate catches this before it reaches
a user's first `autumn generate scaffold`.

The tests capture and print the full `cargo build` / `cargo check`
stdout+stderr on failure, so the breakage is diagnosable directly from the
CI summary.

## Request-path panic gate

Autumn enforces a [#1611][issue-1611] invariant: **request-path modules must not
panic on the production code path.** A request that reaches a runtime module
should never be able to bring the process down through an `unwrap`, `expect`,
`panic!`, out-of-bounds index, a `&s[a..b]` slice on a byte offset that is not a
char boundary, an integer or time overflow, or an unfinished
`todo!`/`unimplemented!`.

### What counts as "request path"

Per AC2, the gate covers modules that run **per request** or in
**framework-owned background loops** — extractors, form/body decoding (including
`nested_form`), session and idempotency stores, the scheduler and job queues,
channels, inbound-mail webhook parsing (`inbound_mail`, which turns unauthenticated
RFC 5322 / MIME bytes into typed values), the shared saturating-arithmetic helpers
those modules call (`time_math`), the per-request middleware stack, and the
failure-capsule capture path (`autumn/src/capsule/capture.rs`, `wire.rs`,
`record_db.rs`: they tee a live request's body and its database connection, so
a panic there would take down the very request they exist to record), and the
sandboxed-plugin runtime (`autumn/src/plugin_sandbox/host.rs`, `wire.rs`,
`plugin.rs`: they run an artifact the operator explicitly did not audit, and the
lane's whole promise is that nothing a hostile guest does can abort the host
process). These are the files listed in the `REQUEST_PATH_MODULES` array in
`scripts/check-panic-gate.sh`, each entry carrying the Cargo feature that gates
its `mod` declaration.

**Honest scoping — the manifest is the *enforced* subset, not the whole request
path.** The 37 modules are the files the gate enforces today, not a claim that
they are the *only* per-request code. Other unambiguously per-request or
framework-owned modules are **not yet gated** and still contain production-path
panics — known examples include `router.rs`, `etag.rs`, `security/rate_limit.rs`,
`security/headers.rs`, `sse.rs`, and the `csrf` / `negotiate` / `range` /
`validation` / `auth` seams, plus the sibling plugin crates
(`autumn-media-plugin`, `autumn-storage-s3`, `autumn-cache-redis`,
`autumn-admin-plugin`). The manifest is deliberately **incremental**: it grows
monotonically and never shrinks (enforced by `MODULE_COUNT_FLOOR`), and the
invariant is being burned down toward full request-path coverage one audited
batch at a time. This staged rollout is the scoping [#1611][issue-1611] accepted
(its own "one-time audit / Tier-M" framing calls for an incremental manifest
rather than a big-bang flip); do not read a module's absence from the list as a
promise that it is panic-free. Adding one of the ungated modules above — after
auditing and fixing its panics — is exactly the expected follow-up.

Explicitly **exempt** surfaces (a panic there cannot take down a live request):

- `#[cfg(test)]` code, benches, and examples;
- build scripts;
- the `autumn-cli` crate (a short-lived operator tool);
- application-author code (your route handlers are yours to write);
- **proc-macro internals** in `autumn-macros`: they run at compile time, so a
  panic there fails a build rather than a request. Code a macro *generates* and
  expands into a user crate is likewise outside the gate — the deny header is
  per-module and does not follow an expansion across crates. So macro-emitted
  request-path logic must stay a thin shim that **delegates to a gated runtime
  function** in `autumn-web`; do not inline fallible parsing or arithmetic into
  the tokens a macro emits, where nothing will ever lint it.
- **`macro_rules!` expansions** are a blind spot the gate *cannot* close:
  clippy suppresses lints inside a macro expansion, so a `panic!`/`.unwrap()`
  written in a `macro_rules!` body — even one defined in an ungated module and
  *invoked from* a gated one — produces no diagnostic on the gated module. A
  request-path module therefore **must not invoke a local panic-expanding macro**
  on its production path; a macro that can expand to a panic must itself expand
  to a call into a gated runtime function that carries the invariant. There is no
  script check for this (the expansion is invisible at the source level), so it
  is a review responsibility — treat a `macro_rules!` that hides an `unwrap`,
  `expect`, `panic!`, or `unreachable!` used on the request path as a gate escape.

### What the gate checks

Each gated module carries a header that opts its **production** target into the
panic-class clippy denials. Copy the `#![cfg_attr(…)]` block verbatim — this is
the rustfmt-normalized form, and `scripts/check-panic-gate.sh` requires the
**complete** set of nine lints, not a subset. (The `// autumn-panic-gate:` marker
line's prose may read "request-path **crate**" for a crate-level header — e.g.
`autumn-search/src/lib.rs` — versus "request-path **module**" for a module-level
one; the script only keys on the `autumn-panic-gate:` token, so either wording is
fine.)

```rust
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
```

The `cfg_attr(not(test), …)` scope means the denials apply to the library build
but auto-exempt the module's own `#[cfg(test)] mod tests`. Enforcement happens
in the CI `lint` job (same workflow as `fmt`/`clippy`, no new services):

- `cargo clippy --workspace --all-targets -- -D warnings` fails on any un-justified
  panic-class site in a gated module built with default features;
- a second clippy run with the gated request-path features
  (`ws,mail,offline-sync,redis,markdown,inbound-mail,inbound-mailgun,inbound-ses,storage`)
  does the same for the feature-gated modules, whose deny blocks the
  default-feature run never even compiles; and
- `scripts/check-panic-gate.sh` verifies the manifest itself.

That last script is the gate on the gate. Because the header is only worth as
much as clippy's ability to *see* it, the script is built to close the ways a
production panic could ship while both it and `cargo clippy -- -D warnings` stay
green:

- every manifest module exists, carries the `autumn-panic-gate:` marker, and the
  marker is **immediately followed** by its `#![cfg_attr(…)]` header — a marker
  floating free in a doc comment or in test code proves nothing, and an unrelated
  `cfg_attr` earlier in the file cannot stand in for the gate;
- **structural header shape**: after stripping `//` comments and whitespace, the
  header must open *exactly* `#![cfg_attr(not(test), deny(` and list all nine
  lints. A widened predicate like `all(not(test), any())` (whose deny never
  compiles), a `not(test)` that lives only in a comment, or a lint named only in
  a comment are all rejected — a plain substring check would wave them through;
- **anti-spoof, tree-wide**: no inner attribute — `#![allow(…)]`, `#![expect(…)]`,
  or the `#![cfg_attr(…, allow(…))]` form — may re-permit a gated lint *or* a
  blanket lint group (`clippy::restriction`/`all`/`pedantic`/`nursery`) that
  contains one. This scan runs over **every** `*.rs` under the scan roots, not
  just manifest entries, so an unmarked submodule of a gated module (e.g. a
  `helpers.rs` with a module-level `#![allow(clippy::unwrap_used)]`) cannot slip a
  suppression past both the manifest and the reverse-manifest check. Inner allows
  inside a `#[cfg(test)]` scope are exempt (that is where a module's own tests
  legitimately allow them);
- every per-site **outer** `#[allow(<gated lint>…)]` carries a **non-empty**
  `reason = "…"` (`reason = ""` does not count);
- **reverse manifest**: every marker-carrying `*.rs` under the scan roots is listed
  in `REQUEST_PATH_MODULES` (a module gated in-file but missing from the manifest
  is unchecked — this is how `nested_form.rs` drifted out). The scan roots are
  `autumn/src`, `autumn-search/src`, and the four sibling framework crates
  (`autumn-admin-plugin`, `autumn-media-plugin`, `autumn-storage-s3`,
  `autumn-cache-redis`); `autumn-cli` and `autumn-macros` are exempt and not
  scanned;
- the manifest never shrinks below `MODULE_COUNT_FLOOR`;
- **feature reachability**: a module gated behind a non-default Cargo feature must
  have that feature enabled by an **enforcing** `ci.yml` clippy lane — one that is
  not commented out and carries both `-p autumn-web` and `-D warnings` — otherwise
  its deny block is never compiled and the gate is decorative there. A
  commented-out or non-deny lane does not count, so it cannot fake reachability.

The last one has exactly one exemption today, spelled out in the script's
`FEATURE_LINT_EXEMPT` array: `middleware/trace_context.rs` is
`#[cfg(feature = "telemetry-otlp")]`, and that feature pulls prost/tonic, whose
build scripts need `protoc` — which the `lint` runner does not install. So its
header is real but unenforced, and every run prints a `NOTE` line saying so.
Burning it down is a workflow change, not a code change (a `protoc` install step
plus the feature in the gated-features clippy step; the module already lints
clean with the feature on). Exemptions are validated, not merely tolerated: the
script rejects one whose module has left the manifest, and rejects one whose
feature has since become linted, so a temporary hole cannot quietly become
permanent. Adding a module to that array — rather than getting it linted — needs
the same scrutiny as deleting a lint.

Run it locally with `./scripts/check-panic-gate.sh` — the default invocation runs
its own `--self-test` (synthetic fixtures in a temp dir, asserting the checker
still *fails* on each spoof it claims to catch, including the widened-predicate,
inner-`expect`, group-allow, and unmarked-submodule bypasses) before checking the
real tree, and both legs together take a couple of seconds. `./scripts/pre-push-check.sh`
runs it as its first step, alongside the gated-features clippy run.

### Falsifying the gate (AC1)

A gate nobody has ever seen fail is a gate nobody should trust. To watch it go
red, add an unannotated panic-class site to any gated module — for example, in
`autumn/src/idempotency.rs`:

```rust
let n: u64 = "1".parse().unwrap();   // clippy::unwrap_used
let m = n + 1;                       // clippy::arithmetic_side_effects
let s = &"hello"[1..];               // clippy::string_slice
```

then run the same command CI runs for that module's feature set:

```sh
cargo clippy -p autumn-web \
  --features "ws,mail,offline-sync,redis,markdown,inbound-mail,inbound-mailgun,inbound-ses,storage" \
  --all-targets -- -D warnings
```

Each line is reported as `error: …` and the build fails. Revert, re-run, green.
Note the site must be **outside** `#[cfg(test)]` — the header exempts the module's
own tests on purpose, so a `.unwrap()` added inside `mod tests` will *not* go red.
The complementary experiment for the manifest half — delete a lint from a header,
or add a marker to an unlisted file — is exactly what
`./scripts/check-panic-gate.sh --self-test` automates on fixtures.

### Justifying an exception

When a site is provably infallible or a misconfiguration you want surfaced
eagerly, annotate it at the **narrowest** scope with a `reason`:

```rust
#[allow(clippy::expect_used, reason = "infallible: HMAC accepts any key length")]
```

For lock poisoning, do **not** annotate — **recover** instead, so one panicking
lock holder can't poison shared state for every later request:

```rust
let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
```

This `unwrap_or_else(PoisonError::into_inner)` idiom is the established
lock-poisoning recovery contract (AC4); see `circuit_breaker.rs`.

### Arithmetic and slicing

Unchecked arithmetic and byte slicing are no longer deferred: the gate denies
`clippy::arithmetic_side_effects` and `clippy::string_slice` in every gated
module, so `a + b`, `a - b`, `a * b`, `now + ttl` and `&s[a..b]` are all errors
on the production path unless you use one of the sanctioned idioms below. The
lints exist because each of those operators panics on inputs that are routinely
*attacker- or config-supplied*: an overflowing counter, a TTL from a config file,
a byte offset from a parser mid-UTF-8.

Which idiom is correct depends on what the value *means*:

- **Counters, capacities, sizes → saturate.** `saturating_add` / `saturating_sub`
  (or `saturating_mul`). A request counter that pins at `u64::MAX` is a
  metrics blemish; a panicking one is an outage.
- **Deadlines and durations → `checked_*` with an explicit fallback,** or the
  crate-private helpers in `autumn/src/time_math.rs` that wrap that pattern:
  `time_math::saturating_deadline` (`Instant + Duration`),
  `time_math::saturating_dt_add` (`DateTime<Utc> + TimeDelta`) and
  `time_math::saturating_time_delta_secs` (`TimeDelta::seconds`, which panics
  above `i64::MAX / 1_000`). For a `TimeDelta` you build yourself, use
  `TimeDelta::try_seconds(..)` and handle the `None`. Clamping to a far-future
  horizon is the safe direction for an expiry: a pathological TTL yields an
  effectively non-expiring entry rather than a dead process.
- **Parser offsets and lengths → `checked_*` and early-reject.** Clamping is for
  time and capacity, *not* for parsers: a malformed MIME boundary or a length
  field that overflows must make the parse **fail**, not silently truncate to a
  clamped value and hand the caller a plausible-looking wrong answer. Prefer
  `get(a..b)` / `split_at_checked` / `char_indices` over `&s[a..b]` — that also
  answers `clippy::string_slice`, which fires because a byte range that lands
  mid-UTF-8 panics.

A per-site `#[allow(clippy::arithmetic_side_effects, reason = "…")]` is legal
when the invariant is genuinely local and you state it (`reason = "i < len,
checked above"`). It is never legal at module level: an inner
`#![allow(clippy::arithmetic_side_effects)]` re-permits the lint for the whole
file, and `scripts/check-panic-gate.sh` rejects it as a spoof of the gate.

### Toolchain caveat

`arithmetic_side_effects`, `string_slice` and `indexing_slicing` are clippy
**restriction** lints: their exact firing set can shift between clippy releases,
so a routine `dtolnay/rust-toolchain@stable` bump can turn an unrelated PR red in
a gated module nobody touched. When that happens, do **not** delete a lint from
the headers to get green. Pin the toolchain action to the previous version
(`dtolnay/rust-toolchain@<ver>`), land the PR, and file a burn-down issue for the
new findings. Losing a lint is permanent; a pin is a week.

## Determinism seam gate

Autumn's simulation testing (`#[sim_test]`, [#1797][issue-1797]) rests on one
promise: **a run is a pure function of its seed**. That holds only while the
framework reads time and mints identifiers through its *injected* seams. A single
`Instant::now()` on a code path a simulation touches makes the run depend on the
machine it ran on, and the failure mode is silent — the test still passes, it
just stops proving anything.

The gate makes that a compile error instead of a code-review hope.

### The seams, and what to reach for

| Instead of | Use | Reachable from |
|---|---|---|
| `chrono::Utc::now()` | `state.clock().now()` | anything holding an `AppState`; the `Clock` extractor in a handler |
| `std::time::Instant::now()` (measuring elapsed) | `clock.monotonic()` for the start reading and `state.monotonic()` for the closing one, then `MonotonicInstant::saturating_duration_since`. The `Clock` extractor **snapshots** at request start, so calling `Clock::monotonic` twice returns the same value | same |
| `std::time::Instant::now()` (a deadline whose counterparty is `tokio::time::sleep`) | `tokio::time::Instant::now()` | anywhere — tokio's paused runtime already virtualizes it |
| `std::time::SystemTime::now()` | `time::clock_unix_secs(clock)` / `time::clock_unix_duration(clock)` | same |
| `uuid::Uuid::new_v4()` | `state.entropy().uuid_v4()`; the `Rng` extractor in a handler | same |

Two notes on the monotonic seam, because they are the parts that surprise people:

- **`tokio::time::pause()` does not virtualize `std::time::Instant`.** Only
  `tokio::time::Instant` moves with the paused timer wheel. That is precisely why
  a raw `Instant::now()` inside a `#[sim_test]` reads the real machine clock, and
  why `MonotonicInstant` exists.
- **`SystemClock` still reads a real `std::time::Instant`.** `MonotonicInstant`
  is an offset from its source's own origin, so a *virtual* clock can produce one
  at any point — but in production the origin is a process-global `Instant`, so an
  NTP step can never make an elapsed duration negative. The seam does not trade
  monotonicity for testability.

When no clock is reachable at all — a constructor that runs before one is
installed, a free function with no state argument — `time::monotonic_now()` is the
sanctioned fallback. It is real time and never follows a simulation, so prefer
threading a real handle whenever that is possible.

### What the gate covers

`clippy.toml`'s `disallowed-methods` array bans the four calls workspace-wide,
and each gated module re-denies `clippy::disallowed_methods` for its production
code path:

```rust
// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
```

**The polarity is inverted from the panic gate, deliberately.** The panic-class
lints are `restriction`-group and allow-by-default, so a module opts in simply by
denying them. `clippy::disallowed_methods` is *warn*-by-default, so populating the
config would arm it in all 24 workspace members at once — hundreds of
pre-existing sites in crates that are not part of the determinism story (the
`examples/*`, `autumn-cli`'s code generators). The workspace therefore
**grandfathers** the lint — `[workspace.lints.clippy] disallowed_methods =
"allow"` in the root `Cargo.toml`, plus a package-level `[lints.clippy]` table in
each crate that does not opt into the workspace table — and a module opts *in* by
carrying the header above.

**Honest scoping — the manifest is the *enforced* subset.** The modules listed in
`GATED_MODULES` in `scripts/check-determinism-gate.sh` are the ones enforced
today, not a claim that the rest of the crate is on-seam. `autumn/src` still
contains roughly 150 ungated production call sites. The highest-value next batch
is the code whose elapsed-time reads gate *control flow* or are observable in a
response: `idempotency.rs` (replay-window TTLs — note its `IdempotencyEntry`
exposes `expires_at: Instant` as a **public** field, so migrating it is a
breaking change and needs a migration-guide entry), `circuit_breaker.rs`
(open/half-open transitions), and the per-request `middleware/access_log.rs`,
`middleware/metrics.rs`, and `middleware/server_timing.rs` timers. Then
`webhook_outbound.rs`, `notifications.rs`, `storage/local.rs`, and the rest. The
manifest grows monotonically and never shrinks (`MODULE_COUNT_FLOOR`); do not
read a module's absence from it as a promise that it is on-seam.

Known-open gaps, named rather than hidden:

- **`db::run_instrumented`** is a published `pub` function taking no state, so
  threading a clock in would break the public API. Its `Instant::now()` carries a
  per-site `#[allow]` with that reason; the instant never escapes (only
  `elapsed_ms` does) and the framework has no caller of its own.
- **`#[repository]`-generated writes.** The macro emits
  `chrono::Utc::now()` for soft-delete and timestamp columns, and the generated
  repository holds only a pool — no `AppState`, so no clock is reachable. The
  expansion carries its own `#[allow(clippy::disallowed_methods, reason = "…")]`
  so it never trips the lint in the *calling* crate, whose author did not write
  it. That is a suppression, not a fix: a soft-deleted row's `deleted_at` is
  still non-deterministic under simulation.
- **`app.rs`'s TLS `now_unix`** reads real wall time on purpose. Certificate
  validity is a fact about the real world; a simulation clock pinned to the sim
  epoch must not be able to declare a live certificate expired.

**Grandfathered crates.** "In-scope crates" is `autumn` (published as
`autumn-web`) and nothing else today. Every other workspace member is
grandfathered by a package-level `[lints.clippy] disallowed_methods = "allow"`
(or by the workspace table, for members that opt into it), and several of them do
carry production off-seam sites: `autumn-media-plugin` (room/session ids and
timestamps), `autumn-admin-plugin`, `autumn-cache-redis`, and `autumn-cli`. That
is a scoping decision, not an audit result — the sim drives an `autumn` app, so
`autumn` is where determinism is load-bearing first. Gating a plugin crate means
migrating its sites, adding the header, and adding it to `GATED_MODULES`.

Exempt surfaces mirror the panic gate: `#[cfg(test)]` code (the
`cfg_attr(not(test), …)` scope handles it automatically), benches, examples,
`autumn-cli`, and application-author code.

### What the script checks

`scripts/check-determinism-gate.sh` is the gate on the gate, and it deliberately
**never greps for the banned calls** — clippy does the detection, because clippy
resolves `use chrono::Utc as U; U::now()` and proc-macro expansions that a grep
cannot, and, decisively, clippy does *not* see string literals. A grep gate would
flag the ~30 templated `Utc::now()` occurrences inside `autumn-cli`'s code
generators and the `include_dir!`-embedded starter apps, which are generated-app
*text*, not compiled code; "fixing" those would corrupt the apps the CLI emits.

The script guards the things clippy cannot report on itself:

- every manifest module exists, carries the `autumn-determinism-gate:` marker,
  and the marker is **immediately followed** by the header;
- **structural header shape**: after stripping comments and whitespace it must
  open exactly `#![cfg_attr(not(test), deny(` and name every required lint, so a
  widened predicate like `all(not(test), any())` — whose deny never compiles — or
  a `not(test)` that lives only in a comment is rejected;
- **anti-spoof, tree-wide**: no inner `#![allow(…)]` / `#![expect(…)]` /
  `#![cfg_attr(…, allow(…))]` anywhere under `autumn/src` may re-permit the lint
  or a blanket group containing it, outside a `#[cfg(test)]` scope;
- **per-site allow hygiene**: an `#[allow(clippy::disallowed_methods)]` in a gated
  module must carry a non-empty `reason = "…"` (an empty string fails);
- **reverse manifest**: a marker-carrying file that is not listed is an error;
- **config completeness**: `clippy.toml` still bans all four paths, each with a
  non-empty reason, and still pins `msrv`. Emptying the array would otherwise
  disarm every header at once while the whole tree stayed green;
- **no crate-local `clippy.toml`**: clippy reads the nearest ancestor config and
  stops, so a crate-local file *shadows* the root one entirely — silently
  removing both the ban and the MSRV pin;
- **workspace grandfather present**: without it the array arms every member, CI
  fails on hundreds of out-of-scope sites, and the pressure is to "fix" that by
  emptying the array;
- **feature reachability**: a gated module behind a non-default feature must have
  that feature enabled by an enforcing `ci.yml` clippy lane, or its deny block is
  never compiled.

Run it locally — it needs no toolchain and finishes in about a second:

```bash
./scripts/check-determinism-gate.sh              # self-test, then the real check
./scripts/check-determinism-gate.sh --self-test  # synthetic fixtures only
./scripts/check-determinism-gate.sh --check-only # real tree only
```

Like the panic gate, it self-tests first, so a refactor that quietly defangs the
checker fails immediately rather than years later on a real regression.

### Adding a module to the gate

1. Migrate its production call sites onto the seams (table above).
2. Add the header block verbatim, right after the module's `//!` docs.
3. Add `<path>:<feature>` to `GATED_MODULES` and bump `MODULE_COUNT_FLOOR`.
4. Run `./scripts/check-determinism-gate.sh` and
   `cargo clippy -p autumn-web --all-targets -- -D warnings`.

[issue-1797]: https://github.com/autumn-foundation/autumn/issues/1797

## Fuzzing

Autumn coverage-guides a set of [cargo-fuzz][cargo-fuzz] (libFuzzer) harnesses
over the untrusted request-parsing surface — the code paths that turn raw
bytes off the wire into typed values. The harnesses live in the `fuzz/` crate
at the workspace root and drive framework code through `#[cfg(fuzzing)]` seams
so the fuzzers exercise the real parsers, not stubs.

### Targets

There are six targets, one per parsing surface:

| Target | Surface under test |
|--------|--------------------|
| `idempotency` | idempotency-key header parsing + replay bookkeeping |
| `routing` | path/router matching and extraction |
| `headers` | request header parsing |
| `session` | session cookie decode/verify |
| `body` | request body decoding **and the inbound-mail parsers** |
| `dns` | DNS wire-format parsing for the ACME DNS-01 propagation probe |
| `sandbox` | the `.autumn-plugin` container, the manifest validator, and the NDJSON frames a sandboxed plugin writes |

Each target has a committed seed corpus at `fuzz/corpus/<target>/`.

`sandbox` splits its input on a NUL byte so one entry can carry a binary
container and a text frame; a single-field entry drives all three decoders. Every
byte it sees came out of an artifact the operator explicitly did not audit
(issue #1609), which is why the surface is fuzzed rather than merely
unit-tested — a length field in that container is chosen by the same person who
chose the module.

`body` multiplexes on its first input byte, so one target covers several
parsers: urlencoded form decoding plus `inbound_mail`'s SES/SNS JSON reader, the
RFC 5322 / MIME body parser (including nested `multipart/*`), the address-list
parser, and the Mailgun `multipart/form-data` webhook parser
(`__fuzz::parse_mailgun_form_data`, which drives the boundary splitter and the
quote-aware `Content-Disposition` reader). Adding a parser to `inbound_mail`
therefore means adding a seam in its "Fuzzing seams" block and a discriminant arm
in `fuzz/fuzz_targets/body.rs` — not a new target.

### Running locally

Fuzzing needs a nightly toolchain and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz          # once

# Run a single target (Ctrl-C to stop); it seeds from
# fuzz/corpus/<target>/ automatically and writes new coverage-increasing
# inputs back into that directory.
cargo +nightly fuzz run idempotency

# Bound the run the way CI does (per-PR gate = 30s, nightly long-run = 300s):
cargo +nightly fuzz run routing -- -max_total_time=30 -timeout=10

# List all targets
cargo +nightly fuzz list
```

CI runs these two ways (see `.github/workflows/`):

- **`fuzz.yml`** — per-PR crash gate. Every target runs a 30s burst seeded
  from the committed corpus on each push/PR to `trunk`/`trunk-dev`. A crash
  fails the check and uploads the minimized reproducer under
  `fuzz/artifacts/**`.
- **`fuzz-nightly.yml`** — a nightly (03:00 UTC) + on-demand 5-minute-per-target
  long-run whose corpus is persisted across runs (it only grows), so coverage
  compounds over time.

### Crash triage contract

When a fuzzer finds a crash — locally or in CI (download the `fuzz-artifacts-*`
reproducer from the failed run) — the fix is not complete until the crash is
turned into a permanent regression guard:

1. **Reproduce and minimize.** Replay the reproducer and shrink it to the
   smallest input that still triggers the crash:

   ```sh
   cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>
   cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<crash-file>
   ```

2. **Fix the parser**, not the harness.

3. **Commit the minimized input into the seed corpus.** Add the minimized
   reproducer to `fuzz/corpus/<target>/` and re-minimize the whole corpus so
   the seed set stays lean:

   ```sh
   cargo +nightly fuzz cmin <target>
   ```

   The committed seed makes the per-PR gate replay this exact input forever, so
   the bug can never silently regress.

4. **Extend the [#1611][issue-1611] request-path lint gate for the crash's
   class.** If the crash represents a class of mistake the lint could catch
   (e.g. an un-bounded allocation from a length field, an un-validated
   percent-decode), add or extend the corresponding #1611 lint so the pattern
   is rejected at the source level, not just caught at runtime. If the class is
   already covered, note the existing lint in the PR.

### New request-path modules

Any new #1611 request-path module — anything that parses untrusted bytes into
typed values on the request hot path — must ship with either:

- a **fuzz target** in `fuzz/` covering its parser (add a `fuzz_targets/<name>.rs`
  binary, a `fuzz/corpus/<name>/` seed dir, and the target name to the matrices
  in `fuzz.yml` and `fuzz-nightly.yml` and to the table above), **or**
- a **documented exemption** in the PR explaining why the module is not on the
  untrusted-input path (e.g. it only ever sees framework-internal, already-typed
  values).

Reviewers should treat a new request-path parser with neither as incomplete.

[cargo-fuzz]: https://github.com/rust-fuzz/cargo-fuzz
[issue-1611]: https://github.com/autumn-foundation/autumn/issues/1611

## Supply chain (cargo-deny)

The `supply-chain` CI job (`.github/workflows/ci.yml`) checks the dependency
tree with a pinned cargo-deny (0.20.2) against two configs: the checked-in
`deny.toml` (the default + Postgres + additive CI feature graph) and
`deny-sqlite.toml` (the mutually-exclusive sqlite backend graph). The two
configs share the same advisories/licenses/sources policy — keep them in sync —
and differ only in their `[graph]` features. Licenses (allow-list, including
dev- and build-dependency licenses) and sources (crate registries) run directly;
all of it is **blocking**, so a PR that introduces a new advisory, an un-allowed
license, or an unknown source registry fails CI. The step-by-step for triaging a
failing advisory (prefer a minimal fix; document an ignore with a reason and a
review-by date only when no fix exists) lives in the header comment of
`deny.toml`.

Advisories go through `scripts/check-advisories.sh` (issue #1600), which the
**Publish Gate** runs too, so a release cannot be tagged while an unwaived
RustSec advisory sits in the tree being published. Run it locally exactly as CI
does:

```bash
./scripts/check-advisories.sh              # workspace, sqlite graph, scaffold graph
./scripts/check-advisories.sh --self-test  # prove the gate still rejects a CVE
```

It audits a third graph beyond the two above: `autumn-web` under the `deny.toml`
that `autumn new` writes into a generated app (`autumn-cli/src/templates/deny.toml.tmpl`),
so an advisory the scaffold's shipped waiver set does not cover fails here
rather than in a user's first CI run. Its advisory-database fetch retries and
**fails closed**; the audits then run `--offline`, so a failure always names an
advisory rather than a network blip. `--self-test` audits a throwaway crate with
a deliberately injected known-vulnerable dependency and requires the gate to
reject it, then to accept it once — and only once — that id is waived.

**Scope.** The gate covers the shipped root workspace — the default plus
additive Postgres feature graph (`deny.toml`) and the mutually-exclusive sqlite
backend graph (`deny-sqlite.toml`), including dev- and build-dependency
licenses — plus, for advisories only, autumn-web's tree under the policy
`autumn new` ships (`autumn-cli/src/templates/deny.toml.tmpl`). The repository's separate *excluded* sub-workspaces — `fuzz/` and
`examples/island-flock`, which each declare their own `[workspace]` and are
excluded from the root `Cargo.toml` — are non-shipped harnesses/examples and are
not gated here. Adding a per-sub-workspace cargo-deny pass (each needs its own
config, and `fuzz/Cargo.lock` is currently out of sync with its manifest) is a
possible follow-up.
