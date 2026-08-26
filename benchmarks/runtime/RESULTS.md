# Benchmark Results

> This file contains two sections:
> 1. **Autumn CI gate baseline** — committed real numbers for the two gated paths,
>    produced by the `runtime-latency.yml` workflow. Updated when `budgets.toml` is re-baselined.
> 2. **Full comparative run** — the six-framework comparison.
>
> Machine-readable gate baseline: [`baseline.json`](baseline.json).
> Raw k6 JSON output files are in `load/results/<timestamp>/`.

## Autumn CI Gate Baseline

> Gated paths measured locally with a fixed k6 profile (VUs=20, duration=30s, k6 v0.55.0).
> Methodology: 1 discarded warmup + 3 measured runs; median p99 reported.
> See [`baseline.json`](baseline.json) for full per-run data and [`budgets.toml`](budgets.toml) for CI thresholds.

| Field | Value |
|-------|-------|
| Date | 2026-06-22 |
| Host OS | Linux 6.18.5 x86_64 |
| CPU | Intel Xeon @ 2.80GHz, 4 vCPU |
| RAM | 15 GiB |
| Postgres | 16 |
| k6 version | v0.55.0 |
| Track | Autumn-only (no container limits; local bare-metal) |
| VUs | 20 |
| Duration | 30s |
| Methodology | 1 warmup (discarded) + 3 measured runs, median p99 |

| Path | Run 1 p99 | Run 2 p99 | Run 3 p99 | Median p99 | CI Budget |
|------|-----------|-----------|-----------|------------|-----------|
| `GET /api/posts` (JSON) | 4.7ms | 4.9ms | 5.6ms | 4.9ms | 50ms |
| `GET /posts` (HTML) | 3.9ms | 4.2ms | 4.4ms | 4.2ms | 50ms |

The 50ms CI budget floor accounts for shared GitHub Actions runner overhead
(typically 5-20× slower than local bare-metal due to CPU contention). The gate
catches a ≥25% regression relative to CI steady-state performance.

### Re-verification — 2026-08-26 (autumn-web 0.7.0)

