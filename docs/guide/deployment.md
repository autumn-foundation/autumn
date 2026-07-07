# Deploying an Autumn App

This guide walks you from a fresh `autumn new` project to a production-shaped
container running against a real Postgres database. Every command is verbatim;
no file editing is required to reach a running container.

Target time: **under 10 minutes** on a machine with Docker and a working
internet connection.

> **This promise is machine-verified, not aspirational.** The
> [`release-image-boot`](../../.github/workflows/release-image-boot.yml) CI gate
> scaffolds a fresh project, runs every command on this page (`autumn new` →
> `autumn release init --force` → `docker build` → one-shot `autumn migrate` →
> boot), and fails the build unless the container answers `GET /health` **and**
> `GET /actuator/health` with `200` within the documented startup budget. It
> covers both the bare `release init` image and the `--target docker-compose`
> stack, so the deployment scaffold can never silently rot — a base-image bump,
> a missing system lib, or an asset-path drift is caught in CI, not by a user's
> first failed production deploy. See
> [`scripts/check-release-image-boot.sh`](../../scripts/check-release-image-boot.sh)
> for the build-and-boot harness.

---

## Prerequisites

- **Rust 1.88.0+** with `cargo`
- **Docker** (or Docker Desktop) — `docker --version`
- **PostgreSQL** accessible at a connection string you control (local or remote)
- The `autumn` CLI - `cargo install autumn-cli --version 0.6.0`

---

## Step 1 — Create the project

```bash
autumn new myapp
cd myapp
```

This scaffolds a working Autumn application with a dev-oriented `Dockerfile`.

---

## Step 2 — Generate production deployment files

```bash
autumn release init --force
```

`--force` is required because `autumn new` already wrote a basic `Dockerfile`
and `.dockerignore`. The `--force` flag replaces them with the production-grade
versions.

The command emits three files at the project root:

| File | Purpose |
|---|---|
| `Dockerfile` | Multi-stage image: cargo-chef dep cache → release binary → debian-slim runtime |
| `.dockerignore` | Keeps `target/`, `.git/`, `node_modules/`, `dist/` out of the build context |
| `autumn.production.toml.example` | Production config template with placeholder values — no real secrets |

> **What changed in the Dockerfile?**
> The production Dockerfile adds cargo-chef dependency caching (so rebuilds only
> recompile what changed), installs `libpq`, `tini`, and `ca-certificates` in the
> slim runtime, copies compiled Tailwind assets from `static/`, leaves
> migrations to an explicit primary-role job, and wires the `/health` endpoint as the container
> `HEALTHCHECK`.

---

## Step 3 — Build the image

```bash
docker build -t myapp .
```

The first build downloads Rust crates and the Tailwind binary; subsequent builds
are fast because cargo-chef caches the dependency layer separately from your
application code.

Expected final output:

```
[...]
 => CACHED [runtime 2/7] RUN apt-get update ...
 => [runtime 7/7] COPY --from=builder /app/autumn.production.toml.example /app/autumn.toml
 => exporting to image
Successfully built <sha>
Successfully tagged myapp:latest
```

---

## Step 4 — Migrate, Then Run the Container

Provide your primary/write Postgres connection string as
`AUTUMN_DATABASE__PRIMARY_URL`. Run migrations once against that primary role
before starting web replicas:

```bash
AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@host:5432/myapp_prod" autumn migrate
```

Then start the web container:

```bash
docker run --rm \
  -p 3000:3000 \
  -e AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@host:5432/myapp_prod" \
  myapp
```

You should see something like:

```
INFO autumn: Listening addr=0.0.0.0:3000
```

