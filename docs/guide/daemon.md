# Daemon mode: `autumn serve`

`autumn serve` runs your app as a long-lived **local daemon** — a personal
service a thin CLI or an agent talks to (the shape of ollama, syncthing, or a
local MCP server). It is the production, non-watch counterpart to
[`autumn dev`](./getting-started.md): no file watching, no hot reload, plus a
managed lifecycle and a no-fuss local database story.

> **Windows:** the daemon lifecycle (`--daemon`, `stop`, `status`, `restart`) is
> **Tier 2 — supported via WSL2**. It is built on Unix domain sockets and POSIX
> signals, so on native Windows these commands fail fast with a message pointing
> at the policy rather than half-working. Foreground `autumn serve` and the whole
> `autumn dev` loop — managed Postgres included — are Tier 1 and run natively.
> See [Platform support](./platform-support.md).

## Quick start

```sh
# A zero-dependency daemon (no database):
autumn new mytool --daemon
cd mytool
autumn serve --daemon      # build + start in the background
autumn serve status        # running (pid 4242) on unix:/run/user/1000/autumn/mytool/serve.sock
autumn serve stop          # graceful drain, then remove socket/pidfile
```

Time from `autumn new --daemon` to a running, reachable daemon is well under
two minutes with **zero externally-installed dependencies**.

## Commands

| Command | Effect |
| --- | --- |
| `autumn serve` | Build and run in the foreground (Ctrl-C drains and stops). |
| `autumn serve --release` | Build an optimized release binary first. |
| `autumn serve --daemon` | Build and run detached in the background. |
| `autumn serve status` | `running (pid, address)` (exit 0) or `stopped` (exit 3). |
| `autumn serve stop` | `SIGTERM` (graceful drain), then `SIGKILL` on timeout. |
| `autumn serve restart` | `stop` (if running) then `--daemon`. |

A second `--daemon` start is **rejected** with a clear message rather than
double-binding, guarded by a PID lockfile.

## Transport and discovery

The daemon binds a **Unix domain socket** (mode `0600`), never a public
interface. A client discovers the address from a small TOML file written next
to the socket:

```toml
# <runtime-dir>/<project>/serve.addr
pid = 4242
transport = "unix"
address = "/run/user/1000/autumn/mytool/serve.sock"
started_at = 1750000000
managed_pg = false
```

To bind a socket from a plain `.run()` app (without the CLI), set
`server.unix_socket` in `autumn.toml` or `AUTUMN_SERVER__UNIX_SOCKET`. Leave it
unset to bind TCP on `server.host:server.port` (default `127.0.0.1:3000`).

## Where state lives

PID lockfile, socket, address file, and logs live under platform-appropriate
directories — XDG (`$XDG_RUNTIME_DIR`, `$XDG_DATA_HOME`, `$XDG_STATE_HOME`) on
Linux, `~/Library/Application Support` on macOS, `%APPDATA%`/`%LOCALAPPDATA%` on
Windows — never the current directory, `/tmp`, or `/etc`. Set
`AUTUMN_RUNTIME_DIR` to override the base (used in tests).

## Databases

### DB-optional by default

A model-free app boots with no `DATABASE_URL` and no Postgres present. The
`autumn new --daemon` starter goes further: it builds with **no database at
all** (drops the `db` feature and the migration wiring), so the binary links no
Postgres client. This is the recommended shape for a local-first tool that
doesn't need persistence.

### Managed local Postgres (opt-in)

For apps that use `#[model]` / `#[repository]`, `autumn new --bundled-pg`
scaffolds a daemon that provisions and supervises a **local Postgres** in the
app's data dir. It wires a `ManagedPostgresPoolProvider` through the existing
[pluggable pool provider](./custom-subsystems.md) — there are no changes to the
query path — and ties the cluster's lifecycle to the daemon via an
`on_shutdown` hook:

```rust
let pg = autumn_web::managed_pg::ManagedPostgresPoolProvider::new();
let pg_shutdown = pg.clone();
autumn_web::app()
    .with_pool_provider(pg)
    .on_shutdown(move || {
        let pg = pg_shutdown.clone();
        async move { pg.stop().await; }
    })
    // ... routes, migrations ...
    .run()
    .await;
```

First-run provisioning (`initdb`) is idempotent and bounded; a cluster that
fails to start surfaces a clear diagnostic instead of hanging.

Two build modes select where the Postgres binaries come from:

- **`managed-pg`** — binaries are **downloaded on first run** (network
  required once), then cached in the data dir.
