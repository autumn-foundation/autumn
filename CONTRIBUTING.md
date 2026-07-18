# Contributing to Autumn

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
`panic!`, out-of-bounds index, or an unfinished `todo!`/`unimplemented!`.

### What counts as "request path"

Per AC2, the gate covers modules that run **per request** or in
**framework-owned background loops** — extractors, form/body decoding, session
and idempotency stores, the scheduler and job queues, channels, and the
per-request middleware stack. These are the files listed in the
`REQUEST_PATH_MODULES` array in `scripts/check-panic-gate.sh`.

Explicitly **exempt** surfaces (a panic there cannot take down a live request):

- `#[cfg(test)]` code, benches, and examples;
- build scripts;
- the `autumn-cli` crate (a short-lived operator tool);
- application-author code (your route handlers are yours to write).

### What the gate checks

Each gated module carries a header that opts its **production** target into a
set of panic-class clippy denials:

```rust
// autumn-panic-gate: request-path module — production code path must be panic-free.
#![cfg_attr(not(test), deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
))]
```

The `cfg_attr(not(test), …)` scope means the denials apply to the library build
but auto-exempt the module's own `#[cfg(test)] mod tests`. Enforcement happens
in the CI `lint` job (same workflow as `fmt`/`clippy`, no new services):

- `cargo clippy --workspace --all-targets -- -D warnings` fails on any un-justified
  panic-class site in a gated module; and
- `scripts/check-panic-gate.sh` verifies every module in the canonical manifest
  still exists and still carries the gate header, so the gate cannot be silently
  removed.

Run the manifest check locally with `./scripts/check-panic-gate.sh`.

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

### Deferred: arithmetic

Unchecked time/duration/integer arithmetic (`clippy::arithmetic_side_effects`)
is a deferred follow-up and is **not** in the automated lint set yet. It is
currently guarded by the saturating-arithmetic idiom (e.g. `saturating_deadline`
in `idempotency.rs`) plus review rather than by the gate.

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
| `body` | request body decoding |

Each target has a committed seed corpus at `fuzz/corpus/<target>/`.

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
[issue-1611]: https://github.com/madmax983/autumn/issues/1611

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
