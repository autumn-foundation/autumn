# Benchmark Results

> This file contains two sections:
> 1. **Autumn CI gate baseline** — committed real numbers for the two gated paths,
>    produced by the `runtime-latency.yml` workflow. Updated when `budgets.toml` is re-baselined.
> 2. **Full comparative run template** — fill in after a manual six-framework run.
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

---

## Full Comparative Run Template

> Fill in this section after a manual six-framework comparable-infrastructure run.

## Run Metadata

| Field | Value |
|-------|-------|
| Date | _YYYY-MM-DD_ |
| Host OS | _e.g. Ubuntu 24.04 LTS_ |
| CPU | _e.g. AMD EPYC 7742 4 vCPU_ |
| RAM | _e.g. 16 GiB_ |
| Docker Engine | _e.g. 27.x_ |
| Container limits | 2 vCPU, 512 MiB per app |
| Postgres | 16-alpine |
| k6 version | _e.g. 0.55.0_ |
| Track | comparable-infrastructure / idiomatic |
| VUs | _e.g. 50_ |
| Duration | _e.g. 60s_ |

## Cold Start Time

| Framework | Time to first 200 /health |
|-----------|--------------------------|
| Autumn | |
| Spring Boot | |
| Rails | |
| Django | |
| Phoenix | |
| Loco | |

## Warm Restart Time

Send `SIGTERM` to the running container; time until `/health` returns 200 again.

| Framework | Warm restart time |
|-----------|-----------------|
| Autumn | |
| Spring Boot | |
| Rails | |
| Django | |
| Phoenix | |
| Loco | |

## Idle RSS (after 30 s idle)

| Framework | RSS |
|-----------|-----|
| Autumn | |
| Spring Boot | |
| Rails | |
| Django | |
| Phoenix | |
| Loco | |

## Container Image Size

| Framework | Image size |
|-----------|-----------|
| Autumn | |
| Spring Boot | |
| Rails | |
| Django | |
| Phoenix | |
| Loco | |

## JSON CRUD — Latency (p50 / p95 / p99) and Throughput

| Framework | p50 | p95 | p99 | req/s | Error rate |
|-----------|-----|-----|-----|-------|------------|
| Autumn | | | | | |
| Spring Boot | | | | | |
| Rails | | | | | |
| Django | | | | | |
| Phoenix | | | | | |
| Loco | | | | | |

## HTML Page — Latency (p50 / p95 / p99) and Throughput

| Framework | p50 | p95 | p99 | req/s | Error rate |
|-----------|-----|-----|-----|-------|------------|
| Autumn | | | | | |
| Spring Boot | | | | | |
| Rails | | | | | |
| Django | | | | | |
| Phoenix | | | | | |
| Loco | | | | | |

## Validation Failure Path — Latency (p50 / p95 / p99)

| Framework | p50 | p95 | p99 | 422 rate |
|-----------|-----|-----|-----|---------|
| Autumn | | | | |
| Spring Boot | | | | |
| Rails | | | | |
| Django | | | | |
| Phoenix | | | | |
| Loco | | | | |

## Auth-Protected Route — Latency (p50 / p95 / p99)

| Framework | p50 | p95 | p99 | req/s |
|-----------|-----|-----|-----|-------|
| Autumn | | | | |
| Spring Boot | | | | |
| Rails | | | | |
| Django | | | | |
| Phoenix | | | | |
| Loco | | | | |

## Memory Under Load (during 60 s json-crud run)

| Framework | Peak RSS | Average RSS |
|-----------|----------|-------------|
| Autumn | | |
| Spring Boot | | |
| Rails | | |
| Django | | |
| Phoenix | | |
| Loco | | |

## Build Time and Test Time (DX signals)

| Framework | `docker compose build` | Test suite time |
|-----------|----------------------|----------------|
| Autumn | | |
| Spring Boot | | |
| Rails | | |
| Django | | |
| Phoenix | | |
| Loco | | |

## Notes and Caveats

_Document any deviations from the standard methodology here._