- **`managed-pg-bundled`** — binaries are **embedded in the app executable** at
  build time, so the end user installs nothing.

#### Bundled-binary caveats

- **Per target.** The embedded binaries match the build target; there is no
  trivial cross-compile.
- **Size.** The executable grows by tens of MB up to ~150MB.
- **Still a child process.** "Bundled" means the binaries ride along in the
  executable and are extracted on first run; Postgres still runs as a
  supervised `postgres` child with an on-disk data dir — it is **not** linked
  in-process like SQLite.

## Database backups

`autumn db backup` and `autumn db restore` capture the databases your app
actually uses — on Postgres as logical dumps, on SQLite as a file
snapshot. They resolve the connection URL(s) through the **same**
path as `autumn migrate` and the other `autumn db` commands — control plus every
configured shard, under the active profile/`.env` overlay — so a backup captures
exactly what the running app reads. On a managed-Postgres daemon the bundled
`pg_dump`/`pg_restore` are used automatically, so there are no external client
tools to install.

On the [SQLite tier](./sqlite-in-production.md) neither tool is involved at all:
a `sqlite://` target is captured with SQLite's own `VACUUM INTO` (one
transactional statement, safe against a live app) and the artifact is a
`control.sqlite` database file. Everything below — the run directory, `--keep`,
`--upload`, `restore` — works the same; only `--format`, which grades Postgres
artifacts, does not apply.

```sh
# Back up control + every shard into ./backups/<profile>/<timestamp>/
autumn db backup

# Compressed by default (pg_dump custom format). Plain SQL instead:
autumn db backup --format plain

# Only the control database, or a single shard:
autumn db backup --control-only
autumn db backup --shard us_east

# Keep only the newest 7 runs, pruning older ones after a successful backup:
autumn db backup --keep 7 --dir /var/backups/myapp
```

Each run writes a self-describing directory containing a `manifest.json` plus
one artifact per database. Every artifact's integrity is verified (custom dumps
via `pg_restore --list`, plain dumps via `pg_dump`'s completion marker) **before**
the run is reported successful; a partial or failed run is removed and never
counted toward `--keep` retention.

If `pg_dump`/`pg_restore` are not on `PATH` (and you are not on a managed-Postgres
app that bundles them), install the PostgreSQL client tools or point
`AUTUMN_PG_BIN_DIR` at their `bin` directory. `autumn doctor` warns when they are
missing.

> **Copying a backup to staging or a laptop?** Run it through
> [`autumn db scrub`](data-scrubbing.md) first — it anonymizes every
> PII-classified column and refuses (rather than silently passing data through)
> when a column has not been classified at all.

### Scheduling

**cron** — a nightly backup with 7-day retention:

```cron
# m h dom mon dow  command
0 2 * * *  cd /srv/myapp && AUTUMN_ENV=prod autumn db backup --keep 7 --dir /var/backups/myapp
```

**systemd timer** — the same schedule as a service + timer pair:

```ini
# /etc/systemd/system/myapp-backup.service
[Unit]
Description=Nightly Autumn database backup

[Service]
Type=oneshot
WorkingDirectory=/srv/myapp
Environment=AUTUMN_ENV=prod
ExecStart=/usr/local/bin/autumn db backup --keep 7 --dir /var/backups/myapp
```

```ini
# /etc/systemd/system/myapp-backup.timer
[Unit]
Description=Run the Autumn database backup nightly

[Timer]
OnCalendar=*-*-* 02:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

```sh
systemctl enable --now myapp-backup.timer   # arm it
systemctl list-timers myapp-backup.timer     # confirm the next run
```

### Restore drill

Rehearse recovery before you need it. `restore` is gated by the **same
production guard** as `autumn db drop`: against a `prod` (or other non-`dev`/`test`)
profile it refuses unless you pass `--force`, and it verifies every artifact's
integrity before touching any database.

```sh
# Restore the whole run (control + shards) — safe on a dev/test profile:
autumn db restore ./backups/dev/20260710T020000Z

# Restore just one shard from a run:
autumn db restore ./backups/dev/20260710T020000Z --shard us_east

