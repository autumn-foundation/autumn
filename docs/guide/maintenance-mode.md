# Maintenance Mode

Maintenance mode lets you take an Autumn application offline in a controlled,
reversible way — without stopping the process or rolling a deploy. It is the
right tool for:

- **Destructive migrations** — schema changes that need write traffic paused
  while the migration runs.
- **Incident response** — stopping user traffic instantly while you investigate
  a data-integrity issue.
- **Planned downtime windows** — a database failover, storage volume swap, or
  dependency upgrade that requires the app to stop accepting writes briefly.

Target time: **under 30 seconds** to enter or exit maintenance on a running app.

---

## How it works

`autumn maintenance on` writes a JSON flag file at `tmp/autumn-maintenance.json`
relative to the current working directory. The running app polls that file
every 500 ms through a background task. When the file appears, every replica
enters maintenance within one poll interval — no process restart, no deploy.
`autumn maintenance off` deletes the file and the app re-opens to traffic within
the same window.

The flag file is intentionally a plain file on disk, not an in-process config
update. This means the CLI (`autumn maintenance`) runs as a **separate process**
alongside the app and needs no IPC or HTTP endpoint to communicate the state
change. Any replica that can see the same working directory (local dev, a
Docker-compose mount, a Fly.io shared volume) reacts in lock-step.

