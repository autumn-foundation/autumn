# Benchmark Results

> This file contains two sections:
> 1. **Autumn CI gate baseline** — committed real numbers for the two gated paths,
>    produced by the `runtime-latency.yml` workflow. Updated when `budgets.toml` is re-baselined.
> 2. **Comparative run** — Autumn 0.10.0 against the other five frameworks on
>    the comparable-infrastructure track (2026-11-06).
>
> Machine-readable gate baseline: [`baseline.json`](baseline.json).
> Raw k6 JSON output from a run is written to `results/<timestamp>/` (ignored by Git).

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

## Comparative Run — Autumn 0.10.0 (2026-11-06)

Autumn 0.10.0 measured against current releases of the other five frameworks.
Every app implements the same routes and schema ([README: Methodology](README.md#methodology))
on the same container limits.
The tables are generated from `results/2026-11-06/` (raw k6 summaries; ignored
by Git) with `load/aggregate.py`.

### Run Metadata

| Field | Value |
|-------|-------|
| Date | 2026-11-06 |
| Host OS | Linux 6.18.44 x86_64 |
| CPU | Intel Xeon @ 2.10 GHz, 4 vCPU |
| RAM | 15 GiB |
| Docker Engine | 29.9.2 |
| Container limits | 2 vCPU, 512 MiB per app |
| Postgres | 16.15 (16-alpine, `max_connections=300`) |
| k6 version | v2.3.0 (`grafana/k6:2.3.0`, host network) |
| Track | comparable-infrastructure |
| Matrix | 50 VUs, 60 s per scenario, 3 runs; medians reported |
| Saturation | 200 VUs, 30 s, 2 runs; `json-crud` and `html-page` only |
| Warmup | 50 VUs, 30 s of `json-crud` and `html-page` per app, discarded |
| DB pool | 20 connections per app |

| Framework | Version | Stack |
|-----------|---------|-------|
| Autumn | 0.10.0 | Rust 1.97, Tokio 1.54, Axum 0.8, Diesel 2.3 + diesel-async, Maud |
| Spring Boot | 4.1.2 | Java 25, Tomcat 11, Hibernate 7.4, HikariCP |
| Rails | 8.1.4.1 | Ruby 4.0 + YJIT, Puma 8 |
| Django | 6.1.3 | Python 3.14, Gunicorn 26 + Uvicorn 0.54 (ASGI), psycopg 3.3 pool |
| Phoenix | 1.8.16 | Elixir 1.19 / OTP 28, Bandit 1.14, Ecto 3.14 |
| Loco | 1.3.1 | Rust, Axum 0.8, SeaORM 2.0 |

Exact versions are in each app's `VERSIONS` file and lockfile.

Reproduce with:

```bash
cd benchmarks/runtime
docker compose up -d --build
./load/resources.sh --out results/resources.csv
./load/run.sh all --vus 50 --duration 60s --repeat 3
./load/run.sh all --vus 200 --duration 30s --repeat 2 --scripts json-crud,html-page
```

### Summary

- **At 50 VUs, four apps hit the load profile's cap.** Autumn, Loco, Phoenix
  and Spring Boot all serve the throughput the scripts' sleeps allow (about
  950 req/s on the one-request scenarios, about 1,900 on JSON CRUD), with no
  failed requests. Latency is what separates them. Autumn, Loco and Phoenix are
  within about 2 ms of each other at p99 on every scenario. Spring Boot is
  close on the light paths and 1.5–1.7× slower at p95/p99 on JSON CRUD and
  the HTML page.
- **Rails and Django are CPU-bound at 50 VUs.** On 2 vCPU they top out near
  500 req/s on JSON CRUD, with p99 latency between 140 and 220 ms.
- **At 200 VUs Loco has the highest ceiling.** Loco serves about 3,260 req/s on
  both saturation scenarios. Autumn follows at 2,844 (JSON CRUD) and 2,979
  (HTML), then Phoenix, then Spring Boot at about 2,000. Rails and Django stay
  between 430 and 580. Every app ran the saturation pass with 0% failed requests.
- **Autumn vs. Loco**, the two Axum stacks: Loco is ahead by 15% on JSON CRUD
  and 9% on HTML at saturation, and has a lower p50 on three of the four
  50-VU scenarios (Autumn's is lower on JSON CRUD). Autumn spends 22–37% more
  CPU per request, and most of the difference is its default middleware
  stack; see [Where the Autumn–Loco gap comes from](#where-the-autumnloco-gap-comes-from).
- **Footprint.** Autumn and Loco idle at about 4 MiB and peak under 20 MiB
  under load. The other four idle between 84 and 198 MiB. Autumn and Loco also
  cold-start in about 1 s, migrations included; Spring Boot takes about 9.5 s.

### Cold Start Time

`docker compose up -d` on a removed container until the first 200 from the
health route. The time includes migrations and pool setup.

| Framework | Time to first 200 |
|-----------|-------------------|
| Autumn | 1.06 s |
| Spring Boot | 9.49 s |
| Rails | 3.46 s |
| Django | 3.43 s |
| Phoenix | 2.39 s |
| Loco | 1.02 s |

### Warm Restart Time

`docker compose stop` (SIGTERM until the process exits), then
`docker compose start` until the health route answers 200 again.

| Framework | Stop (SIGTERM → exit) | Start → first 200 | Total |
|-----------|-----------------------|-------------------|-------|
| Autumn | 5.43 s | 0.91 s | 6.34 s |
| Spring Boot | 0.40 s | 9.31 s | 9.71 s |
| Rails | 0.39 s | 3.32 s | 3.71 s |
| Django | 0.78 s | 3.15 s | 3.92 s |
| Phoenix | 1.55 s | 2.06 s | 3.61 s |
| Loco | 0.31 s | 0.89 s | 1.20 s |

Autumn's stop time is its default 5 s pre-stop drain window
(`server.prestop_grace_secs`). On SIGTERM, `/ready` turns 503 and the listener
stays open for that window, so a load balancer can take the replica out of
rotation before connections are refused. Set `prestop_grace_secs = 0` for an
immediate stop.

### Idle Memory (after 30 s idle)

`docker stats` memory of the container 30 s after it turned healthy.

| Framework | Memory |
|-----------|--------|
| Autumn | 4.4 MiB |
| Spring Boot | 198 MiB |
| Rails | 84 MiB |
| Django | 188 MiB |
| Phoenix | 125 MiB |
| Loco | 3.8 MiB |

### Container Image Size

| Framework | Image size |
|-----------|------------|
| Autumn | 186 MiB |
| Spring Boot | 400 MiB |
| Rails | 435 MiB |
| Django | 311 MiB |
| Phoenix | 239 MiB |
| Loco | 167 MiB |

### JSON CRUD — 50 VUs

List, create, show, update and delete a post: five requests per iteration and
a 0.1 s sleep.

| Framework | p50 | p95 | p99 | req/s | Failed requests |
|-----------|-----|-----|-----|-------|------|
| Autumn | 4.72 ms | 15.4 ms | 22.8 ms | 1,887 | 0% |
| Spring Boot | 6.20 ms | 23.2 ms | 34.7 ms | 1,732 | 0% |
| Rails | 78.7 ms | 114 ms | 143 ms | 497 | 0% |
| Django | 65.6 ms | 189 ms | 219 ms | 496 | 0% |
| Phoenix | 4.52 ms | 14.2 ms | 21.2 ms | 1,922 | 0% |
| Loco | 5.15 ms | 15.5 ms | 22.7 ms | 1,867 | 0% |

### HTML Page — 50 VUs

A server-rendered list page and a detail page: two requests per iteration and
a 0.1 s sleep.

| Framework | p50 | p95 | p99 | req/s | Failed requests |
|-----------|-----|-----|-----|-------|------|
| Autumn | 1.95 ms | 5.03 ms | 8.31 ms | 947 | 0% |
| Spring Boot | 2.61 ms | 8.25 ms | 14.1 ms | 927 | 0% |
| Rails | 35.9 ms | 67.3 ms | 86.2 ms | 563 | 0% |
| Django | 56.4 ms | 129 ms | 171 ms | 448 | 0% |
| Phoenix | 1.88 ms | 4.88 ms | 8.20 ms | 948 | 0% |
| Loco | 1.55 ms | 4.07 ms | 7.02 ms | 955 | 0% |

### Validation Failure Path — 50 VUs

An invalid `POST /api/posts` that must return 422: one request per iteration
and a 0.05 s sleep.

| Framework | p50 | p95 | p99 | req/s | 422 rate |
|-----------|-----|-----|-----|-------|------|
| Autumn | 1.71 ms | 3.86 ms | 6.35 ms | 948 | 100% |
| Spring Boot | 1.36 ms | 3.40 ms | 5.53 ms | 955 | 100% |
| Rails | 13.5 ms | 37.3 ms | 57.0 ms | 748 | 100% |
| Django | 20.6 ms | 47.9 ms | 62.5 ms | 679 | 100% |
| Phoenix | 1.21 ms | 2.83 ms | 4.58 ms | 960 | 100% |
| Loco | 1.03 ms | 2.49 ms | 4.54 ms | 962 | 100% |

### Auth-Protected Route — 50 VUs

A bearer-token route hit with a valid token, no token and a bad token (200,
401, 401): one request per iteration and a 0.05 s sleep. The 401s are expected
responses and do not count as failed requests.

| Framework | p50 | p95 | p99 | req/s | Failed requests |
|-----------|-----|-----|-----|-------|------|
| Autumn | 1.93 ms | 4.47 ms | 7.46 ms | 944 | 0% |
| Spring Boot | 1.76 ms | 4.40 ms | 7.44 ms | 948 | 0% |
| Rails | 22.8 ms | 46.8 ms | 60.1 ms | 666 | 0% |
| Django | 46.5 ms | 83.0 ms | 119 ms | 508 | 0% |
| Phoenix | 1.95 ms | 4.32 ms | 7.06 ms | 946 | 0% |
| Loco | 1.56 ms | 3.61 ms | 6.03 ms | 953 | 0% |

### Saturation — 200 VUs

The 50-VU matrix is capped by the scripts' per-iteration sleeps: 50 VUs that
each sleep 0.05 s cannot send much more than 1,000 requests per second. Four of
the six apps reach that cap, so the matrix separates them by latency rather
than throughput. This pass quadruples the clients to find each app's ceiling
on 2 vCPU.

**JSON CRUD**

| Framework | p50 | p95 | p99 | req/s | Failed requests |
|-----------|-----|-----|-----|-------|------|
| Autumn | 46.8 ms | 86.9 ms | 124 ms | 2,844 | 0% |
| Spring Boot | 62.5 ms | 173 ms | 243 ms | 2,110 | 0% |
| Rails | 378 ms | 486 ms | 540 ms | 494 | 0% |
| Django | 408 ms | 553 ms | 648 ms | 453 | 0% |
| Phoenix | 56.8 ms | 95.4 ms | 121 ms | 2,644 | 0% |
| Loco | 37.4 ms | 83.8 ms | 114 ms | 3,262 | 0% |

**HTML page**

| Framework | p50 | p95 | p99 | req/s | Failed requests |
|-----------|-----|-----|-----|-------|------|
| Autumn | 12.8 ms | 40.4 ms | 60.3 ms | 2,979 | 0% |
| Spring Boot | 50.0 ms | 118 ms | 156 ms | 1,903 | 0% |
| Rails | 291 ms | 355 ms | 405 ms | 576 | 0% |
| Django | 414 ms | 591 ms | 705 ms | 427 | 0% |
| Phoenix | 17.4 ms | 42.8 ms | 57.8 ms | 2,843 | 0% |
| Loco | 7.34 ms | 28.4 ms | 45.9 ms | 3,260 | 0% |

### Where the Autumn–Loco gap comes from

Both apps were profiled with the same setup:
- release builds with frame pointers;
- run on the host, pinned to CPUs 0–1, with k6 and Postgres on CPUs 2–3;
- `perf record -F 1999 -g` for 20 s of a 200-VU run.

The app's CPU time per request comes from its process counters. Each sample is
charged to the innermost frame that names a crate or an Autumn module, so
allocator, `memcpy` and syscall time lands on the code that asked for it.

CPU per request without perf attached:

| Scenario | Autumn | Loco | Autumn costs |
|----------|--------|------|--------------|
| JSON CRUD | 274 µs | 224 µs | +50 µs (+22%) |
| HTML page | 288 µs | 210 µs | +78 µs (+37%) |

Where the HTML page's time goes, in µs per request (perf attached, so both
columns run higher):

| Bucket | Autumn | Loco | Difference |
|--------|--------|------|------------|
| Framework's own layers and services | 55.6 | 3.8 | **+51.8** |
| tokio I/O, scheduler and their syscalls | 113.2 | 91.6 | +21.6 |
| `http` / `bytes` (header maps, buffers) | 23.9 | 13.3 | +10.6 |
| axum routing and extractors | 29.2 | 25.1 | +4.1 |
| hyper | 11.9 | 8.2 | +3.7 |
| Handler and template | 17.3 | 13.1 | +4.2 |
| ORM and driver (Diesel + tokio-postgres; SeaORM + sqlx) | 65.9 | 72.6 | −6.7 |

JSON CRUD shows the same pattern. Autumn's layers cost 51.7 µs against
Loco's 4.0, and header work is 11 µs higher. Autumn's ORM and handler path is
14 µs cheaper, which offsets part of that.

So the gap is the middleware stack, not the database path: Autumn's ORM and
driver cost less than Loco's. No single layer dominates. The 52 µs is spread
over about 30 layers and services that each cost 0.5–5 µs on every request:
- the `Db` extractor (8–11 µs: pool checkout and moving a large future);
- the custom HTTP/1 accept path (`http_server`, about 6 µs);
- metrics, including formatting each query for `record_db_query` (about 4 µs);
- `AppState` lookups (3 µs);
- the session layer (3 µs);
- `Accept` negotiation for agent documents (`aeo`, 3 µs), and request IDs,
  the access log and error reporting (2–2.5 µs each);
- the startup barrier, trusted-host check, request timeout, deadline, load
  shedding, submit tokens, trusted proxies, the error-page and exception
  filters, and maintenance mode (0.5–1.7 µs each).

The extra header work is the eleven response headers Autumn writes (the
security headers, `x-request-id` and `vary: Accept`) against Loco's five.

Two costs come from syscalls rather than code. Both apps make about ten
syscalls per request, but Autumn's include 2.2 `getrandom` calls that Loco does
not make:
- The session layer mints a UUID v4 from the OS for every request without a
  cookie, even when the session is never written.
- The request ID is another OS-drawn UUID. Loco also sends a request ID, but it
  generates it in user space.

### Memory Under Load (60 s `json-crud`, 50 VUs)

`load/sample-mem.sh` sampled `docker stats` about every 2 s during the run.
This pass ran right after the saturation pass, on the same containers, so it
also shows what each runtime holds on to after peak load.

| Framework | Peak | Mean |
|-----------|------|------|
| Autumn | 20 MiB | 18 MiB |
| Spring Boot | 509 MiB | 509 MiB |
| Rails | 175 MiB | 174 MiB |
| Django | 314 MiB | 313 MiB |
| Phoenix | 195 MiB | 194 MiB |
| Loco | 20 MiB | 19 MiB |

Spring Boot's JVM keeps the heap and the Tomcat thread stacks it committed
during the 200-client pass, so it stays near the 512 MiB limit afterwards. On
a freshly restarted container (a 30 s warmup, then the same 60 s run) it peaks
at 294 MiB with a mean of 286 MiB.

### Build Time and Test Time

Not measured in this run. Every image was built from a warm Docker layer
cache, so the build times would not be comparable.

### Notes and Caveats

- **Throughput at 50 VUs is capped by the scripts.** Each iteration sleeps
  0.05 s or 0.1 s, so 50 VUs cannot send much more than about 1,000 requests a
  second on a one-request scenario. Equal req/s in the matrix means "reached the
  cap", not "equally fast". The saturation pass is the throughput comparison.
- **One host runs everything.** k6, Postgres and the app under test share the
  4-vCPU host. Each app is limited to 2 vCPU and is the only one under load at a
  time, but at 200 VUs k6 itself competes for the remaining CPU. Read the
  saturation figures as ceilings on this host, not absolute capacity.
- **Spring Boot was re-run after a heap fix.** The first saturation pass ran
  the JVM with `-XX:MaxRAMPercentage=75`, a 384 MiB heap inside the 512 MiB
  limit. Under 200 clients the cgroup OOM killer stopped the JVM twice (kernel
  log: `Memory cgroup out of memory: Killed process … (java)`). The HTML
  scenario failed two thirds of its requests, and JSON CRUD lost throughput
  while the JVM restarted (medians of the two runs):

| Scenario | req/s | p99 | Failed requests |
|----------|-------|-----|-----------------|
| json-crud | 1,372 | 642 ms | 0% |
| html-page | 1,595 | 897 ms | 66.67% |

  The Dockerfile now gives the heap 50%. Every Spring Boot row above (matrix,
  saturation, memory, resources) comes from the re-run with that setting. No
  other app was OOM-killed or restarted during the run.
- **Rails and Django stop times were re-measured.** Both ran their server under
  `sh -c` as PID 1, which does not forward SIGTERM, so Docker sent SIGKILL
  after its 10 s timeout. The commands now `exec` the server. The tables show
  the re-measured rows; the first pass recorded 10.4 s stops for both.
- **Django uses psycopg's connection pool** (5 per worker × 4 workers = 20)
  with `CONN_MAX_AGE = 0`. Persistent connections under ASGI opened a new
  connection per request context and never closed them: a trial run left 217
  connections open, failed about 40% of Django's requests, and exhausted
  Postgres for the next app.
- **Phoenix now runs on Bandit.** The endpoint names
  `Bandit.PhoenixAdapter` explicitly; the earlier config fell back to Cowboy.
  Phoenix does not migrate at startup and relies on `init.sql`, so its cold
  start does not include migrations.
- **Spring Boot's Hikari pool is 20**, like every other app. It was on
  Hikari's default of 10 before.
- **Rails now migrates at startup.** The earlier image had no `bin/rails`, so
  `db:migrate` never ran and the schema came from `init.sql` alone.
- **Autumn turns on `database.auto_migrate`**, which is off by default in the
  prod profile, so it migrates at startup like the other apps. The compose file
  sets the trusted hosts and allows in-memory sessions, both of which the prod
  profile requires.
- **Loco's health route is `/_health`**, its built-in one. Every other app
  serves `/health`.
- **Memory under load** is sampled about every 2 s (each `docker stats` call
  takes that long), about 30 samples per run.
- **Not comparable with the CI gate baseline above.** That baseline is
  Autumn alone, on a different host, at 20 VUs, outside containers.
