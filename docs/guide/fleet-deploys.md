# Fleet Deploys

[`autumn deploy`](deployment.md) started as a one-server tool. This guide is
about the other shape: **several app servers behind a load balancer you own**,
rolled onto a new release one at a time, with `autumn deploy up` doing the
rolling.

Read this when you are about to add a second app server, or already run several
and want the runbooks for drift, a half-finished rollout, and a fleet-wide
maintenance window.

Target time: **under 15 minutes** to take a working single-host deploy to a
three-host fleet.

> The mechanics of a single host — the release layout, the blue/green slots, the
> `/ready` gate, the systemd units, where secrets live — are all in the
> [deployment guide](deployment.md). This page assumes them and covers only what
> changes when there is more than one host.

---

## The topology

```
                        ┌──────────────────────────────┐
   internet ─── TLS ───▶│  YOUR load balancer          │   (separate host —
                        │  health check: GET /ready    │    not managed by
                        └───────┬───────┬───────┬──────┘    autumn deploy)
                                │       │       │
                  ┌─────────────┘       │       └─────────────┐
                  ▼                     ▼                     ▼
        ┌───────────────────┐ ┌───────────────────┐ ┌───────────────────┐
        │ 10.0.0.1          │ │ 10.0.0.2          │ │ 10.0.0.3          │
        │  kamal-proxy      │ │  kamal-proxy      │ │  kamal-proxy      │
        │   ├ blue  (live)  │ │   ├ blue  (live)  │ │   ├ blue  (live)  │
        │   └ green (idle)  │ │   └ green (idle)  │ │   └ green (idle)  │
        └─────────┬─────────┘ └─────────┬─────────┘ └─────────┬─────────┘
                  └─────────────────────┼─────────────────────┘
                                        ▼
                     shared Postgres · shared Redis · object storage
```

Two things follow from this picture, and they explain most of the rules below:

1. **kamal-proxy is per host, not a fleet load balancer.** Each app server runs
   its own proxy, owning that host's public port and flipping that host's
   blue/green slots. Nothing in `autumn deploy` distributes traffic *between*
   hosts.
2. **The load balancer is yours.** `autumn deploy` neither provisions it nor
   changes its membership. What Autumn gives you is a correct health signal
   (`/ready`) and a rollout that touches one host at a time — and keeps even that
   host serving throughout its own blue/green cutover.

---

## The load-balancer contract

Four rules. Getting the first one wrong is the classic outage.

### 1. Health-check `/ready`. Never `/live`

Point your load balancer's health check at **`/ready`** (the path is
`[health] ready_path`, default `/ready`).

`/live` returns **`200` unconditionally** — it is a liveness probe, answering
"is this process running?", and a supervisor uses it to decide whether to
restart. It says nothing about whether the process can serve. A load balancer
health-checking `/live` will keep a host in rotation while it is still starting
up, while it is shutting down, and while its database pool is unusable.

`/ready` gates on all of that: it is `503` until startup completes, `503` the
instant a shutdown begins, and `503` when a dependency or a readiness
[health indicator](health-indicators.md) is down. That is the signal you want.

### 2. Budget ~35 seconds for a host to leave rotation

When an app process is asked to stop, it does this, in order:

1. `/ready` flips to `503` immediately — your load balancer should now stop
   routing new requests here.
2. It waits `[server] prestop_grace_secs` (default **5 s**) for the load balancer
   to actually notice and drain its connection pool.
3. The listener closes.
4. In-flight requests finish, up to `[server] shutdown_timeout_secs`
   (default **30 s**).

So the default stop budget is about **35 seconds**. Tune `prestop_grace_secs` to
your load balancer's health-check interval × unhealthy-threshold plus its
deregistration delay: too short and the LB is still sending requests to a closed
listener. See [Staged and zero-downtime deploys](staged-deploys.md) for the full
drain lifecycle.

Note that a *rolling deploy* does not normally exercise this path from the load
balancer's point of view: the host's own kamal-proxy flips between slots
atomically, so the host keeps answering `/ready` with `200` throughout. The
budget matters when you take a host out of the fleet, reboot it, or scale down.

### 3. Maintenance mode does **not** drain a host

`/ready` deliberately **bypasses** maintenance mode: a host with the maintenance
flag set keeps answering its health check with `200` and stays in rotation,
serving `503` + `Retry-After` to real user traffic.

That is intentional. If maintenance gated `/ready`, turning maintenance on across
a fleet would eject *every* host from the load-balancer pool simultaneously —
turning a controlled maintenance window into a hard outage, and, on many
platforms, into a restart loop.

