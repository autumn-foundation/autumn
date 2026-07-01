# Autumn Bookmarks Distributed Example

A bookmark manager copied from `bookmarks` as a clean baseline for the future
distributed retrofit. It keeps the same bookmark domain and user-facing surface
today, but it is intentionally separated so we can add distributed plumbing
without contaminating the happy-path example.

This version is the "went viral" sibling: same bookmark domain, but with the
runtime seams pulled into the open instead of hidden behind happy-path defaults.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| **Profiles** | `autumn.toml` + `autumn-dev.toml` + `autumn-docker.toml` | Local dev and Docker deployment use canonical `[database]` primary/replica wiring without touching framework internals |
| **`#[model]`** | `models.rs` | Generates `Bookmark`, `NewBookmark`, `UpdateBookmark` from one struct |
| **Explicit repository** | `repositories.rs` | Routes reads to replica, writes to primary, and defines `/api/bookmarks` handlers by hand |
| **Read-your-own-writes** | `autumn.toml` `[database]` + `repositories.rs` | `read_your_writes = "session"` pins a client's reads to the primary for `pin_after_write_secs` after any write; since this repository predates the `#[repository(...)]` macro it opts in explicitly via the public `read_your_writes::is_pinned`/`mark_write` calls |
| **Partitioned scheduled task** | `tasks.rs` | `#[scheduled(every = "1h")]` link checker partitions work into fixed shards and uses Postgres advisory locks for ownership |
| **Explicit migrator** | `src/bin/migrate.rs` | Runs embedded migrations once against the primary before web replicas start |
| **Compose deployment** | `docker-compose.yml` + `docker/` | Primary + streaming replica + one-shot migrator + 2 web replicas + nginx |
| **Shared signing secret** | `docker-compose.yml` + `autumn-docker.toml` | All replicas share `AUTUMN_SECURITY__SIGNING_SECRET` so sessions and CSRF tokens survive load-balanced requests |
| **Redis session backend** | `autumn-docker.toml` | Sessions stored in Redis so any replica can read a session established by another |
| **Actuator** | Nav bar links | `/actuator/health`, `/actuator/info` auto-mounted |

## Prerequisites

- Rust (edition 2024) if you want to run the app locally
- Docker & Docker Compose for the full distributed stack

## Quick start

### Full distributed stack

From the **workspace root** (`autumn/`):

```bash
# 1. Generate a stable signing secret shared by both web replicas.
#    All replicas must use the same value or session cookies signed by one
#    replica will be rejected by the others.
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"

# 2. Build and start the full stack
docker compose -f examples/bookmarks-distributed/docker-compose.yml up -d --build
```

`docker-compose.yml` passes `AUTUMN_SECURITY__SIGNING_SECRET` from your shell
into every web replica via the `x-bookmarks-base` anchor. Compose will refuse
to start if the variable is unset — that mirrors the hard startup failure you
would see in a production Autumn app.

`autumn-docker.toml` wires three additional things:
- `[database] primary_url` / `replica_url` -- the same first-class topology
  contract used by Autumn itself. The old `[distributed.database]` shape is
  still accepted by the example loader only as a migration bridge.
- `[session] backend = "redis"` — sessions are stored in the shared Redis
  instance so a session established by `bookmarks-1` is readable by
  `bookmarks-2`.
- `[security.signing_secret]` — the runtime secret comes from the env var
  above; the comment reminds you not to hardcode a value in the TOML file.

That stack brings up:

- `postgres-primary` on `localhost:5432`
- `postgres-replica` on `localhost:5433`
- `bookmarks-migrate` as a one-shot embedded-migration runner
- `bookmarks-1`, `bookmarks-2`
- `load-balancer` on <http://localhost:3000>

Generated `static/css/autumn.css` is intentionally ignored. You do not need to
run `autumn setup` just to compile or test the example; the build skips CSS
regeneration when Tailwind is unavailable. Set `AUTUMN_REQUIRE_TAILWIND=true`
when you explicitly want missing or broken Tailwind to fail the build.
The web replicas wait until the standby has replayed the latest Diesel migration
version before they start serving traffic. If the replica is stale, the replica
healthcheck fails and Compose never admits the web tier.

Useful follow-ups:

```bash
# Watch the stack settle
docker compose -f examples/bookmarks-distributed/docker-compose.yml logs -f

# Tear it back down, including Postgres volumes
docker compose -f examples/bookmarks-distributed/docker-compose.yml down -v
```

### Local app against the same primary/replica pair

If you want to run the app directly on your machine but still use the same
database topology:

```bash
# 1. Start only the databases
docker compose -f examples/bookmarks-distributed/docker-compose.yml up -d postgres-primary postgres-replica

# 2. Apply migrations to the primary using the explicit migrator
AUTUMN_PROFILE=dev cargo run -p bookmarks-distributed --bin migrate

# 3. Run the app locally (dev profile auto-detected)
cargo run -p bookmarks-distributed
```

