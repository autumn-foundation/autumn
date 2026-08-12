# Contributing to Autumn

## Before you push

Run the pre-push gate, which compiles the **same targets CI compiles** so a
cross-package break is caught locally instead of on the PR:

```sh
./scripts/pre-push-check.sh
```

It mirrors CI's always-on `lint` + `test` jobs (`.github/workflows/ci.yml`) —
`./scripts/check-panic-gate.sh` (the [#1611][issue-1611] request-path panic
gate; first because it needs no toolchain and finishes in about a second),
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
| `console_bare_playground_target_compiles_untouched` | `integration/console.rs` | `autumn console` first-run scaffold → `cargo check --bin playground` |
| `console_playground_target_compiles_with_a_repository_round_trip` | `integration/console.rs` | playground + `repo.find_all()` → `cargo check --bin playground` |
| `console_run_exits_non_zero_when_the_database_is_unreachable` | `integration/console.rs` | `autumn console` propagates config/connection failures non-zero |
| `console_run_surfaces_a_compile_error_in_the_playground` | `integration/console.rs` | a broken playground edit surfaces cargo diagnostics, non-zero |

The four `console.rs` entries compile into the consolidated `cli_tests` binary,
whose only other CI `--ignored` invocation filters on `offsite`. They are
therefore named **explicitly** in `.github/workflows/generator-conformance.yml`;
a new `#[ignore]`d console test that is not added there will never run in CI.

### Why `#[ignore]`?

These tests carry `#[ignore]` annotations so that `cargo test --workspace`
(which runs in seconds) does not block on multi-minute compile cycles in
everyday development. **The `#[ignore]` label means "CI-gated, not
abandoned."**

The `.github/workflows/generator-conformance.yml` workflow runs all four
tests explicitly via `-- --ignored --exact`. It fires on every PR or push
that touches:

- `autumn-cli/src/generate/**` (generator logic)
- `autumn-cli/src/templates/**` (scaffold/model/auth templates)
- `autumn-cli/src/new.rs` (project scaffolding)
- `autumn/src/lib.rs` or `autumn/src/prelude.rs` (public API surface)
- `autumn-macros/**` (proc-macro API surface)

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
```

The last test requires Docker (for the Postgres testcontainer) and the
`diesel` CLI on `PATH`.

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
those modules call (`time_math`), and the per-request middleware stack. These are
the 30 files listed in the `REQUEST_PATH_MODULES` array in
`scripts/check-panic-gate.sh`, each entry carrying the Cargo feature that gates
its `mod` declaration.

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

### What the gate checks

Each gated module carries a header that opts its **production** target into the
panic-class clippy denials. Copy it verbatim — this is the rustfmt-normalized
form, and `scripts/check-panic-gate.sh` requires the **complete** set of nine
lints, not a subset:

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

That last script is the gate on the gate, and it checks more than "the file still
exists":

- every manifest module exists, carries the `autumn-panic-gate:` marker, and the
  marker is **immediately followed** by its `#![cfg_attr(…)]` header — a marker
  floating free in a doc comment or in test code proves nothing, and an unrelated
  `cfg_attr` earlier in the file cannot stand in for the gate;
- the header is terminated and lists all nine lints;
- **anti-spoof**: no inner attribute (`#![…]`) anywhere in the module may combine
  `allow(` with a gated lint — a module-wide inner allow would silently defeat the
  deny while leaving it in place to read;
- every per-site `#[allow(<gated lint>…)]` carries a `reason = "…"`;
- **reverse manifest**: every `*.rs` file under `autumn/src` and `autumn-search/src`
  that carries the marker is listed in `REQUEST_PATH_MODULES` (a module gated
  in-file but missing from the manifest is unchecked — this is how `nested_form.rs`
  drifted out);
- the manifest never shrinks below `MODULE_COUNT_FLOOR`;
- **feature reachability**: a module gated behind a non-default Cargo feature must
  have that feature enabled by one of `ci.yml`'s `cargo clippy` invocations,
  otherwise its deny block is never compiled and the gate is decorative there.

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
still *fails* on each spoof it claims to catch) before checking the real tree, and
both legs together take about a second. `./scripts/pre-push-check.sh` runs it as
its first step, alongside the gated-features clippy run.

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

## Fuzzing

Autumn coverage-guides a set of [cargo-fuzz][cargo-fuzz] (libFuzzer) harnesses
over the untrusted request-parsing surface — the code paths that turn raw
bytes off the wire into typed values. The harnesses live in the `fuzz/` crate
at the workspace root and drive framework code through `#[cfg(fuzzing)]` seams
so the fuzzers exercise the real parsers, not stubs.

### Targets

There are five targets, one per parsing surface:

| Target | Surface under test |
|--------|--------------------|
| `idempotency` | idempotency-key header parsing + replay bookkeeping |
| `routing` | path/router matching and extraction |
| `headers` | request header parsing |
| `session` | session cookie decode/verify |
| `body` | request body decoding **and the inbound-mail parsers** |

Each target has a committed seed corpus at `fuzz/corpus/<target>/`.

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

The `supply-chain` CI job (`.github/workflows/ci.yml`) runs `cargo deny check
advisories licenses sources` **twice** against a pinned cargo-deny (0.20.2):
once on the checked-in `deny.toml` (the default + Postgres + additive CI feature
graph) and once on `deny-sqlite.toml` (the mutually-exclusive sqlite backend
graph). The two configs share the same advisories/licenses/sources policy — keep
them in sync — and differ only in their `[graph]` features. All three checks —
advisories (RustSec), licenses (allow-list, including dev- and build-dependency
licenses), and sources (crate registries) — are **blocking** in both passes, so
a PR that introduces a new advisory, an un-allowed license, or an unknown source
registry will fail CI. The step-by-step for triaging a failing advisory (prefer
a minimal fix; document an ignore with a reason and a review-by date only when
no fix exists) lives in the header comment of `deny.toml`.

**Scope.** The gate covers the shipped root workspace — the default plus
additive Postgres feature graph (`deny.toml`) and the mutually-exclusive sqlite
backend graph (`deny-sqlite.toml`), including dev- and build-dependency
licenses. The repository's separate *excluded* sub-workspaces — `fuzz/` and
`examples/island-flock`, which each declare their own `[workspace]` and are
excluded from the root `Cargo.toml` — are non-shipped harnesses/examples and are
not gated here. Adding a per-sub-workspace cargo-deny pass (each needs its own
config, and `fuzz/Cargo.lock` is currently out of sync with its manifest) is a
possible follow-up.