The consequence for you: **if you need a host out of rotation, drain it at the
load balancer.** Maintenance mode is a traffic *gate*, not a drain.
`autumn deploy status` therefore reports readiness and maintenance in separate
columns, and `autumn deploy maintenance on` repeats this warning every time.

### 4. Terminate TLS at the load balancer

A fleet deploy **refuses** `[deploy.tls] enabled = true`. Each host's kamal-proxy
would request a certificate for the same public hostname from behind your load
balancer; only one of them can answer any given ACME challenge, and the rest burn
Let's Encrypt failed-validation and duplicate-certificate rate limits.

Terminate TLS at the load balancer and set `[deploy.tls] enabled = false`.

Run the load balancer on a **separate host**, too. kamal-proxy always binds
`:443` on its host and cannot release it, so an external TLS terminator cannot
share a deploy-managed box. (This is not fleet-specific — it is true of a
single-host deploy as well; see the HTTPS/TLS note in the
[deployment guide](deployment.md#push-button-deploy-to-your-own-server-autumn-deploy)
and the [TLS guide](tls.md).)

---

## Shared state: what every host must agree on

Every host in the fleet receives a **byte-identical** env file and manifest — the
same signing secret, the same database URL, the same `autumn.toml`. That is what
makes the fleet one application rather than N applications. It also means
anything that must be shared has to live *outside* the hosts.

| Concern | What a fleet needs | Where |
|---|---|---|
| Database | One Postgres every host can reach. A `sqlite://` URL is **refused** for a multi-host deploy — identical URL, N independent files. | [Topologies a fleet refuses](deployment.md#topologies-a-fleet-deploy-refuses) |
| Signing secret | Identical on every host, or sessions, signed blob URLs and CSRF tokens break as soon as a request lands on a different host. The deploy already ships the same secret everywhere; keep it stable across deploys. | [signing-secrets.md](signing-secrets.md) |
| Sessions | Redis session backend. In-memory sessions are per host. | [Multi-replica setup](deployment.md#multi-replica-setup) |
| Rate limiting | Redis rate-limit backend, or an N-host fleet permits N× your configured rate. | [rate-limiting.md](rate-limiting.md) |
| Uploaded files | Object storage (`[storage] backend = "s3"`). Local disk is per host, and a release dir is wiped by pruning. | [storage.md](storage.md) |
| Scheduled tasks | Advisory-lock coordination so a `#[scheduled]` task runs once per fleet, not once per host. | [scheduled-multi-replica.md](scheduled-multi-replica.md) |
| Migrations at boot | `[database] auto_migrate` **off**. Every host would apply migrations at boot and race the others. | below |

The [Multi-replica setup](deployment.md#multi-replica-setup) section of the
deployment guide has the concrete config for the shared session/rate-limit
backends — it is the same configuration whether your replicas are containers or
`autumn deploy` hosts. Don't duplicate it; read it there.

---

## Scaling one host to three

You have a working single-host deploy. Here is the whole change.

### 1. Provision the new hosts

Each new host needs exactly what the first one needed
([Preconditions](deployment.md#preconditions)): key-based SSH access for
`[deploy] user`, and the `kamal-proxy` binary at `/usr/local/bin/kamal-proxy`.
Nothing else — the release layout, units and directories are created for you.

### 2. Move the shared state off the app host

If your single host was running Postgres or Redis locally, move them now. Every
host must reach the same database and the same Redis. Work through the table
above; a fleet on a `sqlite://` database is refused outright.

### 3. Swap `host` for `hosts`

```toml
[deploy]
# host = "10.0.0.1"
hosts = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
```

The two keys are mutually exclusive — keep one. **List order is rollout order**,
so put the host you want replaced first, first. Blank and duplicate entries are
refused.

`AUTUMN_DEPLOY__HOSTS=10.0.0.1,10.0.0.2,10.0.0.3` does the same from the
environment, replacing the file's list entirely.

### 4. Check, then roll

```bash
autumn deploy check     # grades every host: SSH per host, project graders once
autumn build --embed
autumn deploy up
```

`deploy check` is the cheap step that catches the new hosts being unreachable,
and it names them individually. `deploy plan` will additionally print the rollout
order and the migrate-placement rule without contacting anything.

### 5. What the first fleet rollout actually does

The interesting part of a scale-up is that the hosts are in **different states**,
and the rollout handles that explicitly:

- `10.0.0.1` is already serving, so it takes the **zero-downtime redeploy** path:
  candidate on the idle slot, migrate, `/ready` gate, atomic flip.
- `10.0.0.2` and `10.0.0.3` have nothing installed, so they take the **first
  deploy** path: install kamal-proxy, stand the release up, health-gate, route.
- The migration runs on `10.0.0.1` **only** — the first host in rollout order
  that is still on a previous release — before its cutover. The two new hosts
  skip it, because the schema is fleet-wide and running it three times is at best
  redundant and at worst a race.

```
Rolling release 20260821T101500Z across 3 hosts, ONE AT A TIME, in `[deploy] hosts` order:
  1. 10.0.0.1 — zero-downtime redeploy
  2. 10.0.0.2 — first deploy
  3. 10.0.0.3 — first deploy
  → migrate (10.0.0.1 only — the schema is fleet-wide; 10.0.0.2, 10.0.0.3 skip it)
```

> **A fleet where *every* host is a first deploy migrates nowhere.** A first
> deploy has never run migrations (it stands the release up and health-gates it),
> so a brand-new fleet with no host on a previous release runs no migration at
> all — and says so loudly, telling you to run `autumn migrate` yourself before
> serving traffic. That warning is your cue, not a bug.

### 6. Add the new hosts to the load balancer

Only after `deploy up` reports all three serving. `autumn deploy` does not touch
your load balancer's membership — adding and removing backends is yours to
automate.

Confirm with:

```bash
autumn deploy status
```

Three rows, one release, no drift.

---

## Runbook: drift and partial rollouts

### Detecting drift

```bash
autumn deploy status --strict
```

Read-only, safe mid-incident, and non-zero on any drift — so it belongs in cron
or your monitoring:

```
# crontab: alert when the fleet stops being on one release
*/10 * * * * cd /srv/deploy/myapp && autumn deploy status --strict --json >/var/log/autumn-fleet.json 2>&1
```

It reports two independent things:

- **Version drift** — hosts on different releases. Something did not converge.
- **State drift** — per-host marker damage that will make that host's **next**
  deploy fail closed or take the wrong slot. A perfectly converged fleet can
  still have state drift, which is exactly why the two are not merged.

A host that does not answer is an `unreachable` row, and a host whose release
cannot be read is reported as `release unknown` and explicitly **not** counted as
version drift. A false "your fleet is mixed" alarm at 3 am is worse than no
alarm.

### Converging a mixed fleet

The default answer to version drift is to roll forward:

```bash
autumn deploy up          # the whole fleet, one release, in order
```

or, if the new release is the problem, take everything back:

```bash
autumn deploy rollback    # every host, newest first
```

`rollback` exits non-zero unless every host came back — including a host that had
*nothing* to roll back to, which is reported as a skip and still counts as
failure. That is the honest signal: the fleet is not on one release.

### Repairing one host

When a single host is the problem, `--only` narrows either command:

```bash
autumn deploy rollback --only 10.0.0.2    # take one host back
autumn deploy up --only 10.0.0.2          # or push the intended release to it
```

Every `--only` run prints a loud warning naming the hosts it is *not* touching,
because narrowing a rollout is precisely how a fleet ends up mixed on purpose.
**Finish with a full `autumn deploy up`** and confirm with `deploy status
--strict`.

### When the fleet says "NOT rolled back automatically"

Four situations make a host's rollback *target* untrustworthy, and the fleet
refuses to guess:

| Reason | What it means |
|---|---|
| release markers left mid-transaction by `commit-markers` | The previous-release / `current` / live-slot triple is written as one remote transaction; a failure inside it can leave any subset applied. |
| rollback target release dir missing | The marker names a release directory that is not there (pruned, or removed by hand). |
| rollback target release dir could not be verified | The probe proved nothing either way. |
| no previous release recorded to roll back to | A first deploy clears the marker; a freshly added host never had one. |

In every one of these, running `autumn deploy rollback --only <host>` is the
*wrong* first move — that command trusts the target that is in doubt. The deploy
prints the exact read-only command to look first:

```bash
ssh root@10.0.0.2 'cat /srv/autumn/myapp/shared/previous-release /srv/autumn/myapp/shared/live-slot; ls /srv/autumn/myapp/releases'
```

`previous-release` names the release dir, slot and port the host should return
to. Restore it by hand, then deploy the fleet again.

### After any halted rollout: check the schema

An automatic compensation restores **binaries only**. If the rollout got far
enough to migrate, the schema is still forward while the binaries are back. That
is safe *if* your migrations are expand/contract (below) and alarming if they are
not — so confirm it explicitly rather than assuming the rollback undid
everything.

---

## Runbook: a fleet-wide maintenance window

For a change that genuinely needs write traffic stopped across the whole fleet —
a destructive migration, a database failover:

```bash
# 1. Gate every host at once. Apps react within 500 ms; no restart, no deploy.
autumn deploy maintenance on \
  --message "Upgrading the database. Back by 14:30 UTC." \
  --allow-ips 10.0.0.0/8

# 2. Confirm every host actually took the flag.
autumn deploy status

# 3. Do the work.
autumn migrate

# 4. Reopen.
autumn deploy maintenance off
```

Four things to know before you run it:

- **It is not a drain.** Every host stays in your load-balancer pool and answers
  user traffic with `503`. If the work requires *zero* traffic reaching the app,
  drain at the load balancer as well.
- **A partial result is reported, never reversed.** If host 2 fails, hosts 1 and
  3 stay in maintenance and are named in the summary; the command exits non-zero.
  Reversing them automatically would push users straight back into the window you
  are closing. Reversing by hand (`autumn deploy maintenance off`) is your call.
- **The flag survives deploys.** Deploy-managed hosts read
  `{app_dir}/shared/autumn-maintenance.json`, in the shared directory, because
  `autumn deploy` stamps `AUTUMN_MAINTENANCE_FLAG_FILE` into every slot unit. A
  cutover, a rollback and a prune all leave it in place, and both blue and green
  see the same flag. (See [maintenance-mode.md](maintenance-mode.md).)
- **The local `autumn maintenance` is a different command.** It writes *this*
  machine's working directory, which is not the host you deploy to. Use
  `autumn deploy maintenance` for deploy-managed hosts.

---

## Expand/contract is the prerequisite for safe rollback

The single most important schema rule for a fleet:

> **Nothing ever rolls a migration back.** Not the automatic compensation after a
> halted rollout, not `autumn deploy rollback`. Both restore binaries only.

This is deliberate. An automatic `migrate down` would run, unattended and
mid-incident, exactly the SQL that nothing reviews — `autumn migrate check`
grades the `up` direction. Silently executing the un-reviewed half of a migration
while a fleet is already in trouble is not a recovery mechanism.

It also means a rolling deploy inherently runs **old and new binaries against the
new schema at the same time** — that is not an edge case, it is every moment
between host 1's cutover and host N's. Both facts point at the same discipline:

**Expand → migrate → contract**, across two releases.

| Release | Migration | Code |
|---|---|---|
| N | *Expand*: add the new nullable column / new table / new index. Additive only — nothing existing is dropped or renamed. | Writes both old and new; reads old. |
| N (later) or N+1 | Backfill, then switch reads to the new shape. | Reads new, still writes both. |
| N+2 | *Contract*: drop the old column, once no deployed release reads it. | Uses the new shape only. |

Every step leaves the previous release able to run against the migrated schema —
which is precisely what makes an automatic rollback safe, and what makes the
mixed window during a rollout a non-event.

`autumn migrate check` classifies your local `migrations/` by rolling-deploy risk
and already runs as part of the deploy preflight, so an unsafe migration fails
before anything is touched. For a change that genuinely cannot be made
expand/contract, use the [maintenance window runbook](#runbook-a-fleet-wide-maintenance-window)
above instead of hoping the rollback will save you.

---

## What a fleet deploy does not do

Stated plainly so you can plan around it:

- **No load-balancer management.** No provisioning, no health-check
  configuration, no adding or removing backends during a rollout. Your LB, your
  automation.
- **No parallel rollout.** Hosts are replaced strictly one at a time. A batch or
  percentage rollout is not configurable.
- **No canary weighting.** Autumn's canary primitives (version-labelled metrics,
  the `X-Canary` extractor, `autumn canary rollback`) are driven by a controller
  moving traffic weights at *your* load balancer — see
  [Canary deploys](staged-deploys.md#canary-deploys). `autumn deploy` does not
  shift traffic between hosts.
- **No per-role fleets.** Every host in `[deploy] hosts` gets the same release,
  the same env file and the same unit. Splitting web and worker roles across
  different host lists is not expressible today.
- **No migration rollback.** As above.
- **No media provisioning on a fleet.** `[media.mediamtx]` is refused for a
  multi-host deploy — host media provisioning has no teardown path. Deploy media
  on a single host.

---

## Next steps

- **[Deployment guide](deployment.md)** — the full `autumn deploy` surface:
  release layout, blue/green slots, secrets, `deploy plan`, MediaMTX, and the
  container/PaaS alternatives.
- **[Maintenance mode](maintenance-mode.md)** — the flag file, the allow-list
  options, and the destructive-migration runbook.
- **[Staged and zero-downtime deploys](staged-deploys.md)** — the drain
  lifecycle, blue/green at the platform level, and canary primitives.
- **[Multi-replica setup](deployment.md#multi-replica-setup)** — the shared
  session, rate-limit and secret configuration every fleet needs.
- **Alert on drift** — wire `autumn deploy status --strict` into cron so a fleet
  that quietly stopped converging pages someone.
