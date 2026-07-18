# Dev-Loop Latency Budget

Autumn's product promise is that the edit–refresh loop feels **boringly fast**.
This document defines the accepted latency budget for every change class that
`autumn dev` handles, explains how to measure it, and describes the gates that
prevent regressions from shipping undetected.

> **Quick comparison:** Rails and Phoenix developers expect saves to show up in
> under a second for non-code changes and within a few seconds for compiled
> code. Autumn can't hide Rust compile times, but it publishes budgets per
> change class so contributors can see exactly where the framework stands
> and contributors can tell immediately when a change makes things worse.

---

## Budget Matrix

All timings are **end-to-end wall-clock milliseconds** from the moment a file is
saved on disk to the moment a successful, observable result is confirmed in the
browser (refreshed stylesheet, updated route response, or successful HTTP reply
after a server restart). Process-restart log lines are not sufficient — the
budget covers what the user can actually see.

| Change class | p50 ms | p95 ms | max ms | Notes |
|---|---:|---:|---:|---|
| Initial dev boot to first route | 10 000 | 20 000 | 40 000 | Cold compile; varies widely by machine |
| Rust route edit — `examples/hello` (no DB) | 3 000 | **5 000** | 10 000 | Warm incremental compile |
| Rust route edit — database-backed example | 5 000 | **10 000** | 20 000 | Warm incremental compile + Diesel |
| CSS/Tailwind edit to refreshed stylesheet | 500 | **1 000** | 2 000 | Tailwind rebuild only, no Rust compile |
| Static asset edit to browser reload | 300 | **1 000** | 2 000 | File-copy + SSE push, no compile |
| Config edit (`autumn.toml`) to restarted server | 3 000 | 8 000 | 15 000 | Process restart; no Rust recompile |
| Custom `dev.watch_dirs` edit to restarted server | 3 000 | 8 000 | 15 000 | Same as config edit |

**Bolded p95 values are the enforced gates** referenced in the success metric
and checked by `autumn dev-loop-bench --fail-on-regression`.

### Regression allowance

Any release that **exceeds the absolute budgets above**, or **regresses an
accepted baseline by more than 20%** before the absolute budgets are met, must
either fail the documented gate or carry an explicit release-note exception.

---

## Cold-Start Onboarding Budget