Visit [http://localhost:3000/health](http://localhost:3000/health) — a healthy
response looks like:

```json
{ "status": "ok", "version": "0.6.0" }
```

> **Migration failure stops the rollout.** If the primary URL is wrong or the
> database is unreachable, `autumn migrate` exits non-zero and you do not roll
> the web tier. Fix the connection string and rerun the one-shot job.

---

## How the production image works

```
rust:1.88.0-bookworm (chef stage)
  └─ cargo chef prepare          # snapshot dependency graph
       └─ cargo chef cook        # build all dependencies (cached layer)
            └─ autumn build --embed         # fingerprint + embed assets (embed-assets feature)
                 └─ debian:bookworm-slim (runtime stage)
                       libpq5, tini, ca-certificates, curl
                       /usr/local/bin/myapp     ← your binary (assets + locales embedded)
                       /app/migrations/         ← SQL migration files (one-shot migrate job)
                       /app/autumn.toml         ← production config (host=0.0.0.0)

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/myapp"]
```

> Projects without the `embed-assets` feature instead build with `cargo build
> --release` and stage `/app/static/` — see [Single-binary deploys](#single-binary-deploys-embedded-assets).

Key design decisions:

- **cargo-chef** separates the dependency build layer from your code. Changing a
  handler reuses cached dependencies; only your crate recompiles.
- **tini** is the PID 1 init process. It reaps zombie processes and forwards
  signals (SIGTERM, SIGINT) so the server shuts down gracefully.
- **Explicit migration ownership** -- migrations run once through
  `AUTUMN_DATABASE__PRIMARY_URL=... autumn migrate` before web replicas roll.
  The web image starts only the server, so replicas do not race schema changes.
- **`autumn.production.toml.example` is copied as `/app/autumn.toml`** so the
  binary binds to `0.0.0.0` (all interfaces) instead of the dev default
  `127.0.0.1`. Override any value at runtime via `AUTUMN_*` environment
  variables (see the [config reference](getting-started.md#configuration)).

---

## Single-binary deploys (embedded assets)

Autumn's design pillar is **single binary deployment**: copy one file, run it, no
sidecar directories. The generated release image delivers on this by **embedding**
the app's `static/` tree (CSS/JS/fonts **and** the fingerprint manifest) and its
i18n `i18n/` locale bundles into the binary at compile time — the same way Diesel
migrations are embedded with `embed_migrations!`. With assets embedded:

- `scp ./myapp host && ./myapp` serves styled, localized pages from an empty
  directory — every referenced asset returns `200`, no `static/`/`i18n/` to stage.
- `asset_url()` resolves against the embedded manifest (no disk read). The manifest
  and the files are baked from the **same** build, so fingerprint-vs-manifest drift
  is impossible.

### How it works

Embedding is an **opt-in, release-time** concern (a Cargo feature). In development
— or whenever the feature is off — the app serves from disk so CSS/JS/translation
hot-reload is unaffected.

Generated apps are wired for it out of the box:

```rust
// src/main.rs (generated)
#[cfg(feature = "embed-assets")]
static EMBEDDED_STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();

#[autumn_web::main]
async fn main() {
    let app = autumn_web::app().routes(routes![/* … */]).migrations(MIGRATIONS);

    #[cfg(feature = "embed-assets")]
    let app = app.embedded_static(&EMBEDDED_STATIC);

    app.run().await;
}
```

```toml
# Cargo.toml (generated)
[features]
embed-assets = ["autumn-web/embed-assets"]
```

i18n apps additionally embed locales via `embed_locales!()` /
`.embedded_locales(&EMBEDDED_LOCALES)`.

### Building

```bash
autumn build --embed
```

This compiles your build scripts (e.g. Tailwind), fingerprints `static/`, then
recompiles with the `embed-assets` feature so the manifest and assets are baked in.

`autumn release init` detects the `embed-assets` feature in your `Cargo.toml`: when
present it emits a release `Dockerfile` that runs `autumn build --embed` and **does
not** `COPY static`/`i18n` into the runtime image (only `migrations/` is staged, for
the one-shot `autumn migrate` job). Projects without the feature get the disk-based
build (`cargo build --release` + `COPY static`) so their Docker builds keep working.

> Adding embedding to an existing app: add the `[features]` block above, wire
> `.embedded_static()` (and `.embedded_locales()` if you use i18n) behind
> `#[cfg(feature = "embed-assets")]`, then re-run `autumn release init --force` and
> build with `autumn build --embed`.

---

## Customising the production config

`autumn.production.toml.example` is the starting point for production config.
It is already used by the container (copied as `/app/autumn.toml` at build time).

To change log format, pool size, or health path, edit
`autumn.production.toml.example` before building:

```toml
# autumn.production.toml.example (excerpt)
[server]
host = "0.0.0.0"
port = 3000

[log]
level = "info"
format = "Json"        # structured JSON for log aggregators

[database]
primary_url = "postgres://user:CHANGE_ME@localhost:5432/myapp_prod"
# replica_url = "postgres://user:CHANGE_ME@replica:5432/myapp_prod"
pool_size = 10
replica_fallback = "fail_readiness"
auto_migrate_in_production = false
```

Sensitive values (database password, SMTP credentials) should **never** be in
this file. Pass them as environment variables at runtime:

```bash
-e AUTUMN_DATABASE__PRIMARY_URL="postgres://user:realpass@host:5432/myapp_prod"
-e AUTUMN_LOG__LEVEL=debug
```

`AUTUMN_*` environment variables override `autumn.toml` at the highest
priority layer — see the
[config reference](getting-started.md#environment-variable-overrides).

## Trusted hosts (Host-header allow-list)

Autumn supports a host allow-list to prevent host-header rebinding and cache-poisoning style attacks.

```toml
[security.trusted_hosts]
hosts = ["app.example.com", ".example.com"]
```

- `app.example.com` matches exactly that hostname.
- `.example.com` matches both `example.com` and any subdomain like `api.example.com`.
- `hosts = ["*"]` disables host filtering (escape hatch; not recommended for production).

In `prod`/`production` profile, startup fails when `security.trusted_hosts.hosts` is empty.
Health/probe routes (`/actuator/health`, `/live`, `/ready`, `/startup`) intentionally bypass host checks so orchestration probes remain reliable.

### Runnable repro

```bash
# Expected: 400 + application/problem+json
curl -i http://localhost:3000/ -H 'Host: evil.example'

# Expected: normal route response
curl -i http://localhost:3000/ -H 'Host: app.example.com'
```

---

## Deploy to fly.io

Scaffold a `fly.toml` alongside the production Dockerfile:

```bash
autumn release init --force --target fly
```

The generated `fly.toml` includes four first-class integrations:

| Feature | What it does |
|---|---|
| `/live` + `/ready` checks | Fly uses `/live` to decide machine restarts; `/ready` to gate traffic routing. Autumn flips `/ready` to 503 at drain start so Fly deregisters before the listener closes. |
| `kill_timeout = 45` | Fly waits 45 s after SIGTERM before SIGKILL — `prestop_grace_secs (5) + shutdown_timeout_secs (30) + 10 s buffer` for the process to log and exit cleanly. Value is an integer (seconds); Fly does not accept a string like `"45s"`. |
| `[metrics]` → `/actuator/prometheus` | Fly scrapes Autumn's Prometheus text endpoint and surfaces it in the dashboard. No extra agent needed. Controlled by `actuator.prometheus` (default on) and independent of `actuator.sensitive` — see [Prometheus metrics for platform scraping](#prometheus-metrics-for-platform-scraping). |
| `[deploy]` `release_command` (opt-in) | When uncommented, migrations run in a one-shot machine before new app machines start; a failed migration aborts the deploy before any traffic-serving machine is replaced. |

Deploy:

```bash
fly launch --no-deploy          # creates the app on fly.io
fly secrets set AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@host:5432/myapp_prod"
fly secrets set AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
fly deploy
```

**Using a database?** Uncomment the `release_command` line in `fly.toml` before
the first `fly deploy`:

```toml
[deploy]
  release_command = "autumn migrate"
```

With it enabled, Fly runs `autumn migrate` in a temporary machine before new app
machines start. A failed migration aborts the deploy before any traffic-serving
machine is replaced — keeping the old version live. The line is commented out by
default because `autumn migrate` exits non-zero when no database URL is set,
which would fail the first deploy of a database-free app.

If you add a read replica, set `AUTUMN_DATABASE__REPLICA_URL` as a secret and
Autumn gates `/ready` until the replica has replayed the latest migration.

---

## Prometheus metrics for platform scraping

Autumn exposes a Prometheus text endpoint at `/actuator/prometheus`. It is
controlled by `actuator.prometheus` (default **`true`**) and is **independent of
`actuator.sensitive`**. That separation is the whole point: a production app can
let Fly.io (or any scraper) collect metrics while keeping `actuator.sensitive =
false`, so `/actuator/env`, `/actuator/configprops`, `/actuator/loggers`,
`/actuator/tasks`, `/actuator/jobs`, and the actuator task UI stay off the
public surface.

```toml
# autumn.toml — metrics on, sensitive surfaces off (the safe production shape)
[actuator]
sensitive  = false   # env/configprops/loggers/tasks/jobs NOT mounted
prometheus = true    # /actuator/prometheus still scrapeable
```

To remove the scrape endpoint entirely (it then returns `404`), set
`prometheus = false` — either in `autumn.toml` or via the environment override
`AUTUMN_ACTUATOR__PROMETHEUS=false` (the whole `[actuator]` section follows the
standard `AUTUMN_SECTION__FIELD` convention). Regression tests assert both
directions — the endpoint is present under the non-sensitive config and absent
when export is disabled.

The generated `fly.toml` wires Fly's `[metrics]` block to this endpoint:

```toml
[metrics]
  port = 3000
  path = "/actuator/prometheus"
```

### Keeping metrics off the public HTTP service

`/actuator/prometheus` carries operational counters, not secrets, but you may
still want it unreachable from public traffic. The Fly-native way is to scrape a
**separate, non-public port** rather than the port behind `[http_service]`.
Bind a second internal listener and point `[metrics]` at it:

```toml
[metrics]
  port = 9091                       # internal-only; no [http_service] on it
  path = "/actuator/prometheus"
```

Fly scrapes `[metrics]` over the private 6PN network, so a port that has no
`[http_service]` / `force_https` mapping is reachable by the Fly metrics
collector but not by the public internet. Front the public app on its own port
and reserve the metrics port for scraping. (If you only run a single listener,
gate access at the edge or accept that the counters are publicly readable —
they contain no credentials.)

### OTLP tracing and Prometheus are separate telemetry paths

Enabling OTLP tracing (`telemetry.enabled = true` + `telemetry.otlp_endpoint`)
initializes **span export to an OTLP collector**. It does **not** feed
OpenTelemetry metrics into `/actuator/prometheus`. The Prometheus endpoint is
backed by Autumn's in-process request `MetricsCollector` snapshot plus any
registered [`MetricsSource`](metrics-sources.md) families — it is a distinct
pipeline from the OTLP trace exporter. Treat them as two independent channels:

- **Tracing** → OTLP collector (Jaeger, Tempo, Honeycomb, …) via the OTLP path.
- **Metrics** → `/actuator/prometheus` scraped by Fly `[metrics]` or Prometheus.

Turning on one does not populate the other. Bridging OTLP metrics into the
Prometheus scrape would require an explicit metrics exporter/bridge, which
Autumn does not add implicitly.

---

## Run locally with Docker Compose (app + Postgres)

Scaffold a `docker-compose.yml` with an app service, a one-shot migration job,
and a managed Postgres:

```bash
autumn release init --force --target docker-compose
```

Start both services:

```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
docker compose up --build
```

The `docker-compose.yml` sets `AUTUMN_DATABASE__PRIMARY_URL` pointing at the
`db` service, waits for Postgres to pass its healthcheck, runs `autumn migrate`
once, passes `AUTUMN_SECURITY__SIGNING_SECRET` into the app service, and starts
the app only after that job exits successfully. No manual Postgres setup is
needed.

To reset the database:

```bash
docker compose down -v   # removes the postgres_data volume
docker compose up --build
```

---

## Overwriting specific files

By default `autumn release init` refuses to overwrite existing files:

```
Error: 'Dockerfile' already exists — run with --force to overwrite
```

Use `--force` to regenerate everything, or delete individual files first if you
only want to regenerate a subset.

---

## Signing secret (required before production boot)

Before the server will bind in the `prod` profile, you must set a stable signing
secret. It protects sessions, CSRF tokens, and signed storage URLs:

```bash
# Generate once, store securely (e.g. Fly secrets, AWS Secrets Manager, …)
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
```

**Smoke-gate check** — the app must refuse to boot _without_ the secret:

```bash
docker run --rm \
  -e AUTUMN_ENV=prod \
  -e AUTUMN_DATABASE__PRIMARY_URL=... \
  myapp 2>&1 | grep -i "signing secret"
# Expected: "Invalid signing secret configuration: signing secret is required in production"
```

And must start successfully _with_ a valid secret:

```bash
docker run --rm -p 3000:3000 \
  -e AUTUMN_ENV=prod \
  -e AUTUMN_DATABASE__PRIMARY_URL=... \
  -e AUTUMN_SECURITY__SIGNING_SECRET="$AUTUMN_SECURITY__SIGNING_SECRET" \
  myapp
```

See [docs/guide/signing-secrets.md](signing-secrets.md) for rotation instructions
and the full multi-replica setup guide.

---

## Multi-replica setup

To run multiple replicas behind a load balancer, every replica **must use the
same signing secret and the same Redis session backend**. A session established
on replica A must be readable by replica B.

```bash
SECRET=$(openssl rand -hex 32)

# Replica 1
docker run --rm -p 3000:3000 \
  -e AUTUMN_ENV=prod \
  -e AUTUMN_DATABASE__PRIMARY_URL=postgres://... \
  -e AUTUMN_SECURITY__SIGNING_SECRET="$SECRET" \
  -e AUTUMN_SESSION__BACKEND=redis \
  -e AUTUMN_SESSION__REDIS__URL=redis://redis:6379 \
  myapp &

# Replica 2 — identical secret, primary URL, and Redis URL
docker run --rm -p 3001:3000 \
  -e AUTUMN_ENV=prod \
  -e AUTUMN_DATABASE__PRIMARY_URL=postgres://... \
  -e AUTUMN_SECURITY__SIGNING_SECRET="$SECRET" \
  -e AUTUMN_SESSION__BACKEND=redis \
  -e AUTUMN_SESSION__REDIS__URL=redis://redis:6379 \
  myapp &
```

With this setup:

- A user who logs in via replica 1 is authenticated on replica 2 without
  re-logging in (sessions live in Redis, signed with the shared secret).
- Signed blob URLs generated on replica 1 are served correctly by replica 2
  (same HMAC key).
- CSRF tokens validate regardless of which replica handles the form submission.

### Global rate limiting

By default the rate limiter keeps per-IP token buckets **in memory per replica**.
A 3-replica deployment therefore permits up to 3× the configured rate — enough
to undermine the protection intended by your `requests_per_second` setting.

To enforce the budget globally, point the rate limiter at the same Redis instance
as your session store:

```toml
[security.rate_limit]
enabled = true
requests_per_second = 10.0
burst = 20
backend = "redis"
on_backend_failure = "fail_open"   # or "fail_closed"

[security.rate_limit.redis]
url = "redis://redis:6379"
key_prefix = "myapp:rate_limit"
```

Or with environment variables alongside the session and cache settings:

```bash
AUTUMN_SECURITY__RATE_LIMIT__BACKEND=redis
AUTUMN_SECURITY__RATE_LIMIT__REDIS__URL=redis://redis:6379
```

| Setting | Effect |
|---|---|
| `backend = "memory"` | Default. Each replica enforces the limit independently. |
| `backend = "redis"` | Global enforcement via atomic Lua token-bucket in Redis. |
| `on_backend_failure = "fail_open"` | Requests pass through when Redis is unreachable (default). |
| `on_backend_failure = "fail_closed"` | Requests receive `429` until Redis recovers. |

One `tracing::warn!` is emitted when Redis becomes unavailable and again when it
recovers, so log volume stays low during outages.

---

## Continuous integration

`autumn new` writes `.github/workflows/ci.yml` into every generated project.
The workflow runs automatically on every branch push and pull request, so CI
fires on your first push no matter what the default branch is named:

| Step | Command |
|------|---------|
| Format check | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| Build | `cargo build` |
| Test | `cargo test` |

The Rust toolchain is pinned to the project MSRV (1.88.0+) via
`dtolnay/rust-toolchain@<msrv>` so local and CI toolchains can't drift.

A Postgres 16 service container is provisioned and `DATABASE_URL` is wired in
so DB-dependent tests can opt in. Tests marked `#[ignore]` are skipped in the
default `cargo test` run; pass `-- --ignored` to include them.

### Extending the CI workflow

**Tailwind CSS**: install the Tailwind CLI (`autumn setup --tailwind`) and add a
step before `cargo build` to run it. The generated `build.rs` will auto-detect
it on `PATH` or at `target/autumn/tailwindcss`.

**Coverage**: install `cargo-llvm-cov` (`taiki-e/install-action@cargo-llvm-cov`)
and upload the LCOV report to Codecov. Coverage gating is out of scope for the
generated scaffold but straightforward to add.

**Audit**: `cargo install cargo-audit --locked` then `cargo audit` as a separate
step. Recommended before production deploys.

---

## Next steps

Once the container is running:

- **Monitor**: `autumn monitor --url http://your-host:3000` for a live TUI
  dashboard of metrics, logs, and routes.
- **Scale**: add `min_machines_running = 1` in `fly.toml` to keep a warm
  instance; use `pool_size` in `autumn.production.toml.example` to tune
  database concurrency.
- **Observe**: uncomment the `[telemetry]` block in `autumn.production.toml.example`
  and point it at an OTLP collector for distributed tracing.
- **Harden**: run `autumn doctor --strict` in CI before building the image to
  catch config issues before they reach production.

For a full cloud-native deployment (Kubernetes readiness probes, structured
logging, OTLP tracing), see the [Cloud-Native Guide](cloud-native.md).