# Against production you must opt in explicitly (this overwrites data):
AUTUMN_ENV=prod autumn db restore /var/backups/myapp/prod/20260710T020000Z --force
```

A good drill: restore the latest backup into a scratch database, run
`autumn doctor` / your smoke tests against it, then discard it. Confirming a
backup restores cleanly is the only way to know it is real.

### Offsite backups

A backup on the same disk as the database is not disaster recovery: one failed
drive takes the app and every backup with it. Point `autumn db backup` at an
**S3-compatible** offsite destination (AWS S3, MinIO, Cloudflare R2, Backblaze
B2, Garage) and it uploads each completed run **after** local integrity
verification passes, then **verifies every remote object matches the local
file** before reporting success. If the local backup succeeds but the upload
fails, the command says so unambiguously and exits non-zero — the local artifact
is left intact.

Configure the destination in `autumn.toml` under `[backup.offsite]`. Credentials
are supplied by **env-var indirection**: config names the environment variables
the access key / secret are read from; the secret values never live in config,
argv, logs, or error messages.

```toml
[backup.offsite]
# Upload after every `autumn db backup`, without needing --upload:
auto_upload = true
# Key prefix inside the bucket (optional; objects are keyed
# {prefix}/{profile}/{timestamp}/{file}):
prefix = "db"
# Independent remote retention: keep the newest N uploaded runs per profile,
# pruning older ones only after a verified upload (never the just-uploaded run):
keep = 30

[backup.offsite.s3]
bucket   = "myapp-db-offsite"
region   = "auto"                       # R2 uses "auto"; MinIO ignores it
endpoint = "https://<accountid>.r2.cloudflarestorage.com"
force_path_style = true                 # required by MinIO / R2 / most self-hosted
# Credentials by env-var indirection — names, not values:
access_key_id_env     = "AUTUMN_OFFSITE_ACCESS_KEY_ID"
secret_access_key_env = "AUTUMN_OFFSITE_SECRET_ACCESS_KEY"
```

Every setting also has an `AUTUMN_BACKUP__OFFSITE__*` environment override (e.g.
`AUTUMN_BACKUP__OFFSITE__S3__BUCKET`) and honors profile overlays
(`[profile.prod.backup.offsite]`), matching the rest of Autumn's config.

By default the offsite destination must be **distinct** from the app's
user-facing `[storage.s3]` bucket. Pointing both at the same bucket+endpoint
requires the explicit opt-in `allow_shared_bucket = true`.

```sh
# Export the credentials the config names, then upload with the backup:
export AUTUMN_OFFSITE_ACCESS_KEY_ID=...          # never committed
export AUTUMN_OFFSITE_SECRET_ACCESS_KEY=...
autumn db backup --upload --keep 7 --dir /var/backups/myapp

# List what's offsite (timestamp, size, files) for the active profile:
autumn db offsite list
```

`autumn doctor` includes an `offsite_backup` check that flags an unset bucket, a
shared bucket without opt-in, or credentials that are not ready — without ever
printing a credential value.

**Scheduling.** Extend the cron / systemd recipe above with `--upload` (or set
`auto_upload = true` and drop the flag):

```cron
0 2 * * *  cd /srv/myapp && AUTUMN_ENV=prod autumn db backup --upload --keep 7 --dir /var/backups/myapp
```

```ini
# In the systemd unit's [Service] section, load the offsite credentials from a
# root-only env file and add --upload to the backup command:
EnvironmentFile=/etc/myapp/offsite.env
ExecStart=/usr/local/bin/autumn db backup --upload --keep 7 --dir /var/backups/myapp
```

**Restore-from-offsite drill.** List the offsite runs, then restore one directly
— it downloads the run to a temp directory and applies the **same** integrity
verification and production `--force` guard as a local restore:

```sh
# See what's available offsite:
autumn db offsite list

# Restore the newest offsite run for a profile (into a scratch/dev DB):
autumn db restore offsite:dev/latest

# Or an explicit run, into production (this overwrites data):
AUTUMN_ENV=prod autumn db restore offsite:prod/20260710T020000Z --force
```

**Encryption posture.** Transfers use **TLS in transit** (HTTPS to the S3
endpoint). At rest, objects are protected by the **provider's server-side
encryption** (enable SSE / bucket default encryption on your bucket). Uploads
send `x-amz-checksum-sha256` so the endpoint validates payload integrity
server-side. **Client-side backup encryption** (encrypting the artifact with a
framework-managed key before it leaves the host) is a named follow-up — see the
`autumn/src/encryption.rs` AES-256-GCM envelope — and is **not** applied by this
slice.

## Out of scope

SQLite as an app backend, in-process Postgres, and system-service installation
(systemd unit / launchd plist / Windows Service) are intentionally not part of
daemon mode — see issue #1119 for rationale.