The budget matrix above measures the **warm incremental** loop — the experience
of a developer already working in a built project. A brand-new user has a
different, decisive first experience: `autumn new my-app` → `autumn dev` → a page
in the browser, which includes Rust's **first clean compile**. That cold-start
journey is the single biggest onboarding DX tax versus Rails/Django/Phoenix, so
it gets its own budget and gate (issue #977).

| Change class | p50 ms | p95 ms | max ms | Gate |
|---|---:|---:|---:|---|
| Cold start (`autumn new` → first 200, no-DB) | 45 000 | **60 000** | 90 000 | **Gated** |
| Cold start (`autumn new` → first 200, database-backed) | 120 000 | 180 000 | 300 000 | Informational |

**Success metric:** p95 cold start for the no-DB `hello` shape ≤ **60 s** on the
CI reference runner — matching Autumn's stated "time-from-`cargo new` to first
served route < 60 s" promise.

The **automated** weekly gate checks the **absolute budget** above (it fails when
`all_passed` is `false`), exactly mirroring the warm `dev-loop-latency.yml` model.
The first scheduled run records the baseline number in its uploaded report
artifact; the **> 20%-over-baseline** allowance — used while a runner is still
above the absolute budget — is the same documented **release-checklist policy**
as the warm loop (see [Regression allowance](#regression-allowance)), not a
separately automated check. The database-backed shape is **informational** in
this slice and never fails the gate.

### Cold-start methodology

`autumn dev-loop-bench --cold-start` measures a genuine first-run, not a warm
cache:

1. Scaffolds a **throwaway project** in a fresh temp directory (`autumn new`).
   The no-DB `hello` shape uses the daemon starter; the database-backed shape
   (`--include-db`) uses the **bundled managed-Postgres** starter, so the app
   self-provisions a real Postgres (via `postgresql_embedded`), runs migrations,
   and connects before serving — no external database service required. The clock
   starts just before scaffolding so the whole journey is captured.
2. Repoints the project's `autumn-web` dependency at the repository's local
   source via `[patch.crates-io]`, so the number reflects the code in the repo
   rather than a possibly-unpublished crates.io release — still a genuine clean
   compile.
3. Runs `cargo build` into the project's **own, empty `target/`** (any inherited
   `CARGO_TARGET_DIR` is removed), so the workspace's warm cache is never reused
   — this is the first clean compile.
4. Reserves a **free ephemeral port** for the sample (override with
   `AUTUMN_BENCH_PORT`), starts the built binary — pinning host/port via the
   highest-precedence `AUTUMN_SERVER__HOST`/`AUTUMN_SERVER__PORT` env vars and
   discarding its logs — and polls `http://127.0.0.1:<port>/` until the first
   HTTP `200`. A fresh port per sample avoids colliding with a lingering
   `TIME_WAIT` socket from the previous sample.
5. Records `duration = first_200 − scaffold_start`, repeats `--runs` times, and
   computes p50/p95/max.

The cold-start report carries the **same environment metadata** as the warm
report (`timestamp_utc`, `runner_os`, `rust_version`, `autumn_version`) and, like
it, never includes local absolute paths, usernames, or secrets.

### Running the cold-start benchmark

```bash
# Print the cold-start budget table (no build, no server):
autumn dev-loop-bench --cold-start --dry-run

# Measure the gated no-DB hello shape and write a JSON report:
AUTUMN_BENCH_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
AUTUMN_BENCH_RUST_VERSION="$(rustc --version)" \
autumn dev-loop-bench --cold-start \
  --output cold-start-report.json --fail-on-regression

# Also measure the database-backed shape (informational; needs Postgres):
autumn dev-loop-bench --cold-start --include-db
```

---

## Validated Examples

Measurements are taken against at least two example projects to cover the two
main development paths:

| Path | Example | Prerequisites |
|---|---|---|
| No-database | `examples/hello` | Rust toolchain, `cargo` |
| Database-backed | `examples/todo-app` | Rust toolchain, `cargo`, running PostgreSQL |

### Running `examples/hello` (no database)

```bash
# From the workspace root
cd examples/hello
autumn dev          # leaves dev server running
# In a second terminal, run the benchmark:
autumn dev-loop-bench --example examples/hello --runs 5
```

### Running `examples/todo-app` (database-backed)

```bash
# Start a local Postgres instance
docker compose -f examples/todo-app/docker-compose.yml up -d
# Apply migrations
cd examples/todo-app && autumn migrate
# In a second terminal:
autumn dev-loop-bench --example examples/todo-app --runs 5
```

---

## Measurement Methodology

`autumn dev-loop-bench` measures **user-visible end-to-end latency**, not
internal watcher events or process-restart log timestamps.

For each change class the tool:

1. Confirms the dev server is responding on `http://localhost:3000`.
2. Records a **save timestamp** immediately before writing the file change.
3. Polls the relevant observable endpoint (HTTP route, stylesheet URL, etc.)
   at 50 ms intervals until it observes a successful updated response or the
   timeout (2× the `max_ms` budget) expires.
4. Records an **observe timestamp** when the updated response is confirmed.
5. `duration = observe_timestamp − save_timestamp`
6. Repeats `--runs` times (default 5) and computes p50, p95, and max.

**What counts as "observable":**

| Change class | Observable result |
|---|---|
| Rust route edit | HTTP 200 response body contains the changed text |
| CSS/Tailwind edit | `Content-Length` or `ETag` of the stylesheet URL changes |
| Static asset edit | `ETag` of the asset URL changes |
| Config/watch_dirs edit | HTTP 200 response after restart (liveness probe) |
| Initial boot | First HTTP 200 on root route |

---

## Environment Metadata

Every report includes enough metadata to interpret variance without leaking
local paths or secrets:

| Field | Description |
|---|---|
| `timestamp_utc` | ISO 8601 UTC timestamp of the measurement run |
| `runner_os` | Operating system identifier (`linux`, `macos`, `windows`) |
| `rust_version` | `rustc` version used for the compile steps |
| `autumn_version` | `autumn-web` crate version |
| `example_name` | Which example was benchmarked |

Local absolute paths, usernames, and environment variables are **never
included** in the report output.

---

## Running the Benchmark

### Print the budget table (no server required)

```bash
autumn dev-loop-bench --dry-run
```

### Measure against `examples/hello`

```bash
autumn dev-loop-bench --example examples/hello --runs 5
```

### Write a machine-readable report

```bash
AUTUMN_BENCH_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
AUTUMN_BENCH_RUST_VERSION=$(rustc --version) \
autumn dev-loop-bench \
  --example examples/hello \
  --runs 5 \
  --output dev-loop-report.json
```

### Emit JSON to stdout

```bash
autumn dev-loop-bench --json
```

### Fail CI on regression

```bash
autumn dev-loop-bench --fail-on-regression
# exits 1 if any change class exceeds its budget
```

---

## Report Format

### Human-readable summary (default)

```
Autumn dev-loop latency report — 2026-05-26T12:00:00Z
Runner: linux  Rust: 1.88.0  autumn-web: 0.5.0
Example: examples/hello

Change class                                        p50 ms   p95 ms   max ms  Status
-------------------------------------------------------------------------------------------
Initial dev boot to first route                      8 234   14 102   22 450  PASS
Rust route edit (examples/hello, no-DB)              1 823    3 991    7 234  PASS
CSS/Tailwind edit to refreshed stylesheet              342      712    1 101  PASS
Static asset edit to browser reload                    198      432      890  PASS
Config edit (autumn.toml) to restarted server        2 341    5 102    9 234  PASS
Custom watch_dirs edit to restarted server           2 234    4 891    8 992  PASS

Overall: PASS
```

### Machine-readable JSON

The `--output` flag writes a JSON file suitable for archiving as release
evidence. Example structure:

```json
{
  "timestamp_utc": "2026-05-26T12:00:00Z",
  "runner_os": "linux",
  "rust_version": "rustc 1.88.0 (stable)",
  "autumn_version": "0.5.0",
  "example_name": "examples/hello",
  "all_passed": true,
  "results": [
    {
      "change_class": "css_tailwind",
      "journey_name": "CSS/Tailwind edit to refreshed stylesheet",
      "stats": {
        "p50_ms": 342,
        "p95_ms": 712,
        "max_ms": 1101,
        "sample_count": 5
      },
      "budget": {
        "p50_ms": 500,
        "p95_ms": 1000,
        "max_ms": 2000
      },
      "passed": true,
      "p95_exceeded": false,
      "max_exceeded": false,
      "p95_overage_pct": 0.0,
      "diagnosis": "",
      "next_action": ""
    }
  ]
}
```

---

## Regression Diagnostics

When a change class exceeds its budget the report names the failing user
journey, diagnoses what exceeded the limit, and proposes a concrete next
action. Example failing row:

```
CSS/Tailwind edit to refreshed stylesheet  1 234  1 450  2 100  FAIL
  ↳ Journey 'CSS/Tailwind edit to refreshed stylesheet' regressed:
      p95 1450ms exceeds budget 1000ms (45% over).
  ↳ Next: Check for new CSS plugins or a slow Tailwind config glob.
          Run `autumn dev` manually and time the Tailwind step in the log.
```

### Diagnostic actions by change class

| Change class | Typical causes | Next action |
|---|---|---|
| CSS/Tailwind | New Tailwind plugins, slow glob patterns | Profile `tailwind --watch` independently |
| Static asset | New large assets, spurious reload triggers | Check watcher filter rules |
| Rust route edit | New proc-macro deps, increased monomorphisation | Run `cargo build --timings` |
| Initial boot | New blocking startup tasks, migration growth | Review `autumn dev` startup log |
| Config/watch_dirs | New blocking startup I/O | Audit app initialisation code |

---

## Scaling: Macro-Tax Budget

The budget matrix above measures the **warm incremental** loop at a single,
small app size. Autumn leans heavily on proc-macros (`#[get]`, `#[model]`,
`#[repository]`, `#[secured]`, `#[scheduled]`). If a one-line handler edit
forces re-expansion or re-monomorphization proportional to the total
route/model count, edit-refresh latency rots silently as real apps reach
50–200 routes — invisible in CI because no example is large enough to surface
it (issue #983).

### Scaling budget

| Metric | Budget | Notes |
|---|---|---|
| p95 per size | ≤ **8 000 ms** | At every N ∈ {1, 25, 50, 100} |
| Slope (p95@N=100 / p95@N=1) | ≤ **2×** | Near-flat growth guarantee |
| Slope regression vs baseline | ≤ **20%** | Once a baseline is established; skipped until then |

**Success metric (issue #983):** single-file handler-edit warm incremental
rebuild p95 grows ≤ 2× from N=1 to N=100 routes **and** stays under 8 s
absolute on the reference runner.

### Regression allowance

Any release that **exceeds the absolute 8 s ceiling at any size**, or
**exceeds the 2× slope**, or **regresses an accepted slope baseline by more
than 20%**, must either fail the documented gate or carry an explicit
release-note exception — the same escape hatch as the
[warm-loop regression allowance](#regression-allowance).

### Synthetic-app methodology

`autumn dev-loop-bench --scaling` generates a **deterministic synthetic
Autumn app** at each requested size N (default: `{1, 25, 50, 100}`) from a
built-in code generator — not a hand-checked-in fixture. For each N the
generator produces:

- `Cargo.toml` — standalone workspace root (same `[patch.crates-io]` trick
  as `--cold-start` to point at local `autumn-web`).
- `src/schema.rs` — N `autumn_web::reexports::diesel::table!` declarations.
- `src/models.rs` — N `#[autumn_web::model]` structs.
- `src/repositories.rs` — N `#[autumn_web::repository]` traits.
- `src/handlers.rs` — N `#[get]` handlers; `handler_0` contains a
  `BENCH_EDIT_SENTINEL` constant that is bumped each run to force a
  single-file warm incremental recompile.
- `src/main.rs` — `routes![handler_0 … handler_{n-1}]`.

The generated app depends only on `autumn-web` (default features, which
include `db` + diesel-async). **No Postgres is required** — we `cargo build`
only; the binary is never started.

**Measurement procedure per size N:**

1. Write the generated app to a fresh `tempdir`.
2. Run one **warm build** (untimed) to populate the incremental cache with
   all N routes, models, and repositories compiled.
3. Repeat `--runs` times (default 5):
   a. Bump the `BENCH_EDIT_SENTINEL` in `src/handlers.rs` (exactly one
      line changed in exactly one file).
   b. Time `cargo build` end-to-end.
4. Compute p50/p95/max over the `--runs` samples.
5. Move on to the next N.

The **slope** is `p95@N_max / p95@N_min` (last vs first size in the sweep).

### Running the scaling benchmark

```bash
# Print the budget table (no build):
autumn dev-loop-bench --scaling --dry-run

# Run a fast local sweep (2 sizes, 2 runs):
AUTUMN_BENCH_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
AUTUMN_BENCH_RUST_VERSION="$(rustc --version)" \
autumn dev-loop-bench --scaling \
  --sizes 1,25 \
  --runs 2 \
  --output scaling-report.json

# Full sweep with CI regression gate:
autumn dev-loop-bench --scaling \
  --baseline benchmarks/dev-loop-scaling/baseline.json \
  --fail-on-regression

# JSON output:
autumn dev-loop-bench --scaling --json
```

### Scaling CI gate

`.github/workflows/dev-loop-scaling.yml` runs weekly (Monday 08:00 UTC,
after the warm dev-loop and cold-start gates) and on `workflow_dispatch`.
Per-PR runs are limited to the dry-run budget table and the module unit tests.
The `measure` job writes `scaling-report.json`, uploads it as a 90-day
artifact, and fails closed when `all_passed` is `false`.

The accepted slope baseline lives in
`benchmarks/dev-loop-scaling/baseline.json`. Until `"established": true` is
set, the > 20%-regression check is skipped and only the absolute (8 s) and 2×
slope gates apply. To establish the baseline after the first successful run:
set `established: true` and `accepted_slope: <measured_slope>` and document
the decision in `RELEASE_NOTES.md`.

---

## Overload / Load-Shedding Benchmark (issue #1006)

`autumn dev-loop-bench --overload` measures the Success Metric declared by
the overload-protection feature (`server.max_concurrent_requests`, see
[ADR 0009](../adr/0009-adopt-overload-protection-load-shedding.md)): under a
synthetic overload, admitted-request tail latency must stay stable and RSS
must stay bounded, while excess requests are shed near-instantly.

### Budget

| Dimension | Budget |
|---|---|
| Admitted-request p99 latency | ≤ 120% of the unloaded baseline p99 |
| Shed (`503`) response latency | ≤ 5 ms |
| RSS during the overload phase | must not grow unboundedly |

All three must pass for the run to be reported `PASS`.

### Methodology

`autumn dev-loop-bench --overload` measures a genuine live run, not a
synthetic estimate:

1. Scaffolds a minimal throwaway app (a single `/block` handler that sleeps
   `--block-ms`) and compiles it against the workspace's local `autumn-web`
   source via the same `[patch.crates-io]` trick the cold-start and scaling
   benchmarks use.
2. Boots it with `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS=<--ceiling>` and
   waits for the built-in `/live` probe to report ready.
3. **Baseline**: fires `--ceiling` concurrent requests (offered load == the
   ceiling, no shedding expected) and records admitted-request latency.
4. **Overload**: fires `--ceiling × --load-multiplier` concurrent requests
   simultaneously, classifies each response as admitted (2xx) or shed
   (`503`), and samples the child process's RSS every 30ms throughout
   (Linux only; the RSS check is skipped, not failed, elsewhere).
5. Repeats steps 3-4 `--runs` times against the same running server,
   accumulating samples, then checks the accumulated stats against the
   budget above.

### Measurement caveat: client-side overhead on constrained hardware

The shed/admitted latency samples are measured **client-side**, timed from
just before each request is sent to just after its response is received —
this necessarily includes thread-scheduling and TCP-connection overhead on
the machine running the benchmark, not just the server's processing time.
On a CPU-constrained or heavily virtualized runner, firing `ceiling ×
load_multiplier` concurrent OS threads can itself become the bottleneck,
inflating *all* measured latencies (including shed responses, which the
framework rejects in well under a millisecond server-side — verified
directly, with no network stack involved, by the `autumn/tests/integration/
load_shed.rs` and `autumn/src/middleware/load_shed.rs` test suites). If
`--overload` reports shed latency far above the 5ms budget on a busy or
small runner, prefer those in-process tests as the authoritative check of
the framework's admission-control contract, and treat the live benchmark's
absolute numbers as most meaningful on dedicated, lightly-loaded hardware —
the same caveat the warm dev-loop and cold-start benchmarks carry for CI
runner variance (see [Regression allowance](#regression-allowance)).

### Running the overload benchmark

```bash
# Print the budget table (no build, no server):
autumn dev-loop-bench --overload --dry-run

# Measure with the default ceiling (64), block time (200ms), and load (2x):
autumn dev-loop-bench --overload

# Tune the synthetic load:
autumn dev-loop-bench --overload --ceiling 32 --block-ms 150 --load-multiplier 3 --runs 3

# Fail CI on regression, writing a JSON report:
autumn dev-loop-bench --overload --fail-on-regression --output overload-report.json
```

### Report format

The JSON report carries the same environment metadata as the other
benchmark modes (`timestamp_utc`, `runner_os`, `rust_version`,
`autumn_version`) plus the run parameters (`ceiling`, `block_ms`,
`load_multiplier`) and a `result` object with each dimension's measured
value and pass/fail flag — suitable for archiving as release evidence
alongside the warm/cold-start/scaling reports.

---

## CI Gate

### Scheduled job (`.github/workflows/dev-loop-latency.yml`)

A weekly CI job runs `autumn dev-loop-bench --fail-on-regression` against
`examples/hello` and uploads the JSON report as a workflow artifact. The job
runs on an `ubuntu-latest` GitHub Actions runner.

For checks that are **too flaky or expensive for every PR** (live browser
polling, database-backed paths), the job is scheduled weekly and can be
triggered manually via `workflow_dispatch`. These checks are excluded from
per-PR required status checks.

### Cold-start scheduled job (`.github/workflows/cold-start-latency.yml`)

The cold-start onboarding budget is gated by a sibling workflow that mirrors this
gating model. A full cold compile is far too slow and variable for a per-PR
required check, so live measurement runs only on a **weekly schedule** and on
manual `workflow_dispatch`; per-PR runs are limited to the `--cold-start
--dry-run` budget-table smoke-check and the measurement unit tests. The
measurement job writes a JSON report, uploads it as an artifact, and fails when
`all_passed` is `false` (the no-DB `hello` shape exceeded its budget). The
database-backed shape is opt-in via the `include_db` dispatch input and is
informational only.

```bash
gh workflow run cold-start-latency.yml --ref your-branch-name
```

### Per-PR opt-in

To run the latency gate against a specific PR branch before merging:

```bash
gh workflow run dev-loop-latency.yml --ref your-branch-name
```

### Release gate

The release checklist (`docs/release-checklist.md`) requires that the most
recent weekly latency run **passed** before a release is tagged, or that an
explicit release-note exception is documented explaining why a regression is
acceptable.

---

## Comparison with Other Frameworks

Autumn's dev-loop budget is designed to be competitive with Rails and Phoenix
for the non-Rust-compile change classes (CSS, static assets, config) while
being honest that Rust warm incremental compiles are slower than Ruby/Elixir
interpreted reloads.

| Change type | Rails/Phoenix | Autumn budget (p95) |
|---|---|---|
| CSS/static reload | < 500 ms | 1 000 ms |
| Config reload | < 2 s | 8 000 ms |
| Code change (server restart) | < 2 s | 5 000 ms (hello) / 10 000 ms (DB) |

The 5 s / 10 s Rust budgets reflect warm incremental compilation with
`cargo build`, not cold builds. Cold builds on underpowered CI runners are
explicitly excluded from the per-PR gate (see `max_ms` limits) and documented
in release notes when they regress.

---

## Out of Scope

- **Hot Module Replacement** that patches Rust handlers without a process
  restart — this is a separate feature tracked in its own issue.
- **Production runtime latency** — this document covers local development only.
- **Browser visual regression testing** — screenshot assertions are not part of
  this budget.
- **Reducing** cold compile time (dependency trimming, `codegen-units`, linker
  swaps, prebuilt artifacts) — the [Cold-Start Onboarding
  Budget](#cold-start-onboarding-budget) above *measures and gates* cold start;
  optimizing it is a separate, evidence-driven slice that this measurement
  unlocks.