Same methodology, same k6 profile, re-run after the 0.5.0 → 0.7.0 performance
work (notably the `AutumnConfig` request-path deep-clone removal in #2203 and
the deferred `error_id` allocation in #2304).

| Path | Run 1 p99 | Run 2 p99 | Run 3 p99 | Median p99 | vs 0.5.0 baseline |
|------|-----------|-----------|-----------|------------|-------------------|
| `GET /api/posts` (JSON) | 4.88ms | 4.97ms | 4.59ms | **4.88ms** | −0.04ms (−0.8%) |
| `GET /posts` (HTML) | 4.17ms | 4.43ms | 4.23ms | **4.23ms** | ±0.00ms (0.0%) |

`bench-runtime-gate` reports PASS on both paths.

**Interpretation:** the gated read paths are unchanged within run-to-run noise.
This is the expected result, not a null finding to explain away — both gated
paths are single-query reads at 20 VUs, where wall-clock is dominated by the
Postgres round-trip and response serialization. The 0.5.0 → 0.7.0 work removed
per-request *allocations and instructions*, which this profile is not shaped to
resolve. It does confirm no regression was introduced. Note the re-verification
host is nominally slower than the baseline host (2.10GHz vs 2.80GHz Xeon), so
identical p99 on the slower box is a marginally favourable result.

`budgets.toml` and `baseline.json` are deliberately **left unchanged**: the
medians moved by less than 1%, so there is no intentional shift to re-baseline
against (see README § "Re-baselining the Latency Budget").

---

## Full Comparative Run

## Run Metadata

| Field | Value |
|-------|-------|
| Date | 2026-08-26 |
| Host OS | Linux 6.18.44 x86_64 (Ubuntu 24.04) |
| CPU | Intel Xeon @ 2.10GHz, 4 vCPU |
| RAM | 16 GiB |
| Docker Engine | **not used — see Deviations** |
| Container limits | **none — apps run natively on the host** |
| Postgres | 16.13, `max_connections=300` |
| k6 version | v0.55.0 |
| Track | comparable-infrastructure (native variant — see Deviations) |
| VUs | 20 |
| Duration | 30s |
| Methodology | 1 warmup pass (discarded) + 3 measured runs; medians reported |
| Frameworks run | Autumn, Spring Boot, Rails, Django, Loco (**Phoenix not run**) |

### Versions Exercised

| Framework | Version | Runtime | Server |
|-----------|---------|---------|--------|
| Autumn | autumn-web 0.7.0 | Rust 1.94.1 | Axum 0.8.9 |
| Spring Boot | 3.3.4 | OpenJDK 21.0.10 | Tomcat (embedded) |
| Rails | 7.2.3.2 | Ruby 3.3.6 | Puma 7.2.1, 20 threads |
| Django | 5.1.15 | Python 3.11.15 | Gunicorn 23.0.0 + Uvicorn 0.32.1, 4 workers |
| Loco | 0.14.1 | Rust 1.94.1 | Axum 0.8 |
| Phoenix | — | — | not run (see Deviations) |

## JSON CRUD — Latency (p50 / p95 / p99) and Throughput

| Framework | p50 | p95 | p99 | req/s | Error rate |
|-----------|-----|-----|-----|-------|------------|
| Autumn | 1.56ms | 3.84ms | 5.56ms | 904 | 0.00% |
| Spring Boot | 1.57ms | 3.92ms | 6.68ms | 899 | 0.00% |
| Rails | 61.82ms | 106.82ms | 135.20ms | 245 | 0.00% |
| Django | 91.86ms | 161.51ms | 228.33ms | 189 | 0.23% |
| Phoenix | not run | not run | not run | not run | not run |
| Loco | 1.45ms | 3.30ms | 5.00ms | 909 | 0.00% |

## HTML Page — Latency (p50 / p95 / p99) and Throughput

| Framework | p50 | p95 | p99 | req/s | Error rate |
|-----------|-----|-----|-----|-------|------------|
| Autumn | 1.20ms | 2.53ms | 3.76ms | 386 | 0.00% |
| Spring Boot | 1.50ms | 3.03ms | 4.69ms | 384 | 0.00% |
| Rails | 15.22ms | 40.23ms | 57.27ms | 293 | 0.00% |
| Django | 48.07ms | 129.37ms | 189.44ms | 180 | 0.00% |
| Phoenix | not run | not run | not run | not run | not run |
| Loco | 0.92ms | 2.11ms | 3.10ms | 388 | 0.00% |

## Validation Failure Path — Latency (p50 / p95 / p99)

| Framework | p50 | p95 | p99 | 422 rate |
|-----------|-----|-----|-----|---------|
| Autumn | 1.32ms | 2.48ms | 3.29ms | 100% |
| Spring Boot | 0.97ms | 1.83ms | 2.56ms | 100% |
| Rails | 2.28ms | 6.10ms | 27.55ms | 100% |
| Django | 2.62ms | 6.53ms | 12.09ms | 100% |
| Phoenix | not run | not run | not run | not run |
| Loco | 0.60ms | 1.08ms | 1.95ms | 100% |

## Auth-Protected Route — Latency (p50 / p95 / p99)

| Framework | p50 | p95 | p99 | req/s |
|-----------|-----|-----|-----|-------|
| Autumn | 1.31ms | 2.38ms | 3.50ms | 384 |
| Spring Boot | 1.18ms | 2.58ms | 4.23ms | 385 |
| Rails | 3.85ms | 14.74ms | 26.33ms | 355 |
| Django | 35.59ms | 79.27ms | 96.76ms | 222 |
| Phoenix | not run | not run | not run | not run |
| Loco | 1.01ms | 1.87ms | 2.83ms | 387 |

## Cold Start Time

Native process start → first `200` on `GET /api/posts` (not container start).

| Framework | Time to first 200 |
|-----------|------------------|
| Autumn | 0.13s |
| Spring Boot | 5.48s |
| Rails | 1.58s |
| Django | 0.34s |
| Phoenix | not run |
| Loco | 0.12s |

## Warm Restart Time

Not measured — see Deviations (requires the container lifecycle).

## Idle RSS (after 30 s idle)

Summed across each app's process tree. **Not comparable to the documented
512 MiB-capped container track**: with no memory cgroup, the JVM sizes its heap
from host RAM (`MaxRAMPercentage=75.0` of 16 GiB, not of 512 MiB), and Gunicorn
runs 4 independent worker processes.

| Framework | RSS |
|-----------|-----|
| Autumn | 29 MiB |
| Spring Boot | 580 MiB |
| Rails | 130 MiB |
| Django | 390 MiB |
| Phoenix | not run |
| Loco | 32 MiB |

## Container Image Size

Not measured — Docker unavailable (see Deviations). Deployable-artifact sizes
are given instead, and are **not** equivalent to image sizes:

| Framework | Artifact | Size |
|-----------|----------|------|
| Autumn | release binary | 28 MiB |
| Spring Boot | fat jar | 52 MiB |
| Rails | installed gem tree | 101 MiB |
| Django | virtualenv | 120 MiB |
| Phoenix | — | not run |
| Loco | release binary | 37 MiB |

## Memory Under Load (during 60 s json-crud run)

Not measured — requires `docker stats` sampling against the container track.

## Build Time and Test Time (DX signals)

`docker compose build` not measurable here. Native release/package build times:

| Framework | Build | Test suite time |
|-----------|-------|----------------|
| Autumn | 4m40s (`cargo build --release`, cold) | not measured |
| Spring Boot | ~1m (`mvn package -DskipTests`, cold) | not measured |
| Rails | ~1m (`bundle install`, cold) | not measured |
| Django | ~10s (`pip install`, cold) | not measured |
| Phoenix | not run | not run |
| Loco | 3m56s (`cargo build --release`, cold) | not measured |

## Notes and Caveats

### Deviations from the documented methodology

This run deviates from the comparable-infrastructure track in
[`README.md`](README.md). Every deviation is listed here; none of them
advantages Autumn specifically.

1. **No containers.** The environment's egress policy denies
   `production.cloudfront.docker.com` (Docker Hub's blob CDN), so no images
   could be pulled and `docker compose` could not be used. All five apps were
   built and run natively on the host instead. Consequently there are **no
   2 vCPU / 512 MiB per-app limits** — every app had the full 4 vCPU host
   available. This inflates the absolute numbers for all frameworks relative
   to the capped track and makes the memory figures non-comparable to it.
2. **Phoenix was not run.** Elixir/OTP is not installed in this environment and
   `repo.hex.pm` is denied by the same egress policy, so the app could not be
   built. All Phoenix cells above read "not run" rather than being left blank,
   so they are not mistaken for a zero.
3. **Python 3.11.15, not 3.12.** The Django `VERSIONS` file and Dockerfile
   specify 3.12; the host provides 3.11. Django 5.1 supports both.
4. **Per-framework databases.** Each app was given its own database
   (`bench_autumn`, `bench_springboot`, …) on the one Postgres 16 instance,
   each built from the canonical [`schema/init.sql`](schema/init.sql) and
   re-seeded from [`seed/seed.sql`](seed/seed.sql) before every measured run.
   The compose track shares a single database; isolating them keeps one
   framework's `json-crud` writes from perturbing the next framework's dataset,
   and guarantees all five start each run from a byte-identical 1 000-post seed.
5. **App-owned migrations were skipped, not re-run.** The canonical schema is
   the source of truth. Django ran `migrate --fake-initial`, Rails had its
   migration pre-recorded in `schema_migrations`, and Flyway was baselined at
   V1. This is a workaround for a latent harness problem — see below.
6. **Load generator shares the host** with the app under test (4 vCPU total),
   as it would in the documented local `run.sh` flow.
7. **Idle apps stayed running** during each other's measurements, matching the
   compose track where all services are up simultaneously.

### `max_connections` is a hard requirement for native runs too

The first full pass of this suite produced badly degraded Loco and Django
numbers (Loco: a flat ~501ms on every DB-touching path with a 50% failure rate;
Django: a 60% failure rate). Neither was a framework characteristic. The five
apps' idle pools held **99 of the default 100** Postgres connections, so Loco
could not acquire a connection at all and hit its configured 500ms
`connect_timeout` on every request.

`docker-compose.yml` already sets `max_connections=300` on the Compose Postgres
service for exactly this reason, and the README documents it — but only for the
Compose track. Anyone running the suite against a local Postgres must set it
too. That first pass was discarded in full and the suite re-run from a warmup
pass after raising the limit; only the corrected numbers appear above.

### Throughput numbers are latency-bound, not saturation throughput

Every k6 scenario script includes a `sleep()` between iterations, which caps
request rate at roughly `VUs × requests-per-iteration ÷ (sleep + latency)`.
At 20 VUs the html-page ceiling is about 390 req/s, and Autumn, Spring Boot and
Loco all sit within 2% of it — they are *idle-waiting*, not saturated. The
req/s column therefore measures "requests served at a fixed offered load",
and only Rails and Django are actually latency-limited below the ceiling.
**Do not read these as capacity numbers.** Measuring sustained throughput
needs a saturating profile (no sleep, ramping VUs) that this suite does not
currently define.

### Framework-specific notes

- **Autumn vs. Loco.** Both are Rust + Axum. Loco is consistently a little
  faster (e.g. 0.92ms vs 1.20ms html p50), which is the expected shape: the
  Autumn bench app runs its default middleware stack — access logging, request
  IDs, inbound timeouts, body-size limits, trusted-host checks, sessions —
  while the Loco app runs a much thinner one. The gap measures feature
  assumptions, not language or router overhead.
- **Spring Boot** is within noise of Autumn on p50 after JIT warm-up, and
  trails on p99 (6.68ms vs 5.56ms on json-crud). The warmup pass matters here
  more than for any other framework; unwarmed numbers were substantially worse.
- **Django** shows a 0.23% error rate on json-crud (99.04% k6 check pass rate)
  under this profile — 4 Gunicorn/Uvicorn workers saturate before the offered
  load is met. Its sync ORM under an ASGI worker class is the documented
  bottleneck.
- **Rails**' `validation-fail` p99 (27.55ms) is far above its p95 (6.10ms), a
  much wider spread than any other framework on that path.

### Reproducing

Raw per-run k6 summary exports (60 files: 5 frameworks × 4 scenarios × 3 runs)
are written under `results/` and are gitignored. Re-run with
`load/run.sh <framework> <base_url>` per framework, or see README
§ "Running the Benchmark" for the Compose flow.