> **"Same working directory" is the limit of that lock-step.** Replicas on
> *different machines* do not see each other's flag file, so the local
> `autumn maintenance` cannot gate a multi-host fleet — it only writes the
> machine it runs on. For hosts managed by `autumn deploy` (`[deploy] host` or
> `[deploy] hosts`), use
> [`autumn deploy maintenance on|off`](#fleet-wide-maintenance-autumn-deploy-maintenance),
> which writes the same flag file to every configured host over SSH. Running the
> local command *on* a deploy-managed host does not help either — it writes a
> path the app no longer reads, silently and with exit `0`. See
> [Where the flag file lives](#where-the-flag-file-lives).

### Where the flag file lives

The default path is `tmp/autumn-maintenance.json`, **relative to the process's
working directory**. Set `AUTUMN_MAINTENANCE_FLAG_FILE` to an absolute path to
put it somewhere else; the app reads that variable both at startup and in the
500 ms poller, so the two can never disagree. An unset or blank value falls back
to the default, so an app that does not set it is unaffected.

Hosts deployed by `autumn deploy` get this automatically. Each slot unit is
written with

```ini
Environment=AUTUMN_MAINTENANCE_FLAG_FILE=/srv/autumn/myapp/shared/autumn-maintenance.json
```

pointing at the per-app `shared/` directory. That matters because a slot unit's
`WorkingDirectory` is the **release** directory, which is new on every deploy: on
the default cwd-relative path a cutover would orphan an active maintenance flag
and silently un-maintain the host, and the blue and green slots could not see
each other's flag at all. The `shared/` directory survives cutovers, rollbacks
and pruning, and both slots read it.

> **On a deploy-managed host, the local `autumn maintenance` command no longer
> does anything.** `autumn maintenance on|off` always writes and removes
> `tmp/autumn-maintenance.json` relative to its own working directory — it is a
> separate process from the app, it does not read the unit's `Environment=`, and
> it has no flag to point it elsewhere. On a host deployed by `autumn deploy`,
> the app reads `{app_dir}/shared/autumn-maintenance.json`, so SSH-ing in and
> running `autumn maintenance on` writes a file nothing reads: the command exits
> `0`, and the host keeps serving traffic normally. There is no error to warn
> you.
>
> **On deploy-managed hosts, use
> [`autumn deploy maintenance on|off`](#fleet-wide-maintenance-autumn-deploy-maintenance)**
> — from your workstation or CI, not from the host. It writes the path the slot
> units actually point at (and, for a host still running a unit deployed before
> this existed, the old release-relative path as well), on every configured host,
> and reports per host what it changed. The local command remains the right tool
> for local development and for replicas that share a working directory
> (docker-compose, a Fly.io volume) — anywhere the app and the CLI see the same
> directory and no unit is overriding the path.

**`autumn deploy status` resolves this per host rather than assuming it.** Its
maintenance column does not read one fixed path: for each host it reads the
**live slot unit** and resolves the flag file that unit makes the app poll —
`Environment=AUTUMN_MAINTENANCE_FLAG_FILE` when present, otherwise the unit's
`WorkingDirectory` plus the relative default above — which is the same rule the
runtime applies. Reading the shared path unconditionally would lie in both
directions: `off` for a maintained host whose unit polls elsewhere, `ON` for a
host still taking traffic.

So a host deployed **before** slot units carried
`AUTUMN_MAINTENANCE_FLAG_FILE` reports its release-local
`{release_dir}/tmp/autumn-maintenance.json` — the truth for that host — and the
row also carries a drift reason saying the app polls a release-local flag rather
than the shared one, whose remedy is to **redeploy that host**. When the live
slot unit cannot be read at all, the cell reads `maintenance ?` — never a
confident `off` — and on a host reported as `deployed` that carries a drift
reason of its own (a host with nothing deployed has no slot unit to read, and is
already reported as such).
The column reports which file the running unit polls and whether that file
exists — it is not a statement about the app's in-memory state, which follows on
the next 500 ms poll.

When maintenance is active, all gated requests receive **503 Service Unavailable**
with `Retry-After: 120`. The app never returns 200 for application routes while
the flag is present.

---

## Quick reference

```bash
# Enter maintenance
autumn maintenance on

# Enter maintenance with a user-visible message
autumn maintenance on --message "Down for scheduled maintenance. Back in 10 minutes."

# Exit maintenance
autumn maintenance off

# Check current status (also surfaced by `autumn doctor`)
autumn doctor

# Same thing, but on every host `autumn deploy` manages (over SSH)
autumn deploy maintenance on --message "Down for scheduled maintenance."
autumn deploy maintenance off
autumn deploy status            # per-host maintenance + readiness columns
```

---

## What passes through during maintenance

The following requests always reach the application regardless of the flag:

| Path prefix | Reason |
|---|---|
| `/actuator/*` | Orchestration health probes (Kubernetes, Fly.io) must keep working so the machine is not killed. |

Everything else is gated — unless you configure explicit exceptions (see below).

---

## CLI options

### `autumn maintenance on`

```
autumn maintenance on [OPTIONS]
```

| Flag | Type | Description |
|---|---|---|
| `--message <MSG>` | string | Message displayed to users in the 503 response. |
| `--allow-ips <CIDR>` | repeatable | One or more IP/CIDR blocks whose traffic passes through unblocked. |
| `--readonly` | flag | Only blocks mutating requests (POST, PUT, PATCH, DELETE). GET, HEAD, and OPTIONS pass through. |
| `--bypass-header <NAME:VALUE>` | string | Requests carrying this exact header name and value bypass maintenance. |

All options are additive — you can combine them in any order.

### `autumn maintenance off`

```
autumn maintenance off
```

Deletes the flag file. If the file is not present (maintenance was not active),
the command prints a warning but exits successfully.

---

## Allow-list options in detail

### `--message`

The message appears in both the HTML and JSON 503 responses. Omit it for a
generic "service unavailable" body, or set it to something actionable:

```bash
autumn maintenance on \
  --message "We are running a planned migration. Back online by 14:30 UTC."
```

### `--allow-ips`

Pass individual IPs or CIDR blocks. Repeat the flag for multiple ranges:

```bash
autumn maintenance on \
  --allow-ips 10.0.0.0/8 \
  --allow-ips 192.168.1.50
```

Traffic from addresses inside any listed range reaches the application normally.
Both IPv4 and IPv6 ranges are accepted. IPv4-mapped IPv6 addresses
(e.g. `::ffff:10.0.0.1`) are matched against the IPv4 block.

Useful when you want your own office or VPN IP to have read access while the
public is locked out.

### `--readonly`

Read-only mode passes GET, HEAD, and OPTIONS through to the application and
returns 503 only for POST, PUT, PATCH, and DELETE:

```bash
autumn maintenance on --readonly \
  --message "We are migrating data. Reads are available; writes are paused."
```

This is ideal when the migration only affects tables that are not read by the UI,
or when you want users to be able to view the site but not submit forms.

### `--bypass-header`

Any request carrying the exact header name and value listed here bypasses
maintenance entirely:

```bash
autumn maintenance on \
  --bypass-header "X-Maintenance-Bypass:my-internal-token"
```

Use this to keep an admin dashboard, a health-check script, or an internal API
consumer working while all other traffic is blocked. Keep the value secret —
it is stored in the flag file on disk.

---

## Response format

The 503 response is content-negotiated based on the `Accept` header.

### HTML (default)

Requests that include `text/html` in `Accept` (a browser, an htmx request)
receive an HTML page:

```html
<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Maintenance</title></head>
<body>
  <h1>Service Unavailable</h1>
  <p>We are running a planned migration. Back online by 14:30 UTC.</p>
  <p>Please try again shortly.</p>
</body>
</html>
```

### JSON (APIs)

Requests that prefer `application/json` (an API client, a mobile app) receive
an [RFC 7807 Problem Details](https://www.rfc-editor.org/rfc/rfc7807) response:

```json
{
  "type": "https://docs.autumn-framework.dev/errors/maintenance",
  "title": "Service Unavailable",
  "status": 503,
  "detail": "We are running a planned migration. Back online by 14:30 UTC."
}
```

Both responses include `Retry-After: 120`, which tells HTTP clients and proxies
to wait at least two minutes before retrying.

---

## Middleware registration

The maintenance middleware ships as `autumn_web::middleware::MaintenanceLayer`.
`autumn_web::app()` registers it automatically — you do not need to add it
manually. The middleware slot sits between the load-shedder and the authentication
layers so unauthenticated traffic is still gated during maintenance.

If you are building a custom app without `autumn_web::app()`, register it
explicitly:

```rust,no_run
use autumn_web::middleware::{MaintenanceLayer, MaintenanceState};

let state = MaintenanceState::default();

let app = Router::new()
    .route("/", get(index))
    .layer(MaintenanceLayer::new(state.clone()));
```

To override the health prefix (default `/actuator`):

```rust,no_run
MaintenanceLayer::new(state)
    .with_health_prefix("/health")
```

---

## `autumn migrate --with-maintenance`

For migrations that need write traffic stopped during the run, pass
`--with-maintenance` to `autumn migrate`. The CLI will:

1. Write the maintenance flag file before running the migration.
2. Run migrations.
3. Remove the flag file on success, reopening traffic.
4. **Leave the flag file in place on failure** and print guidance. You must
   diagnose the failed migration and run `autumn maintenance off` yourself when
   it is safe to reopen traffic.

```bash
autumn migrate --with-maintenance
```

The flag file message is set automatically to
`"Database migration in progress"`. If you want a custom message for the
public-facing 503, run `autumn maintenance on --message "..."` yourself before
calling `autumn migrate` without `--with-maintenance`.

---

## Fleet-wide maintenance (`autumn deploy maintenance`)

The commands above write **this machine's** working directory. For hosts you
deploy to with [`autumn deploy`](deployment.md), use the deploy-scoped verb
instead — it fans the same flag file out to every configured host over SSH:

```bash
autumn deploy maintenance on --message "Upgrading database schema"
autumn deploy maintenance on --readonly --allow-ips 10.0.0.0/8
autumn deploy maintenance on --bypass-header X-Dev-Bypass:mytoken
autumn deploy maintenance off
```

It applies to the deploy-configured target(s) — `[deploy] host` (one server) or
`[deploy] hosts` (a fleet) — and takes exactly the same flags as
`autumn maintenance on`, because both write the same wire format. Running apps
react within the same 500 ms poll interval; nothing is restarted and no release
is deployed.

Three behaviours worth knowing:

- **Best-effort, aggregate, never reversed.** Every host is attempted, a per-host
  table names what changed, and the command exits non-zero if any host failed.
  The hosts that *did* change are deliberately **not** rolled back — that would
  push users straight back into the window you are closing — so the summary names
  them and the decision is yours.
- **It writes the shared flag path** (`{app_dir}/shared/autumn-maintenance.json`)
  that deploy-managed slot units point `AUTUMN_MAINTENANCE_FLAG_FILE` at. For a
  host still running a unit deployed before that existed, `on` also writes the
  old release-relative path; if the host's `current` release cannot be resolved,
  its row says the shared flag only was written.
- **It does not drain a host from your load balancer.** `/ready` stays `200`
  while maintenance is on — by design, since gating it would eject every host
  from the pool the moment maintenance was enabled. A maintained host keeps
  taking traffic and answers it with `503` + `Retry-After`. Drain at the load
  balancer if you need a host out of rotation.

`autumn deploy status` reports the maintenance flag per host, in its own column
next to readiness, precisely because the two are orthogonal. The cell is
three-valued — `maintenance ON` / `maintenance off` / `maintenance ?` — and
reports the flag file that host's *running* slot unit polls; see
[Where the flag file lives](#where-the-flag-file-lives). In `--json` the same
field is `true` / `false` / `null`, `null` being the unproven case. See the
[fleet deploys guide](fleet-deploys.md#runbook-a-fleet-wide-maintenance-window)
for the full window runbook.

---

## `autumn doctor` integration

`autumn doctor` includes a maintenance-mode check. It reports:

- **PASS** — no flag file present; maintenance is not active.
- **WARN** — flag file found; maintenance is active. The check prints the
  message from the flag file so it is visible in the `doctor` report.

`WARN` is intentional — `autumn doctor` stays green during a planned maintenance
window so CI health scripts can still pass.

---

## `autumn dev` banner

When you start the development server with `autumn dev` while the flag file
exists, the CLI prints a banner before the server output:

```
  ⚠️  MAINTENANCE MODE IS ON
     Message: Database migration in progress
     Run `autumn maintenance off` to disable.
```

This prevents accidentally leaving maintenance on after a local test.

---

## Runbook: destructive migration window

This is the full sequence for a migration that **drops or renames a column** (or
makes any other change that would produce errors if write traffic continued
during the migration).

### Before you start

Confirm you have:

- `autumn-cli` >= 0.5.0 installed on the machine that runs the CLI.
- SSH or shell access to the working directory of the running app (or a shared
  volume that all replicas read from).
- The `AUTUMN_DATABASE__PRIMARY_URL` environment variable set to the write
  connection string.

### Step 1 — Enter maintenance

```bash
autumn maintenance on \
  --message "We are running a planned migration. Back online in a few minutes." \
  --allow-ips 10.0.0.0/8
```

Verify the banner appears in the app's log output within 500 ms. Check the
health endpoint (which always passes through):

```bash
curl -i http://localhost:3000/actuator/health
# Expected: 200 OK — the actuator prefix is always allowed
```

Verify that application routes are blocked:

```bash
curl -i http://localhost:3000/
# Expected: 503 Service Unavailable
# Expected header: Retry-After: 120
```

### Step 2 — Run the migration

```bash
AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@host:5432/myapp_prod" \
  autumn migrate
```

If the migration succeeds, move to Step 3.

If the migration **fails**, leave maintenance on and investigate before removing
the flag. A failed destructive migration may have left the schema in a partial
state. Do not re-open traffic until you have confirmed the schema is consistent.

```bash
# Once the schema is confirmed safe:
autumn maintenance off
```

### Step 3 — Exit maintenance

```bash
autumn maintenance off
```

Traffic resumes within 500 ms. Confirm with:

```bash
curl -i http://localhost:3000/
# Expected: 200 OK (or whatever your root route returns normally)
```

### Step 4 — Verify

Run `autumn doctor` to confirm no residual maintenance state:

```bash
autumn doctor
# Expected: all checks PASS, including "Maintenance mode: PASS"
```

### Automated version (CI/CD pipeline)

`autumn migrate --with-maintenance` condenses Steps 1–3 into a single command.
Use it in your release pipeline when migrations are always safe to run
automatically:

```bash
AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@host:5432/myapp_prod" \
  autumn migrate --with-maintenance
```

The flag is automatically removed on success. On failure the flag remains and
the command exits non-zero, failing the pipeline step so the outage window does
not silently close while the schema is broken.

---

## Fly.io deploy integration

For Fly deployments using a `release_command`, pair maintenance with the
migration release command:

```toml
# fly.toml
[deploy]
  release_command = "autumn migrate --with-maintenance"
```

When the release command runs in a temporary Fly machine, it enters maintenance
(blocking traffic on the existing machines via the shared volume), runs
migrations, then exits maintenance. The new machines roll out only after the
release command exits zero. See [deployment.md](deployment.md) for the full
Fly.io setup.

---

## Relation to other safe-deploy features

| Feature | Guide | What it protects |
|---|---|---|
| Migration safety | [deployment.md](deployment.md) | Ensures migrations run before web replicas start (schema-first rollout). |
| Graceful shutdown | [deployment.md](deployment.md) | Ensures in-flight requests complete before the process exits (SIGTERM → drain → exit). |
| Maintenance mode | This guide | Stops new requests from reaching the application while a maintenance operation runs. |
| `autumn deploy maintenance` | [fleet-deploys.md](fleet-deploys.md) | Applies the same gate to **every deploy-managed host at once** over SSH, so a fleet enters and leaves the window together. Gates traffic; does **not** drain hosts from the load balancer. |

These features are complementary. A typical zero-downtime destructive
migration uses the first three: graceful shutdown ensures no request is abandoned
mid-flight when the old replica exits; migration safety ensures the schema is
updated before new replicas serve traffic; maintenance mode ensures writes are
paused while the migration runs. On a multi-host fleet, the fourth is how you
open and close that window on every host at once.

---

## Next steps

- **Automate**: wire `autumn migrate --with-maintenance` into your CI/CD
  release pipeline so every deploy automatically manages the maintenance window.
- **Alert**: add a log alert on `"Maintenance mode ENABLED"` in your log
  aggregator so on-call is paged if maintenance is left on unexpectedly.
- **Monitor**: `autumn doctor` can be run as a cron job or a CI step to catch
  a forgotten maintenance flag before it affects users.
