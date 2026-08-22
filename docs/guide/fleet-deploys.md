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
| Background jobs | A durable queue every host shares: `[jobs] backend = "postgres"` or `"redis"`. The default `local` backend is an in-process queue, so N hosts means N independent queues. | [below](#background-jobs-the-default-queue-is-per-host) |
| Scheduled tasks | `[scheduler] backend = "postgres"` — advisory-lock coordination so a `#[scheduled]` task runs once per fleet, not once per host. The default is a per-process timer. | [scheduled-multi-replica.md](scheduled-multi-replica.md) |
| Migrations at boot | `[database] auto_migrate` **off**. Every host would apply migrations at boot and race the others. | below |

The [Multi-replica setup](deployment.md#multi-replica-setup) section of the
deployment guide has the concrete config for the shared session/rate-limit
backends — it is the same configuration whether your replicas are containers or
`autumn deploy` hosts. Don't duplicate it; read it there.

### Background jobs: the default queue is per host

This is the row people miss, because on one host it is invisible.

Every fleet host runs the **combined** process role — one env file and one unit
go to every host, and nothing in `autumn deploy` sets a per-host role — so all
three hosts serve HTTP *and* run job workers *and* run the cron scheduler. That
is the right shape for a fleet, but only once the queue those workers drain is
shared. `[jobs] backend` defaults to `local`, which is an in-process queue. Three
hosts on the default are three independent queues:

- **Work never balances.** A job enqueued while serving on `10.0.0.2` can only
  ever be executed by `10.0.0.2`.
- **A rollout drops pending work.** `local` holds queued and delayed jobs in
  memory only; a rolling deploy stops each host's old slot in turn, so anything
  not yet run is gone. The durable backends keep it (`run_at` column on Postgres,
  a due-time ZSET on Redis).
- **`unique` and `concurrency` caps are per host.** `#[job(unique)]` and
  `concurrency = N` are distributed-safe only on the durable backends, so the
  same "runs at most once" job can run once *per host*.
- **Tracked-job polling breaks behind the load balancer.** The record store
  behind `enqueue_tracked` follows the jobs backend; on `local` it is
  process-local, so when the browser's next poll of `GET /_autumn/jobs/{token}`
  lands on a different host, that host returns the same `404` it returns for an
  unknown token.
- **`/admin/jobs` shows one host.** The local runtime installs a process-local
  dashboard backend, so the dashboard — and its retry/discard/cancel buttons —
  act on whichever host the load balancer happened to pick. (`redis` installs a
  cluster-wide dashboard backend automatically.) `/actuator/jobs` counters are
  per host for the same reason.

So pick a shared backend before the first fleet rollout:

```toml
[jobs]
backend = "postgres"   # or "redis"
```

`postgres` is usually the cheaper choice for a fleet: a fleet already requires a
shared Postgres (`sqlite://` is refused), the queue reuses the `[database]` pool,
and no new service appears. It needs the framework's `autumn_jobs` table, which
`autumn migrate` creates — so on a fleet that has already deployed against
Postgres it is there, and on a brand-new fleet it arrives with the `autumn
migrate` the guide already tells you to run. Do not count on the *same* rollout
to create it: a host's candidate process starts before that host's `migrate` step
runs. `redis` reuses the Redis you already stood up for sessions and rate
limiting; `[jobs.redis] key_prefix` keeps its keys separate.

**Nothing refuses `local` on a fleet — but `deploy up` does warn.** The
in-process defaults are correct on one host and are what every un-configured app
runs, so refusing them would break the scale-up this guide exists to make easy.
Instead, whenever more than one host is configured, the rollout prologue prints:

```
⚠️  `[jobs] backend = "local"` (an in-process queue) and `[scheduler] backend =
"in_process"` (a per-process timer) are in effect and this deploy targets more
than one host — they are PER HOST: each host then runs its OWN copy, so work
enqueued on one host is only ever run by that host, whatever is still queued dies
when the next rollout drains that host's old slot, `unique`/`concurrency` limits
stop being enforced fleet-wide, and every scheduled task fires once PER HOST.
Move them to a shared backend (`postgres` or `redis`) before you rely on
background work across the fleet — see docs/guide/fleet-deploys.md (#1621).
```

It names only the key(s) actually in effect, so a fleet on a shared queue with an
in-process scheduler is warned about the scheduler alone. It is keyed on the
number of *configured* hosts, so `--only` does not silence it, and it is silent
for a single-host config, where the defaults are right.

That warning is the **only** place this surfaces. It comes from the rollout
driver, after the preflight report — so `deploy check`, `deploy plan` and
`deploy status` say nothing about it, and neither does `autumn doctor` (its
`process_role_backend` check only fires for a split `web`/`worker` role, and
fleet hosts are all combined-role). Sessions and rate limiting get no warning at
all. Work the table above yourself rather than waiting to be told.

Full backend comparison, delivery semantics and the migration note are in the
[jobs guide](jobs.md#backend-selection-autumntoml); don't duplicate them, read
them there.

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

This is also where you switch the backends that silently defaulted to per-host
on a single server: sessions, rate limiting, `[jobs] backend` and
`[scheduler] backend`. Nothing in the rollout *refuses* a per-host default — it
becomes three of everything. It does not become three of everything quietly,
though: `autumn deploy up` prints a loud ⚠️ naming `[jobs] backend = "local"`
and/or `[scheduler] backend = "in_process"` whenever the deploy targets more than
one host ([above](#background-jobs-the-default-queue-is-per-host)). Sessions and
rate limiting get no such warning — those two are on you.

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
- The migration runs on `10.0.0.1` **only** — the **first host in rollout order**
  — before its cutover. The other two skip it, because the schema is fleet-wide
  and running it three times is at best redundant and at worst a race.

```
Rolling release 20260821T101500Z across 3 hosts, ONE AT A TIME, in `[deploy] hosts` order:
  1. 10.0.0.1 — zero-downtime redeploy
  2. 10.0.0.2 — first deploy
  3. 10.0.0.3 — first deploy
  → migrate (10.0.0.1 only — the schema is fleet-wide; 10.0.0.2, 10.0.0.3 skip it)
```

> **A brand-new fleet migrates too.** A first deploy runs its pending migrations
> before it starts the release, so a fleet where *every* host is a first deploy
> still migrates exactly once — on host 1, before that host takes traffic. There
> is no out-of-band `autumn migrate` step for a new fleet.

#### Rollout order and the migration

The migration is placed on the **first host in rollout order**, whatever state
that host is in. Host 1 is the earliest point in the rollout, so the schema
always moves before any host in the fleet serves the new release — including in
the scale-up shape `hosts = ["<new>", "<new>", "<existing>"]`, where the leading
new host carries the migration itself.

Declaration order still matters for *everything else* — it is the rollout order,
and it is never reordered for you — but it is no longer load-bearing for
migration safety.

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

That exclusion is about *version* drift only. A **reachable** host that proved it
has a `current` symlink, yet resolves to no readable release, is now **state**
drift and `--strict` exits non-zero on it (it was previously reported without
counting). Its row says so:

```
⚠️  this host has a `current` symlink but the release it points at could not be
read (a broken symlink or a missing releases dir) — repair it before the next
deploy, which would record that unresolvable target as this host's rollback point
```

Two more state-drift reasons come from the maintenance probe, and both name the
action they need:

- `the live slot unit could not be read, so which maintenance flag file this
  host's app polls is unknown — the maintenance column reports ? rather than
  guessing` — inspect the unit on that host; the row's maintenance cell reads
  `maintenance ?` instead of a confident `ON`/`off`.
- `this host's app polls a release-local maintenance flag file, not the shared
  one (its slot unit predates AUTUMN_MAINTENANCE_FLAG_FILE) — redeploy it so
  maintenance survives cutovers` — **redeploy that host.** Until you do, its flag
  is orphaned by the next cutover, and a fleet-wide `deploy maintenance` has to
  write that release-local file as a second write (resolved from the same live
  slot unit) — reporting the host as a `PARTIAL` failure if it cannot.

The first reason is also the one that makes `autumn deploy maintenance` fail
closed on that host: if the live slot unit cannot be read, the fan-out writes the
shared flag and then reports the host as `PARTIAL` with a non-zero exit, rather
than guessing at a path from the `current` symlink.

> **A broken app config no longer takes `deploy status` offline.** `status` needs
> only `[server] port` from your application config, so a config that fails to
> validate under the deploy profile is no longer fatal to it: it prints a caveat
> on **stderr** (in `--json` mode too, leaving stdout's shape intact) naming the
> config error and the declared port it is probing against, then reports the
> fleet anyway. `deploy check`, `up` and `rollback` still refuse — they grade and
> upload runtime *values*, so an invalid config has to stop them. That asymmetry
> is deliberate: the read-only incident command stays available, the
> state-changing ones do not.
>
> `autumn deploy maintenance` is on the available side of that line too, for the
> same reason: a window gets closed mid-incident. It prints the same caveat, then
> continues against the declared port — which it uses **only** to identify which
> slot unit each host is running:
>
> ```
> ⚠️  this project's configuration does not validate under the `production` deploy
> profile: <the config error>
>    `deploy maintenance` continues against the DECLARED `[server] port = 80` for
> that profile (read without validation); it uses that port only to identify which
> slot unit each host is running. `autumn deploy check`/`up` still refuse to run
> until the config is fixed.
> ```

Each row also carries a `last deploy` cell — the last action that host
*completed* (`deployed`, `rolled back` or `torn down`, with the host's UTC time;
`?` when the marker is absent or unreadable). Mid-incident that is what tells a
compensated host apart from one that simply deployed, since both read back
healthy and on the same release. A host the rollout compensated by removing its
first deploy reads `torn down <time>`, which is how you tell it from a host that
was never deployed at all — that one reads `?`. Mind its scope: a deploy that
failed *before* cutover never
rewrites it, so it is that host's own last completed action rather than a
verdict on the last rollout, and it is reported, never counted as drift.

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

> **`--only` down to one host is a single-host run, and it behaves like one.**
> This is worth knowing *before* an incident, because the fleet summary's
> recovery line points you at exactly this command. Narrowing to a single target
> takes the pre-fleet code path, so `autumn deploy rollback --only 10.0.0.2`:
>
> - prints **no `Fleet state:` table** and no per-host outcome rows — you get
>   `Rolling back 10.0.0.2 to <release>…` and a single `✅ Rollback complete.`, or
>   a plain error. Run `autumn deploy status` afterwards for the fleet-wide
>   picture.
> - gets **no benefit from the fleet rollback's reachability softening.** A
>   multi-host rollback deliberately continues past a host that does not answer
>   SSH, because stopping would strand the *other* hosts on the release you are
>   abandoning. With one target there are no other hosts: an unreachable host
>   stops the command non-zero having changed nothing.
> - still needs the project's local inputs (signing secret, database URL,
>   `migrations/`) wherever you invoke it — the preflight runs first either way.
>
> The same narrowing applies to `--only` on `deploy up`: only the selected hosts
> are reachability-graded. The fleet-wide topology refusals (`sqlite://`,
> `[media.mediamtx]`, `[deploy.tls]`) are keyed on the *configured* host count, so
> `--only` never unlocks those.

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

**The trigger is the migration, not the compensation.** If the rollout got far
enough to reach the host carrying the migration, the schema is forward — whether
or not the fleet ever compensated anything. That is safe *if* your migrations are
expand/contract (below) and alarming if they are not, so confirm it explicitly
rather than assuming the halt undid everything.

The `Fleet state:` summary states which of three things is true, and it is worth
reading the exact sentence rather than skimming for the ⚠️:

- **Some host is still on the new release** → `the schema has moved; from here an
  automatic rollback restores BINARIES only — it never rolls a migration back`.
  Forward-looking: it describes what a rollback *you* run next will not undo.
- **The fleet actually put a host back** — restored it to its previous release,
  or removed its just-completed first deploy → `the compensating rollback
  restored BINARIES only — the migration that already ran was NOT rolled back;
  confirm the schema still fits the release now serving`. A compensation that
  only **failed** does not produce this line: that host is still serving the new
  release, so it gets the forward-looking note above instead. Both lines appear
  together when a halt compensated some hosts and left others forward.
- **Nothing is forward and nothing was compensated** — the migrating host failed
  after `migrate` but before its own cutover (a `readiness-gate` timeout is the
  usual shape) and tore its candidate down, so every later host was never touched
  → `no host is serving the new release, but the migration that already ran was
  NOT rolled back — the binaries went back and the schema did not; confirm the
  release now serving still fits the migrated schema`. This is the one that reads
  like a clean no-op and is not: the table is all "previous release still
  serving" rows, and the database has already moved on.

Every non-empty rollout schedules a migration, so the gate on all of these is
simply whether the rollout reached host 1 at all: one that failed before touching
it moved no schema and prints none of these.

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

Step 2 is a real check, not a formality. The `maintenance` column reports the
flag file **the host's running slot unit actually polls** — resolved from that
unit's `Environment=AUTUMN_MAINTENANCE_FLAG_FILE`, or from its
`WorkingDirectory` plus the legacy relative `tmp/autumn-maintenance.json` when
the unit predates that override. So read it as three answers, not two:

| Cell | What it means | What to do |
|---|---|---|
| `maintenance ON` | The file that host's running unit polls exists. | Nothing — the window is closed on that host. |
| `maintenance off` | That file does not exist. | The host is still serving normally. |
| `maintenance ?` | The live slot unit could not be read, so *which* file the app polls is unproven. | Inspect the unit; never read this as "off". On a `deployed` host it is also state drift. |

A host whose unit predates the shared flag path reads its **release-local** file
here — true for that host, and flagged as state drift with `redeploy it so
maintenance survives cutovers`. Note the column's scope: it proves which file the
unit polls and whether that file is there, not what the process currently holds
in memory — the app picks the change up on its own 500 ms poll.

Five things to know before you run it:

- **It is not a drain.** Every host stays in your load-balancer pool and answers
  user traffic with `503`. If the work requires *zero* traffic reaching the app,
  drain at the load balancer as well.
- **A partial result is reported, never reversed.** If host 2 fails, hosts 1 and
  3 stay in maintenance and are named in the summary; the command exits non-zero.
  Reversing them automatically would push users straight back into the window you
  are closing. Reversing by hand (`autumn deploy maintenance off`) is your call.
  The `Changed anyway: …` line lists only the hosts that changed **fully** — a
  partially-changed host is named on the failed side instead, so never treat that
  line as "everything except these is done".
- **A `PARTIAL` row means that host may still be serving traffic.** The fan-out
  writes the shared flag first and then, when the host's live slot unit polls a
  different file (a unit deployed before the shared path existed), that file too.
  If the live unit cannot be read — or that second write fails — the row is:

  ```
  ❌ web-3  PARTIAL — shared flag written, but the file this host's RUNNING unit
  polls was NOT (failed at `detect-maintenance-flag`), so this host may still be
  serving traffic
  ```

  This **counts as a failure** and the command exits non-zero, even though the
  host did change. Treat that host as **not maintained**: its shared flag is set,
  but a unit that predates the shared path ignores it entirely, so it may still
  be taking write traffic. Do not start the destructive work until you have
  either fixed it or accepted it — run `autumn deploy status` to see what that
  host's unit actually polls (a `maintenance ?` cell means the unit still cannot
  be read), inspect
  `/etc/systemd/system/{service}-{slot}.service` on the host, and redeploy it so
  its unit carries `AUTUMN_MAINTENANCE_FLAG_FILE`. The same row on `off` ends
  `so this host may still be in maintenance` — there, the host is the one still
  gated after you thought you had reopened.
- **The flag survives deploys.** Deploy-managed hosts read
  `{app_dir}/shared/autumn-maintenance.json`, in the shared directory, because
  `autumn deploy` stamps `AUTUMN_MAINTENANCE_FLAG_FILE` into every slot unit. A
  cutover, a rollback and a prune all leave it in place, and both blue and green
  see the same flag. (See [maintenance-mode.md](maintenance-mode.md).)
- **The local `autumn maintenance` is a different command.** It writes *this*
  machine's working directory, which is not the host you deploy to — and SSH-ing
  into a host to run it there does not work either, because it writes the
  cwd-relative `tmp/autumn-maintenance.json` while a deploy-managed app reads
  `{app_dir}/shared/autumn-maintenance.json`. It exits `0` and changes nothing.
  Use `autumn deploy maintenance` for deploy-managed hosts; see
  [maintenance-mode.md](maintenance-mode.md#where-the-flag-file-lives).

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
- **No draining of a host, ever.** A rolling deploy does not take a host out of
  rotation and does not push its `/ready` to `503` — not before the cutover, not
  during it. Each host is replaced *in place*: the candidate starts on that
  host's idle loopback slot, is `/ready`-gated on that loopback port, and the
  host's own kamal-proxy flips upstreams atomically; the old slot is drained and
  stopped only **after** the flip. From your load balancer's point of view the
  host answers `200` on `/ready` the whole way through. If you need a host
  genuinely out of the pool — a reboot, a scale-down, a host-level repair — you
  drain it at the load balancer yourself, budgeting per
  [rule 2](#2-budget-35-seconds-for-a-host-to-leave-rotation). Maintenance mode
  is not that lever either ([rule 3](#3-maintenance-mode-does-not-drain-a-host)).
- **No proof about *your* load balancer.** Autumn's end-to-end deploy test rolls
  a real two-host fleet in CI and asserts that neither host returned a single
  failed response during the rollout. Read it for what it is: the checker is a
  **liveness prober** — one thread per host, one request at a time, roughly ten
  requests a second — pointed at each host's *public port directly*, with no load
  balancer in the harness at all. It is good evidence that a host being replaced
  keeps answering. It is not a load test, it says nothing about behaviour under
  concurrency, and it exercises none of your LB's health-check interval,
  unhealthy threshold or deregistration delay. Those you validate yourself.
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
- **[Background jobs](jobs.md#backend-selection-autumntoml)** and
  **[Multi-replica scheduled tasks](scheduled-multi-replica.md)** — the durable
  queue and the advisory-lock scheduler a fleet needs instead of the per-process
  defaults.
- **Alert on drift** — wire `autumn deploy status --strict` into cron so a fleet
  that quietly stopped converging pages someone.