The local app uses `autumn.toml` plus `autumn-dev.toml`, so reads go to
`localhost:5433` and writes go to `localhost:5432`.

If you want to recompile Tailwind locally instead of using the checked-in CSS,
run `cargo run -p autumn-cli -- setup` first.

## Available routes

### HTML (browser)

| Method | Path         | Description                  |
|--------|--------------|------------------------------|
| GET    | `/`          | List all bookmarks           |
| GET    | `/tag/{tag}` | Filter bookmarks by tag      |
| GET    | `/new`       | Add bookmark form            |

### JSON API

These routes are explicit handlers in `repositories.rs` and use the distributed
state seam directly instead of generated CRUD endpoints.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/bookmarks` | List all bookmarks |
| GET | `/api/bookmarks/{id}` | Fetch one bookmark |
| POST | `/api/bookmarks` | Create a bookmark |
| PUT | `/api/bookmarks/{id}` | Update a bookmark |
| DELETE | `/api/bookmarks/{id}` | Delete a bookmark |

### Framework

| Method | Path                     | Description            |
|--------|--------------------------|------------------------|
| GET    | `/actuator/health`       | Health + profile info  |
| GET    | `/actuator/info`         | Build & runtime info   |
| GET    | `/actuator/metrics`      | Request and pool stats |
| GET    | `/health`                | Health check           |
| GET    | `/static/js/htmx.min.js` | Bundled htmx          |
| GET    | `/static/css/autumn.css` | Compiled Tailwind CSS  |

## Try the explicit CRUD API

```bash
# Create
curl -X POST http://localhost:3000/api/bookmarks \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://rust-lang.org","title":"Rust","tag":"lang"}'

# List
curl http://localhost:3000/api/bookmarks

# Update
curl -X PUT http://localhost:3000/api/bookmarks/1 \
  -H 'Content-Type: application/json' \
  -d '{"title":"Rust Lang","tag":"rust","alive":true}'
```

## Verifying cross-replica session consistency

Once the stack is up, you can confirm that a session established on one replica
is honoured by the others — which is the practical test of a shared signing
secret plus a shared session store:

```bash
# 1. Open a session on whichever replica nginx selects first
SESSION=$(curl -s -c /tmp/bm-cookies.txt http://localhost:3000/ > /dev/null && cat /tmp/bm-cookies.txt | grep session | awk '{print $NF}')

# 2. Hit the load balancer several times: nginx round-robins across the two
#    replicas, so at least one request will land on a different process.
for i in 1 2 3 4 5; do curl -s -b /tmp/bm-cookies.txt http://localhost:3000/ | grep -c "bookmarks" ; done
```

Each request should return bookmark data (rather than a redirect to a sign-in
page), regardless of which replica handles it.

## Pain points surfaced on purpose

- The generated repository macro is great for the happy path, but read/write
  split wanted an explicit repository seam almost immediately.
- Scheduled tasks are process-local by default, so distributed safety required
  explicit shard ownership and advisory-lock coordination in application code.
- Splitting reads to the replica for real also means a client can create a
  bookmark and then, on the very next page load, not see it — the classic
  read-your-own-writes gap whenever replication lag (or, as here, a second
  independent database standing in for one) sits between the write and the
  next read. `read_your_writes = "session"` closes this for the common
  create-then-redirect flow, but because this repository predates the
  `#[repository(...)]` macro it doesn't get that wiring automatically —
  `BookmarkRepository::conn()` opts in by hand via the same public
  `is_pinned`/`mark_write` calls the macro emits. Session-mode pinning
  itself needs a configured signing secret to issue its `autumn.ryw` cookie
  (see `docs/guide/signing-secrets.md`) — dev's zero-config ephemeral key is
  deliberately not used for it, so a plain `cargo run` with no
  `AUTUMN_SECURITY__SIGNING_SECRET` set will quietly keep missing the
  just-created bookmark. `docker compose` already requires that variable
  (see below); for a plain `cargo run`, set it yourself to see pinning work.
  Use `#[scheduled]` for light in-process work like this demo; move durable or
  coordinated multi-step work to Harvest.
- Non-dev deployment needed an explicit migration runner because
  `AUTUMN_PROFILE=docker` correctly does not auto-apply migrations.
  Treat that migrator as the shape to copy into a real deployment job before
  any web replica starts.
- The distributed state lives beside Autumn's normal app state instead of inside
  it. That is an escape hatch, but it also shows where a future framework helper
  might reduce ceremony without hiding the runtime truth.
- Signing secrets must be provisioned before `docker compose up`. The compose
  file uses `${AUTUMN_SECURITY__SIGNING_SECRET:?...}` syntax so that Compose
  itself fails loudly if the variable is missing — rather than silently starting
  replicas with per-process ephemeral keys that would cause every session cookie
  to be invalid on any replica other than the one that set it.
