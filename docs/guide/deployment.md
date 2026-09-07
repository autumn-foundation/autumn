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
- The `autumn` CLI - `cargo install autumn-cli --version 0.7.0`

---

## Upgrading in place, without a restart

A deploy does not have to be a process replacement. On Linux an Autumn app can
swap itself to a newly-built binary while it is running — handing over its
listening socket (so no connection is refused) and a designated block of typed
in-memory state (so no cache starts cold), with the old→new state migration
proven total by the compiler:

```console
$ cargo build --release      # install the new binary over the running path
$ kill -USR2 $(pidof my-app)
```

See [In-place upgrades](hot-upgrades.md). The rest of this page covers the
process-replacement path, which is what `autumn deploy` and the container
images use.

---

## HTTPS with no certificate work (`[server.tls.acme]`)

If you run the binary yourself on a VPS — no `autumn deploy`, no reverse proxy —
the app can obtain and renew its own Let's Encrypt certificate. Point DNS at the
box, add the section below, and build with the `acme` feature; that is the whole
happy path:

```toml
# autumn.toml
[server]
host = "0.0.0.0"           # the default, 127.0.0.1, is loopback-only
port = 443                 # ACME wraps THIS listener in TLS; the default is 3000

[server.tls.acme]
domains = ["app.example.com"]
contact_email = "ops@example.com"
directory = "production"   # default is Let's Encrypt STAGING — validate there first
```

The `[server]` block is not optional here. ACME terminates TLS on the listener
`server.host`/`server.port` already binds, and only the challenge listener binds
`:80` on its own — so at the defaults you would serve HTTPS on `127.0.0.1:3000`,
where neither Let's Encrypt nor your visitors can reach it, and the HTTP→HTTPS
redirect would point at that unreachable port.

On first boot the app answers the ACME HTTP-01 challenge on `:80` (which also
redirects plain HTTP to HTTPS), serves the issued certificate on `:443`, caches
it plus the account key under `config/acme`, and renews in the background well
before expiry with no restart and no dropped connections. It needs inbound
TCP/80 and TCP/443, and the privilege to bind them
(`CAP_NET_BIND_SERVICE`). Run `autumn doctor --online` first: it grades DNS,
port reachability, and the stored certificate.

**Opting out** is the absence of the section. With no `[server.tls]` and no
`[server.tls.acme]`, the app serves plain HTTP on `server.port` and TLS is
somebody else's job — a reverse proxy (nginx, Caddy, a cloud load balancer), or
kamal-proxy if you use `autumn deploy` (see the note under
[`autumn deploy`](#push-button-deploy-to-your-own-server-autumn-deploy) — do not
combine the two). In-process ACME is a **single-host** feature: behind a load
balancer the CA's challenge can land on a replica that never published the
token, and the issued certificate is cached on one replica's local disk where
the others cannot read it. DNS-01 fixes the first of those, not the second.
Terminate TLS at the proxy for multi-replica deployments.

Full reference, including the private-CA / Pebble setup and the renewal
internals: [TLS & HTTPS guide](./tls.md).

### Subdomain-per-tenant: wildcard HTTPS on a VPS

The `saas` starter is built for multi-tenancy — it ships session-based tenant
resolution, which you switch to `source = "subdomain"` to give each tenant its
own hostname. That is what the single-hostname setup above cannot serve: one certificate per tenant means an issuance on
tenant *N*'s first request and Let's Encrypt rate limits as an onboarding
ceiling. A **wildcard** certificate covers every tenant, existing and future,
and needs the DNS-01 challenge — which needs your DNS provider's API token.

Zero to wildcard HTTPS, start to finish — sixteen lines of configuration across
three files, twelve of them in `autumn.toml`:

```console
$ autumn new myapp --starter saas
$ cd myapp
```

**1.** Point DNS at the box, once, by hand: an `A` record for `myapp.com` and a
wildcard `A` record for `*.myapp.com`, both at the VPS's public IP. (Autumn
writes only the ephemeral ACME challenge records, never these.)

**2.** Store the DNS provider token. `autumn credentials edit` opens the
encrypted store in `$EDITOR` — pass the profile you will run under, because that
is the file the server reads (`AUTUMN_ENV=production` resolves to the `prod`
profile):

```console
$ autumn credentials edit --env prod
```

```toml
[acme_dns]
api_token = "your-cloudflare-token"
```

A Cloudflare token needs **Zone:Read** (to find the zone) and **DNS:Edit** (to
write the challenge record), scoped to the zone.

**3.** Turn on the `acme` feature in the app's `Cargo.toml` — it is off by
default:

```toml
[features]
acme = ["autumn-web/acme"]
```

**4.** Edit `autumn.toml`. The starter already ships a `[server]` and a
`[tenancy]` table, so change those in place rather than adding second ones, and
append the two ACME tables — twelve lines, of which four are the ACME wiring
itself:

```toml
[server]
host = "0.0.0.0"   # the starter's default, 127.0.0.1, is loopback-only
port = 443         # ACME wraps THIS listener in TLS

[tenancy]
source = "subdomain"     # the starter ships "session"
base_domain = "myapp.com"

[server.tls.acme]
domains = ["myapp.com", "*.myapp.com"]
contact_email = "ops@myapp.com"
directory = "production"

[server.tls.acme.dns]
provider = "cloudflare"
```

**5.** Check it before spending a rate limit, then build:

```console
$ autumn doctor --online
$ AUTUMN_ENV=production autumn build --embed --features acme
```

On first boot the app publishes the two `_acme-challenge` TXT records, waits for
them to propagate, obtains the wildcard, removes the records, and serves HTTPS
on `:443`. Every tenant subdomain works immediately — and so does the next one
you create, with no certificate work at all.

Leave `directory` unset (Let's Encrypt **staging**) for the first run: the
certificate is untrusted, but the rate limits are generous enough to iterate
against. Switch to `production` once `autumn doctor --online` is clean and the
staging issuance succeeded.

Provider list, the escape hatch for providers not listed, propagation tuning and
the DNS-01 `autumn doctor` checks:
[TLS & HTTPS guide](./tls.md#wildcard-certificates-via-dns-01-servertlsacmedns).

---

## Push-button deploy to your own server (`autumn deploy`)

`autumn deploy` takes a fresh project to a live, zero-downtime service on a
Linux VPS you control — no Dockerfile for your app, no registry to publish it
to, no PaaS account. It uploads a single embedded binary, supervises it with systemd behind
a reverse proxy, migrates before cutover, health-gates on `/ready`, and flips
traffic atomically. Re-running it is a zero-downtime redeploy; one command rolls
back.

List several servers under `[deploy] hosts` instead of one under `[deploy] host`
and the same command becomes a **rolling deploy across the fleet**: hosts are
replaced one at a time in declaration order, migrations run exactly once, and a
mid-rollout failure halts and rolls the hosts that already cut over back. See
[Rolling deploy across a fleet](#rolling-deploy-across-a-fleet) and the
[fleet deploys guide](fleet-deploys.md).

This is the **primary** deployment path. A few alternatives remain documented
below and are better fits in specific cases:

- **[Deploy to fly.io](#deploy-to-flyio)** — a managed platform (machines,
  built-in metrics scraping, `fly deploy`).
- **[Deploy to Azure Container Apps](#deploy-to-azure-container-apps)** —
  Terraform-provisioned Container Apps, ACR, Postgres Flexible Server, and
  Key Vault for Azure-heavy shops that already have the IAM/RBAC patterns in
  place.
- **[Deploy to AWS App Runner](#deploy-to-aws-app-runner)** — the fast,
  minimal-Terraform AWS path: ECR, App Runner, and RDS behind a VPC
  connector, closest AWS analog to Fly.io.
- **[Deploy to AWS ECS Fargate](#deploy-to-aws-ecs-fargate)** — the
  production AWS path: VPC/ALB/ECS Fargate/RDS, for AWS-experienced infra
  teams who already have runbooks for this shape.
- **[Deploy to GCP Cloud Run](#deploy-to-gcp-cloud-run)** —
  Terraform-provisioned Cloud Run, Artifact Registry, Cloud SQL PostgreSQL
  behind a Serverless VPC Access connector, and Secret Manager-backed
  secrets, for GCP shops that already have workload identity federation in
  place.
- **The container path** ([Step 1](#step-1--create-the-project) through
  [How the production image works](#how-the-production-image-works)) — a
  portable OCI image you run on Kubernetes, ECS, Nomad, or any Docker host.

> **HTTPS/TLS.** kamal-proxy fronts your app and listens on your configured
> public HTTP port (`server.port`) and, **by default, 443** — its HTTPS listener
> is always bound and cannot be disabled. By default `autumn deploy` provisions
> no certificate for your app, so nothing is served over HTTPS until you opt in.
> Set `[deploy.tls] enabled = true` and `host = "app.example.com"` in
> `autumn.toml` to have `autumn deploy` pass `--host`/`--tls` on your app's
> kamal-proxy route/flip, so kamal-proxy provisions an **automatic Let's
> Encrypt** certificate for that host on-demand and terminates TLS for it on its
> always-bound 443 listener. This needs **no `server.port` change** — issuance
> uses TLS-ALPN-01 on the already-bound 443, so it works on both the first deploy
> and a later redeploy. Setting `server.port = 80` is **recommended** (it is the
> default) so the proxy also serves HTTP on 80 for the HTTP→HTTPS redirect, but
> it is not required. An external TLS terminator sharing the same host is **not**
> supported (kamal-proxy always binds 443 and cannot release it); run such a
> terminator on a separate host in front of the deploy host instead. Either way
> TLS terminates at the **proxy**: don't enable in-process `[server.tls]`/ACME on
> a deploy-managed app — deploy binds each slot to a private loopback HTTP port
> that the readiness gate and kamal-proxy target over plain HTTP, so a TLS
> listener there breaks its health checks. See the [TLS & HTTPS guide](./tls.md)
> for the full picture, including in-process TLS for self-run apps.

### Preconditions

`autumn deploy up` automates everything on the target: the only host prerequisite
is SSH access, and the only other prerequisite is on your own machine.

- **Key-based SSH access as `root` (or an equivalently privileged account) to a
  stock Ubuntu LTS (or other systemd) host.** The deploy runs non-interactively
  (`BatchMode=yes`) as the configured user (`[deploy] user`, default `root`) —
  no password prompts, so your SSH key must already be authorized on the target.
  `autumn deploy` runs its remote steps **directly over SSH, without `sudo`**: it
  writes units into `/etc/systemd/system/`, runs `systemctl`
  (`daemon-reload`/`enable`/`restart`/`disable`) and `systemd-run`, and writes
  under the app directory. The `[deploy] user` must therefore be `root` (the
  default) or an account that **already holds those permissions directly**. A
  plain non-root SSH login passes preflight (which only checks reachability and
  secrets, not privilege) and then fails once it tries to write the unit or
  invoke `systemctl`.
- **A local Rust toolchain** to build the release binary (`autumn build
  --embed`). This one is on *your* machine, not the target.

The reverse proxy **binary**, the reverse proxy service, install directories,
systemd units, release layout, and the secret env file are all created for you.

A host that still needs the reverse-proxy binary installed additionally needs
**apt** (host preparation is Debian/Ubuntu-only — on another systemd distro,
install `kamal-proxy` yourself and set `install_proxy = false`) and **outbound
HTTPS** to your distro mirrors and to Docker Hub. Once prepared, neither is
needed again. See [Host preparation](#host-preparation-install_proxy).

#### Host preparation (`install_proxy`)

`autumn deploy up` prepares the target itself. Before anything else runs it
probes the host for a working `kamal-proxy`, and:

- **a host that already has one** is left completely untouched — the probe is a
  read-only `kamal-proxy deploy --help`, and nothing else runs;
- **a host that has none** gets the pinned build installed at
  `/usr/local/bin/kamal-proxy`. kamal-proxy publishes no release binaries
  (upstream ships it as a container image), so the deploy installs the packages a
  minimal image may lack — `curl` (the readiness gate polls `/ready` *on the
  host*) and a container runtime — copies the binary out of the
  `basecamp/kamal-proxy` image **pinned by digest**, and moves it into place. Only
  genuinely missing packages are installed; the container runtime and the pulled
  image are left on the host afterwards. The step announces itself before it runs:

  ```
  host preparation: no kamal-proxy on this host — installing v0.9.2 at
  /usr/local/bin/kamal-proxy, from the pinned basecamp/kamal-proxy image. This also
  installs, and leaves behind, any of `curl` and a container runtime the host is
  missing. Decline with `[deploy] install_proxy = false`.
  ```

  The install ends by running the binary it just placed, so an install that
  "succeeded" without producing a working proxy fails the deploy there rather than
  at the first cutover. It also refuses outright if anything already exists at
  `/usr/local/bin/kamal-proxy`, so it can never replace a binary the probe merely
  failed to reach.

- **a host whose kamal-proxy responds but whose CLI surface has drifted** (a
  renamed or removed subcommand/flag) is *never* replaced — that binary may be
  shared with something else on the host. The deploy aborts with a message naming
  exactly what is missing and the version to pin, before touching live traffic.

If the install can't be done — no outbound network, no apt, Docker Hub rate
limits — the deploy fails fast, before anything is uploaded or cut over, with a
message naming what the host needs and the `install_proxy = false` opt-out.

To provision the proxy yourself (a pinned internal build, your own package, or a
host you don't want a container runtime on), decline host preparation:

```toml
[deploy]
install_proxy = false
```

A missing binary is then an actionable deploy failure instead of something the
deploy fixes.

### First deploy: from `autumn new` to a live app

```bash
# 1. Scaffold a project (its package name becomes the deploy app name).
autumn new myapp
cd myapp
```

Point the deploy at your server by adding a `[deploy]` section to `autumn.toml`:

```toml
[deploy]
host = "203.0.113.10"     # SSH-reachable IP or hostname of your VPS (required)
# hosts = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
#                         # a FLEET instead of one server (see "Rolling deploy
#                         # across a fleet"). Mutually exclusive with `host`;
#                         # the list order IS the rollout order.
# user = "root"           # SSH user (default: root)
# ssh_port = 22           # SSH port (default: 22)
# app_dir = "/srv/autumn/myapp"   # remote install dir (default: /srv/autumn/<app_name>)
# readiness_timeout_secs = 60     # /ready window before rollback (default: 60)
# keep_releases = 3               # prior releases retained for rollback (default: 3)
# install_proxy = false           # decline host preparation — you install
#                                 # kamal-proxy yourself (default: true)
```

Every key also has an environment override (`AUTUMN_DEPLOY__HOST`,
`AUTUMN_DEPLOY__HOSTS`, `AUTUMN_DEPLOY__USER`, `AUTUMN_DEPLOY__SSH_PORT`, …) if
you prefer to keep the host out of the file. `AUTUMN_DEPLOY__HOSTS` takes a
comma-separated list (`AUTUMN_DEPLOY__HOSTS=10.0.0.1,10.0.0.2`) and **replaces**
the whole `hosts` list from the file rather than appending to it. Entries are
trimmed and blank segments are dropped, so a trailing or doubled comma is
tolerated, and `AUTUMN_DEPLOY__HOSTS=` (empty) means *unset* — the same as
`AUTUMN_DEPLOY__HOST=` — rather than a blank fleet entry that would refuse every
deploy subcommand.

> **`host` and `hosts` are mutually exclusive.** Setting both is refused before
> anything runs — with both set the rollout order would be ambiguous. A blank
> entry or a repeated entry in `hosts` is refused too (deploying the same server
> twice would corrupt its previous-release chain). A one-entry `hosts` list is
> byte-for-byte the historical single-server deploy.

**Env wins over the file for *both* spellings.** Because the two are mutually
exclusive, setting one spelling in the environment now **clears the other from
`autumn.toml`**: a non-empty `AUTUMN_DEPLOY__HOSTS` retargets a `[deploy] host`
project as a fleet, and a non-empty `AUTUMN_DEPLOY__HOST` retargets a
`[deploy] hosts` project at a single server. Neither combination is a conflict —
it is the documented env-over-TOML precedence applied to the spelling you did
*not* set. Two rules bound it:

- **Setting both env spellings non-empty is still refused**, naming both keys.
  That is not a precedence question; with both set the rollout order is genuinely
  ambiguous, so it is treated as an operator error rather than tie-broken.
- **An empty or blank value still means *unset*** and leaves the TOML spelling
  alone. `AUTUMN_DEPLOY__HOST=`, a whitespace-only value, `AUTUMN_DEPLOY__HOSTS=`
  and `AUTUMN_DEPLOY__HOSTS=" , "` are the shape a CI or compose template emits
  for an unfilled slot, so they can never silently drop a fleet list configured
  in the file.

Put these two values in a project `.env` (git-ignored — never committed). The
deploy reads them and writes **only** them to a `0600` env file
(`app_dir/shared/autumn.env`) **on the server**:

```bash
# .env
AUTUMN_SECURITY__SIGNING_SECRET=<paste `openssl rand -hex 32` here>
# Only if your app is database-backed:
AUTUMN_DATABASE__URL=postgres://user:pass@db-host:5432/myapp_prod
```

> **The signing secret, the database URL, and the profile selector are
> deployed.** `autumn deploy` serializes exactly three variables to the host env
> file: `AUTUMN_SECURITY__SIGNING_SECRET`, — for database-backed apps —
> `AUTUMN_DATABASE__URL`, and `AUTUMN_ENV` (the deploy profile, `prod` by
> default — see below). **Any other runtime secret** (OAuth client secrets, SMTP
> credentials, a Redis URL, object-storage keys, etc.) placed in `.env` is
> **not** transferred to the server, so those features fail in production unless
> you provision the secret on the target host yourself. The env file is
> **rebuilt from scratch on every `deploy up`**, so hand-adding entries to
> `app_dir/shared/autumn.env` is not durable — they are overwritten on the next
> deploy; provision such secrets out of band on the host (or via the systemd
> unit environment) and re-apply them after each deploy.

> **`autumn deploy` runs the app under the production profile by default.** The
> deploy writes `AUTUMN_ENV=prod` into the host env file, so the deployed app
> boots under the `prod` profile and its production smart-defaults apply — strict
> CORS, minimal actuators, prod CSRF/session hardening, and strict
> signing-secret enforcement. To deploy a non-production target, set `[deploy]
> profile = "staging"` in `autumn.toml` (or `AUTUMN_DEPLOY__PROFILE=staging`);
> the deploy writes that value as `AUTUMN_ENV` instead.

> **Your `autumn.toml` is deployed alongside the binary.** `autumn deploy up`
> uploads your project's `autumn.toml` (and, when present, the profile sibling
> `autumn-<profile>.toml`) into the **per-release directory** — next to the
> binary it shipped with — and sets `AUTUMN_MANIFEST_DIR` to that release dir in
> the systemd unit, so at startup the app loads the same non-secret configuration
> you tested locally — auth, the jobs and scheduler backends, health/telemetry
> paths, CORS, signing-secret rotation
> (`security.signing_secret.previous_secrets`), and so on — instead of falling
> back to built-in defaults (fixes
> [#1952](https://github.com/autumn-foundation/autumn/issues/1952)). Coupling the config
> to the release (rather than a single shared dir) means a **rollback loads the
> rolled-back release's own config** — never the latest deploy's — and **removing
> a local override and redeploying no longer leaves a stale one loaded**, because
> a fresh release dir carries only the manifests uploaded that deploy. The
> profile sibling is picked the same way the host runtime picks it: the deploy
> normalizes the profile and uploads the first matching `autumn-<profile>.toml`
> (e.g. `profile = "Production"` uploads `autumn-production.toml`, matching the
> file the app loads first). The raw manifest is uploaded, not a flattened copy,
> so the app still applies its `[profile.<AUTUMN_ENV>]` overlay at runtime; the
> manifest is re-uploaded on every `deploy up` so the config on the server always
> matches the shipped binary. If no `autumn.toml` is found in the project
> directory, the deploy prints a loud warning and the app runs built-in defaults
> for all non-secret settings. `autumn deploy check` prints the same
> confirmation or warning **before** anything touches the server, so an
> operator relying on `check` as a preflight gate gets the same signal `up`
> would — not just after `up` actually runs.
> **Secrets never go in the manifest** — the manifest is owner-only (`0600`), so
> any inline config secrets are never exposed to other local accounts, while the
> signing secret, database URL, and `AUTUMN_ENV` continue to travel only
> in the `0600` host env file (`app_dir/shared/autumn.env`), which overrides the
> `autumn.toml` at load time.

Generate the signing secret once and store it somewhere durable:

```bash
openssl rand -hex 32
```

Run the preflight — it checks SSH reachability, the signing secret, the database
URL (when the app is DB-backed), and, via an offline scan of your local
`migrations/` directory, that every migration file is safe for a rolling deploy.
That scan does not consult the database, so it does not distinguish already-applied
from pending migrations — an old destructive migration file left in the directory
fails the check even after it has been applied. Keep `migrations/` rolling-safe
(remove or adjust an already-applied destructive migration if it trips the scan).
The preflight exits non-zero if anything fails, so nothing touches the server
until it is green.

With a fleet (`[deploy] hosts`), **every host the run targets is graded before
any host is touched**: SSH reachability is probed once per host and reported on
its own line (`ssh_reachability (10.0.0.3)`), while the project-wide graders —
signing secret, database URL, migrate-safety — run once. So an unreachable host
in position 3 is named before host 1 is deployed, not after two hosts have
already cut over. `autumn deploy up` and `autumn deploy rollback` re-run this
same fleet-wide preflight and abort on any failure.

```bash
autumn deploy check      # doctor --online runs the same graders (plain doctor skips network probes)
```

Build the single embedded binary, then deploy:

```bash
autumn build --embed     # produces target/release/myapp (assets + i18n baked in)
autumn deploy up
```

`autumn deploy up` re-runs the full preflight, aborts before any remote call if
it fails, then on this first run:

1. installs and supervises **kamal-proxy** on the public port
   (`server.port`, default `3000`),
2. creates the release + `shared` directories under `app_dir`,
3. uploads the binary into a timestamped release dir (`0755`),
4. writes the signing secret, database URL, and `AUTUMN_ENV` (the profile,
   `prod` by default) to `app_dir/shared/autumn.env` (`0600`, sourced by
   systemd — never printed, never on a command line; rebuilt each deploy),
5. writes the app's systemd unit (bound to a private `127.0.0.1` port) and
   points `current` at the release,
6. **runs any pending migrations** (a blocking `AUTUMN_MIGRATE=1` one-shot) —
   before the release is started, so it never boots against a schema that was
   never applied,
7. starts the unit,
8. health-gates on `GET /ready` within `readiness_timeout_secs`, then
9. routes the proxy at the freshly-ready release.

On success it prints:

```
✅ Deploy complete. Roll back with `autumn deploy rollback`.
```

Verify it is serving (the public port is your configured `server.port`):

```bash
curl http://203.0.113.10:3000/health   # -> {"status":"ok", ...}
curl http://203.0.113.10:3000/ready    # readiness probe used during cutover
```

### Migration ordering (first deploy included)

Pending migrations run **before the new version takes traffic**, on both paths:

| | when the migration runs | if it fails |
| --- | --- | --- |
| **First deploy** | after the binary, env file and unit are in place, **before the unit is started** | the deploy aborts and the half-written release is torn down — nothing is started, nothing is routed |
| **Redeploy** | after the candidate slot starts, **before the proxy flip** | the deploy aborts with the **old release still serving** — no flip, no drain, no promote |

A first deploy migrates before it *starts* the app rather than after, because
there is no live release to keep warm and an app booted against an unapplied
schema can crash-loop under systemd long before the readiness gate says anything
useful. Either way the migration is a blocking `systemd-run --wait` one-shot of
the release binary in `AUTUMN_MIGRATE=1` mode, run from the release directory
with the same `0600` env file the app uses, so its exit status gates everything
that follows. An app with no database support reports "nothing to migrate" and
exits 0, so this step is harmless for a DB-free app.

This is why a `autumn new` → `autumn build --embed` → `autumn deploy up` first
deploy needs no out-of-band `autumn migrate`.

### Zero-downtime redeploy

Ship a new version by re-running the exact same command:

```bash
autumn build --embed
autumn deploy up
```

On a host that is already serving, `up` performs a blue/green cutover with **no
dropped requests**:

1. stands the new release up on the **idle** loopback slot while the current
   release keeps serving,
2. runs **pending migrations on the host before cutover** (`AUTUMN_MIGRATE=1`
   one-shot) — a failed migration aborts here with the old version still live,
3. health-gates the candidate on `/ready` within the readiness window,
4. does an **atomic kamal-proxy upstream flip** old → new,
5. drains and stops the old release, then
6. prunes old releases, keeping the most recent `keep_releases` (default `3`)
   plus any rollback targets.

Because the run stops at the first failure, a bad migration or a candidate that
never reports `/ready` leaves the previous version serving and tears the
candidate down automatically — there is no half-deployed state serving traffic.

### Rolling deploy across a fleet

List more than one server under `[deploy] hosts` and the same `autumn deploy up`
becomes a **rolling deploy across the fleet** — no new command, no new flags
required:

```toml
[deploy]
hosts = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
```

```bash
autumn build --embed
autumn deploy up
```

**The list order is the rollout order.** Nothing sorts or regroups it: hosts are
replaced strictly front to back, **one at a time**, and each host must finish its
own cutover before the next host is started. Every host runs the *same*
zero-downtime blue/green sequence documented above, against its own kamal-proxy,
so a host being replaced never stops serving and the hosts behind it in the
queue are still on the old release — your load balancer always has healthy
upstreams. See the [fleet deploys guide](fleet-deploys.md) for the load-balancer
contract, the shared-state prerequisites, and the scale-up walkthrough.

**No host is ever drained for the rollout's sake.** The host being replaced is
not removed from your load-balancer pool and its `/ready` never goes `503`: the
candidate comes up on that host's idle loopback slot, is health-gated there, and
that host's own kamal-proxy flips upstreams atomically, with the old slot drained
and stopped only afterwards. `autumn deploy` never touches your load balancer's
membership — draining a host for a reboot or a scale-down stays your job.

The whole run mints **one release id**, so every host's `current` symlink
resolves to the same release and drift is meaningful.

Abridged output for a three-host fleet:

```
Rolling release 20260821T101500Z across 3 hosts, ONE AT A TIME, in `[deploy] hosts` order:
  1. 10.0.0.1 — zero-downtime redeploy
  2. 10.0.0.2 — zero-downtime redeploy
  3. 10.0.0.3 — zero-downtime redeploy
  → migrate (10.0.0.1 only — the schema is fleet-wide; 10.0.0.2, 10.0.0.3 skip it)

[1/3 10.0.0.1] deploying release 20260821T101500Z (zero-downtime redeploy)…
✅ [1/3 10.0.0.1] serving 20260821T101500Z
…
Fleet state:
  ✅ 10.0.0.1  serving 20260821T101500Z
  ✅ 10.0.0.2  serving 20260821T101500Z
  ✅ 10.0.0.3  serving 20260821T101500Z

✅ Fleet deploy complete — all 3 hosts serving 20260821T101500Z.
```

#### Migrations run exactly once

The schema is fleet-wide, so the rollout runs the pre-cutover migration on
**exactly one host**: the **first host in rollout order**, whatever its mode,
immediately before *its* cutover. Every other host builds with the migrate step
omitted. Because host 1 is the earliest point in the rollout, the schema always
moves before *any* host in the fleet takes traffic on the new release.

That holds for a brand-new fleet too: a first deploy runs its pending migrations
before it starts the release (see
[Migration ordering](#migration-ordering-first-deploy-included)), so an
all-first-deploy rollout migrates on host 1 exactly like an all-redeploy one. The
rollout header states where the migration lands:

```
  → migrate (10.0.0.1 only — the schema is fleet-wide; 10.0.0.2, 10.0.0.3 skip it)
```

Once the migrating host cuts over, the rollout says so:

```
⚠️  the schema has moved; from here an automatic rollback restores BINARIES
only — it never rolls a migration back
```

Design your migrations **expand/contract** so the old and new binaries can both
run against the migrated schema — that is what makes the automatic rollback below
safe. `autumn migrate check` (already part of the preflight) classifies your
local `migrations/` by rolling-deploy risk; the
[fleet deploys guide](fleet-deploys.md) covers the pattern in full.

##### The three schema notes on the `Fleet state:` summary

The same binaries-versus-schema truth is repeated once at the end of the run, on
the `Fleet state:` table described [below](#a-failure-halts-the-rollout-and-rolls-the-fleet-back).
The question being answered is never "did the fleet compensate" — it is **the
schema is at the new release; where are the binaries?** So the gate is the
migration: the summary says nothing at all unless this run actually scheduled a
migration *and* reached the host carrying it. (Every non-empty rollout schedules
one, so in practice the gate is whether host 1 was reached at all — a rollout that
failed before touching it moved no schema.) Beyond that gate the fleet's shape
only picks which sentence is true:

- **Some host is still forward.** At least one host is on the new release —
  deployed, degraded, left alone because its rollback target was in doubt, or one
  whose compensating rollback *failed*. You get:

  ```
  ⚠️  the schema has moved; from here an automatic rollback restores BINARIES
  only — it never rolls a migration back
  ```

  Forward-looking: it describes what a rollback you run *from here* will and will
  not undo. It is the same sentence the rollout printed mid-run, repeated because
  the run is ending with hosts still on the new release.

- **The fleet actually put hosts back.** Compensation *restored* a host to its
  previous release or *removed* a just-completed first deploy. You get:

  ```
  ⚠️  the compensating rollback restored BINARIES only — the migration that
  already ran was NOT rolled back; confirm the schema still fits the release now
  serving
  ```

  Past-tense: something already moved back underneath you. A compensation that
  only **failed** does not count here — that host is still serving the new
  release, so it lands in the first case instead, and claiming a rollback
  "restored binaries" would be false.

  These two are independent. A halt that compensated some hosts and left others
  forward (a `NOT rolled back automatically` row, or a compensation that failed)
  prints **both**.

- **Nothing forward, and nothing compensated.** No host is on the new release and
  the fleet never had anything to undo — because the migrating host itself failed
  at a step *after* `migrate` but *before* its cutover (a `readiness-gate`
  timeout is the ordinary shape) and tore its own candidate down, leaving every
  later host untouched. The table is then all "previous release still serving"
  rows with the database already moved on underneath them, so the summary says so
  explicitly:

  ```
  ⚠️  no host is serving the new release, but the migration that already ran was
  NOT rolled back — the binaries went back and the schema did not; confirm the
  release now serving still fits the migrated schema
  ```

  This is the case that is easiest to misread as a clean no-op. It is not one:
  the rollout aborted, but the migration stands.

The gate is deliberately conservative. The rollout cannot prove from a failing
step label whether the one-shot `migrate` itself succeeded, so it errs toward
"the schema moved" — one extra line of output is cheaper than an operator rolling
binaries back believing the schema came with them.

#### A failure halts the rollout and rolls the fleet back

A host that fails **stops the rollout**: the hosts behind it are never touched.
What happens to the hosts already on the new release depends on *where* the
failure landed relative to the **go-live step** — `proxy-flip` on a redeploy,
`proxy-route` on a first deploy:

| Where it failed | What the host is | What the fleet does |
|---|---|---|
| At or before go-live, on a redeploy | Previous release still serving (the candidate was torn down) | Already clean — nothing to undo |
| At or before go-live, on a first deploy | Nothing serving (the candidate was torn down) | Already clean — nothing to undo |
| After go-live, in housekeeping (`record-proxy-options`, `drain-old`, `prune`) | **Live and healthy on the new release** | **Warn and keep rolling** — the host is fine; only bookkeeping failed |
| After go-live, at `commit-markers` | Live on the new release, markers mid-transaction | Halt, and **never** auto-roll this host back — the rollback target cannot be trusted |
| After go-live, anything else | Live on the new release | Halt and compensate, this host included |

(The redeploy sequence is `start-candidate` → `migrate` → `readiness-gate` →
`proxy-flip` → `commit-markers` → `record-proxy-options` → `drain-old` →
`prune`, so a failed migration or a candidate that never reports `/ready` is
always a pre-go-live failure with the old release still serving.)

Compensation walks the hosts that are on the new release in **strict reverse
rollout order** (newest cutover first — that shrinks the mixed window from the
newest end) and, per host, either rolls it back to its previous release with the
same primitives `autumn deploy rollback` uses, or — for a host whose *first*
deploy had just completed — removes that first deploy so the host returns to
nothing-installed. It is best-effort-**continue**: a failed compensation on one
host never stops the next, and every result is named.

Two things compensation deliberately does **not** do:

- **It restores binaries, never schema.** A migration that already ran stays
  applied. The summary says so out loud — but only when compensation actually
  *restored* a host to its previous release or *removed* a just-completed first
  deploy. A compensation that merely **failed** leaves that host serving the new
  release, so the summary prints the forward-looking `the schema has moved …`
  note for it instead. Either way, confirm the schema still fits the release now
  serving; the three notes and when each appears are spelled out under
  [The three schema notes on the `Fleet state:` summary](#the-three-schema-notes-on-the-fleet-state-summary).
- **It does not touch a host whose rollback target is in doubt.** Markers left
  mid-transaction, a missing or unverifiable target release dir, or no recorded
  previous release each produce a `NOT rolled back automatically` row plus the
  exact read-only command to inspect that host's markers by hand (see
  [Troubleshooting](#troubleshooting)).

A **degraded** host — live and healthy on the new release, but whose post-cutover
housekeeping failed — is worth repairing promptly: a failed `record-proxy-options`
makes the **next** deploy of that host fail closed. A redeploy of that host
repairs it, and the run's final line says how many hosts finished degraded.

Every exit path — success, halt, or halt-plus-compensation — ends with the
per-host `Fleet state:` table, printed **after** any compensation so it describes
where the fleet actually is. On a halt it is your only source of truth about
which host is running what. It names every host currently on the new release
along with the levers for reversing them:

```
  On 20260821T101500Z: 10.0.0.2, 10.0.0.3. Roll ONE back with `autumn deploy
  rollback --only <host>`, or the whole fleet with `autumn deploy rollback`.
```

#### `--only`: the repair lever

```bash
autumn deploy up --only 10.0.0.3          # repeatable: --only a --only b
autumn deploy rollback --only 10.0.0.3
```

`--only` narrows `up` or `rollback` to a subset of `[deploy] hosts`. Each value
must appear in that list **verbatim** — nothing is prefix-matched or DNS-resolved,
so a typo can never deploy the wrong machine — and the selection keeps
*declaration* order regardless of the order you typed the flags, so `--only c
--only a` cannot invert a rolling deploy.

It is a **repair lever, not a faster deploy.** Whenever it excludes a configured
host the command says so before it starts:

```
⚠️  `--only` narrows this command to part of the fleet, so the hosts it skips
keep running whatever they are running now — THE FLEET MAY END UP MIXED. This is
a repair lever, not a faster deploy: finish with a full `autumn deploy up` (no
`--only`) so every host converges on one release (#1621).
   this run touches: 10.0.0.3
   left as they are: 10.0.0.1, 10.0.0.2
```

Finish with a full `autumn deploy up`.

#### `--no-rollback`: freeze a failed rollout

```bash
autumn deploy up --no-rollback
```

Halt and **freeze** instead of compensating: every host is left exactly as it is
— including the ones already on the new release — and named in the final state
table, so you can inspect the failure before anything else moves. Use it when you
would rather diagnose a half-rolled fleet than automatically reverse it.

#### Topologies a fleet deploy refuses

Three configurations are refused in the rollout prologue, **before a single
remote command runs**, because they cannot be deployed safely across more than
one host. Each is gated on the number of *configured* hosts, so `--only` does not
unlock them:

| Config | Why it is refused |
|---|---|
| `[database] url` is a `sqlite://` target | Every host receives the same database URL, which for SQLite means N independent database *files* — no shared schema, no lock to serialize migrations, and every host serving a different dataset behind your load balancer. Point `[database] url` at a Postgres server every host can reach. |
| `[media.mediamtx] enabled = true` | Host media provisioning has no teardown or rollback path; fanning it out would leave one MediaMTX daemon per host on identical ports with nothing to undo them with. Deploy media on a single host with `[deploy] host`. |
| `[deploy.tls] enabled = true` | The deploy-managed kamal-proxy runs on **every** host, so each would request a certificate for the same `[deploy.tls] host` from behind your load balancer; only one can answer a given ACME challenge and the rest burn Let's Encrypt rate limits. **Terminate TLS at the load balancer that fronts the fleet** and set `[deploy.tls] enabled = false`. |

One more fleet condition is a loud **warning** rather than a refusal:
`[database] auto_migrate` (or the `auto_migrate_in_production` alias) on a
multi-host deploy means every host applies migrations at boot, so the hosts race
each other during the rollout and a checksum mismatch exits the process under
`Restart=on-failure` — a crash loop mid-rollout. Turn it off for fleets and let
the rollout's single `migrate` step own the schema.

#### What fleet support changed for an existing single-host deploy

`[deploy] hosts = ["x"]` behaves exactly like `[deploy] host = "x"` — one entry
is one host, and the two spellings produce byte-identical output, uploads and
remote commands. What that guarantee does *not* mean is that a single-host deploy
is unchanged from the release before fleets landed. Several hardening changes
apply to **every** deploy, one host or five. In full, so an upgrade holds no
surprises:

- **A release-directory collision is now refused, and that is a new hard
  failure.** The release id has one-second granularity, so a fast re-run can
  reuse it. Re-uploading into a directory that `shared/previous-release` still
  points at would put the *new* binary there — a later rollback would roll
  *forward*. The deploy now probes for that directory first and refuses before
  writing anything, as it does when the probe cannot prove the directory is
  absent. **Concretely: a second `autumn deploy up` inside the same second now
  exits 1 with `release directory … already exists` where it previously went
  ahead.** Wait a second and re-run, or remove the directory if you are certain
  it is stale. The probe also costs one extra SSH round-trip per deploy, before
  anything is mutated.
- **SSH sessions are bounded.** Every `ssh`/`scp` invocation carries
  `ConnectTimeout=10`, `ServerAliveInterval=15` and `ServerAliveCountMax=4`
  alongside `BatchMode=yes`. The preflight is a TCP connect, which proves a host
  *accepts* a connection, not that its SSH daemon will ever answer; without these
  a wedged host would hang the deploy forever. For one server that is a stuck
  command someone cancels — for a fleet it would be a rollout frozen mid-flight
  with no error to compensate. Turning an infinite hang into a finite error is
  what lets the fleet halt and roll back. A host that accepts TCP and then wedges
  now fails the deploy instead of hanging it.
- **The maintenance flag file moved to `shared/`.** Every slot unit is now
  written with
  `Environment=AUTUMN_MAINTENANCE_FLAG_FILE={app_dir}/shared/autumn-maintenance.json`,
  so after the next deploy the app reads the flag from the shared directory
  instead of the release directory's `tmp/`. This fixes a real bug — a cutover
  used to orphan an active maintenance flag — but it is a behaviour change, and
  it means the **local** `autumn maintenance on`, run on a deploy-managed host,
  now writes a path the app no longer reads. Use `autumn deploy maintenance`
  there; see [Maintenance mode](maintenance-mode.md#where-the-flag-file-lives).
- **A `shared/last-deploy` marker appears.** The ops that complete a cutover
  (`commit-markers`, and `record-proxy-options` on a first deploy) now append an
  advisory shell fragment recording the action that completed plus the host's UTC
  time. It is what `deploy status` reads for its `last deploy` cell.
  The fragment can never fail the op it rides on, and it adds one small file
  under `shared/`; the op labels and everything else those ops do are unchanged.
  The first-deploy teardown writes it too, through one extra
  `teardown-last-deploy` op recording `torn down`, so a host whose first deploy
  was removed again can never keep reporting a successful one. That op runs only
  on the first-deploy failure and compensation path — a redeploy's candidate
  teardown leaves the marker alone, because the previous release is still serving
  and the marker still describes it.
- **The `detect-current` probe reads one more thing.** Its shell gained a
  delimited section that resolves the `current` symlink, so the deployed release
  id comes back in the same round-trip the deploy already made. Same op, same
  label, no extra round-trip; only the remote shell text differs.
- **The "no host configured" hint now mentions `hosts`.** A project with neither
  key set still fails with `no target host configured`, but the remediation hint
  gained the fleet spelling:

  ```
  no target host configured: Set `[deploy] host` in autumn.toml to your server's
  SSH-reachable address (or `[deploy] hosts = ["<address>", …]` to deploy a fleet)
  ```

  Stderr text only; the failure and its exit code are unchanged.

One further change is invisible on a single host and listed only for
completeness: a post-cutover failure is now wrapped in an error type that records
which step it landed on, so the fleet driver can decide whether that host may be
auto-rolled-back. Its `Display` delegates to the wrapped error verbatim, so the
single-host path prints byte-for-byte what it printed before.

`autumn deploy --help` was also rewritten, and `up`/`rollback` gained `--only`
and `--no-rollback`; no existing flag changed meaning.

> **Known limitation — a single-host deploy that fails after its migration ran
> says nothing about the schema (#2276).** On one host, a failure at any point is
> reported as the plain per-host error and the command returns right there: the
> single-host path deliberately keeps its pre-fleet output byte-for-byte, so it
> renders no `Fleet state:` summary and therefore none of
> [the three schema notes](#the-three-schema-notes-on-the-fleet-state-summary).
> If the failure landed *after* `migrate` but before the cutover — a
> `readiness-gate` timeout is the ordinary shape — the candidate is torn down and
> your previous release keeps serving, **against the already-migrated schema**,
> with nothing on screen saying so. The fleet path does warn in exactly this
> situation; the single-host path does not yet. This is tracked as
> [#2276](https://github.com/autumn-foundation/autumn/issues/2276) and is not fixed. Until
> it is: after any failed single-host `deploy up`, check `autumn migrate status`
> before assuming the failure left nothing behind — and write expand/contract
> migrations so the still-serving release fits the migrated schema either way.
>
> Since a **first** deploy migrates too
> ([Migration ordering](#migration-ordering-first-deploy-included)), this now has a
> second shape: a single-host *first* deploy that migrates and then fails its
> readiness gate tears the release down and leaves **nothing serving at all**
> against a schema that has already moved. The same advice applies, and more
> sharply — `autumn migrate status` is how you find out, and the fix for the next
> attempt is usually just re-running `autumn deploy up`, which is idempotent about
> an already-applied migration.

### Rollback

Roll back to the previous release on demand:

```bash
autumn deploy rollback
```

This resolves the previous release on the host, brings its slot back up, flips
the proxy back to it (health-gated on `/ready`), repoints `current`, and
re-probes `/ready`. It fails loudly and non-zero when there is no previous
release to return to.

**With `[deploy] hosts` this rolls back the whole fleet**, one host at a time, in
**reverse** declaration order (newest cutover first — the mirror of the rollout,
and of the automatic compensation above). It is best-effort-**continue**: a host
that fails does not stop the others, because stopping would leave *more* hosts on
the release you are trying to leave. Each host reports one of three outcomes:

| Outcome | Meaning |
|---|---|
| `previous release restored` | The host is serving its previous release again. |
| `SKIPPED — no previous release recorded on this host` | Nothing to roll back to (a first deploy clears the marker; a host just added to the fleet never had one). It keeps serving what it served before. |
| `NOT rolled back — failed at <step>` | The attempt failed; this host still serves what it served before. |

**The command exits non-zero unless *every* host rolled back** — a skip counts as
a failure here. That is deliberate: the contract of a fleet rollback is that the
*fleet* returns to its previous release, and a fleet where one host could not
follow the others is mixed. The per-host table distinguishes the two cases, which
is what your next move turns on; retry a single host with `autumn deploy rollback
--only <host>`, or deploy the intended release to it explicitly.

> **`--only` down to one host runs the single-host rollback path.** Everything
> above describes a rollback with more than one target. Narrow it to one and you
> get the pre-fleet behaviour instead: no per-host outcome table — just
> `Rolling back <host> to <release>…` and `✅ Rollback complete.`, or a plain
> error — and no benefit from the reachability softening described below, which
> exists so that the *rest* of a fleet can still move past a dead host. With one
> target there is no rest: a host that does not answer stops the command non-zero
> having changed nothing. Worth knowing before an incident, because a stranded
> host is exactly when this command gets reached for. Follow it with
> `autumn deploy status` for the fleet-wide picture.

Like the automatic compensation, a fleet rollback **restores binaries only** — no
migration is rolled back. Confirm the schema still fits the release now serving.

> **Rollback runs the same local preflight as `deploy up` first.** Before it
> makes any remote call — before it even resolves the previous release on the
> host — `autumn deploy rollback` runs the identical local preflight graders
> (signing secret, database URL, migrate-safety, and — for a fleet — SSH
> reachability for every host it targets) and aborts non-zero if any fail.
>
> One class is deliberately downgraded, and only on a multi-host rollback: a
> per-host `ssh_reachability` failure is reported and the rollback continues
> without that host, which is then named as `NOT rolled back` and still makes the
> command exit non-zero. Refusing to move the healthy majority off a bad release
> because one host is dead would strand *more* hosts on the release you are
> abandoning. Every other grader keeps the hard gate, and a single-host rollback
> — including a fleet narrowed with `--only` to one host — keeps it for
> reachability too.
>
> So it needs the same local inputs as a deploy — your project's `.env`/signing
> secret and database URL, and the `migrations/` dir — available **wherever you
> invoke it**: an emergency rollback from a bare CI checkout or a machine without
> the project's secrets fails preflight before it ever reaches the host. Keep the
> deploy inputs available where you would run a rollback.

### Fleet status

```bash
autumn deploy status
autumn deploy status --json
autumn deploy status --strict
```

`deploy status` probes every configured host **read-only** and prints one row per
host, in `[deploy] hosts` order. It mutates nothing, so it is safe mid-incident;
an unreachable host becomes a row, never an abort. It works for a single
`[deploy] host` too (one row).

Each row carries: mode (`deployed` / `not deployed` / `unreachable` /
`unknown`), the deployed release (read from the host's `current` symlink), the
live slot, the `/ready` status code, the maintenance flag
(`maintenance ON` / `maintenance off` / `maintenance ?`), the proxy's bound
port, that host's last deploy result, and any per-host drift reasons.

> **The maintenance cell reports the flag file that host's *running* slot unit
> polls.** It is resolved on the host from the live slot unit's
> `Environment=AUTUMN_MAINTENANCE_FLAG_FILE=…`, falling back to that unit's
> `WorkingDirectory` plus the legacy relative `tmp/autumn-maintenance.json` when
> the unit carries no override — the same rule the runtime itself applies. It is
> deliberately *not* a fixed read of the shared path, which would report `off`
> for a maintained host whose unit polls somewhere else, and `ON` for a host
> still taking traffic.
>
> Two consequences. First, **a host deployed before this feature reports its
> release-local flag** — its unit carries no `AUTUMN_MAINTENANCE_FLAG_FILE`, so
> the cell tells the truth about that host while also raising the "polls a
> release-local maintenance flag file" drift reason below; redeploy it to move
> it onto the shared path. Second, when the live slot unit cannot be read at all
> the cell reads `maintenance ?` rather than guessing — "we could not tell" must
> never render as a confident `off`.
>
> It reports which file the running unit polls and whether that file exists. It
> is not a guarantee about the app's in-memory state: the running process picks
> the change up on its own 500 ms poll.

> **`last deploy` is the last action that host *completed*.** It reads
> `last deploy: deployed <UTC time>`, `last deploy: rolled back <UTC time>` or
> `last deploy: torn down <UTC time>` —
> the host's own `date -u` clock, from a marker written by the ops that complete
> a cutover. A deploy that failed *before* the cutover boundary never rewrites
> it, so the host still shows its previous action: this is a per-host fact, not
> a verdict on the last fleet rollout (the rollout itself exits non-zero and
> prints its own per-host outcome table). A `rolled back` host was rolled back
> by hand or compensated after a halted rollout. A `torn down` host had its
> first deploy removed again by a halted rollout's compensation — nothing is
> installed on it, and the row's `not deployed` mode says so too. The cell reads
> `last deploy: ?` when the marker is absent or unreadable — a host that has
> never completed a cutover, or one last deployed by a CLI that predates the
> marker — because "we could not tell" must never render as a result. It is
> **reported, never counted as drift**.

Two kinds of drift are reported, and they are deliberately separate:

- **Version drift** — more than one *distinct known* release across the fleet.
  The remedy is `autumn deploy up` to converge, or `autumn deploy rollback`.
- **State drift** — per-host marker damage that will make a **future** deploy of
  that host fail closed or take the wrong slot. Today's reasons are: the
  `live-slot` marker disagrees with the slot kamal-proxy is actually serving (the
  next redeploy would restart the *serving* slot); the `shared/proxy-options`
  marker is unreadable (the next deploy of that host will refuse); the installed
  proxy unit binds a different public port than `[server] port` configures; no
  release is deployed on this host while the rest of the fleet is serving one;
  the host has a `current` symlink but the release behind it could not be read;
  and the two maintenance-probe reasons —

  - `the live slot unit could not be read, so which maintenance flag file this
    host's app polls is unknown — the maintenance column reports ? rather than
    guessing`, and
  - `this host's app polls a release-local maintenance flag file, not the shared
    one (its slot unit predates AUTUMN_MAINTENANCE_FLAG_FILE) — redeploy it so
    maintenance survives cutovers`.

  The remedy for the second is exactly what it says: **redeploy that host**, so
  its slot unit is rewritten with the shared flag path and its maintenance flag
  survives the next cutover.

> **"Release unknown" is never *version* drift — but a broken `current` symlink
> is *state* drift.** The only release identity a host can be asked for is its
> `current` symlink, and that can be missing on a never-deployed host, dangling
> mid-incident, or unreadable because the host did not answer. Those hosts are
> *named* in the report and explicitly excluded from the version-drift verdict —
> reporting a mixed fleet that does not exist is worse than reporting nothing.
>
> A reachable host that *proved* it has a `current` symlink and still resolves to
> no readable release is a different case, and it is now **state drift** that
> `--strict` exits non-zero on (it was previously reported without counting).
> That host claims to serve a release nobody can name, and deploying it next
> would copy the unresolvable target into `previous-release` — making a later
> rollback refuse. Repair it before the next deploy.

`--strict` exits non-zero when **any** drift is found, so drift is alertable from
cron. The default exits `0`: status is a report, not a judgement. `--json` emits
a stable machine-readable report on stdout — `hosts[]` (with `host`, `reachable`,
`mode`, `release`, `live_slot`, `ready`, `maintenance`, `proxy_port`,
`last_deploy`, `drift[]`), plus `version_drift`, `state_drift[]`, and `drifted`
(the `--strict` condition).

`maintenance` is **three-valued**: `true` and `false` are proved states of the
file the host's *running* slot unit polls, and `null` means the CLI could not
prove which file that is — the same condition the table renders as
`maintenance ?`. Both `false` and
`null` are falsy, so an existing `maintenance == true` check is unaffected — no
consumer that tests the field for truth starts matching more hosts.

`last_deploy` is `{"result", "at"}`, or `null` when the host reports no readable
marker. `result` is `"deployed"`, `"rolled back"` or `"torn down"`; `at` is the
host's own UTC timestamp (`null` for a marker written before the timestamp field
existed). It records the host's last **completed** action, so a deploy that
failed before cutover never rewrites it, and it is reported rather than counted
as drift.

> **`deploy status` still runs when the app config does not validate.** It needs
> exactly one value from your application config — `[server] port`, the public
> port kamal-proxy binds and every loopback slot port is derived from — so an
> unrelated invalid `[scheduler]`, `[mail]`, `[database]` or `[security]` setting
> used to abort the fleet's only read-only incident command before a single host
> was probed. It no longer does. When the config fails to validate under the
> deploy profile, `status` prints a caveat **on stderr** (in both text and
> `--json` mode, so the `--json` shape on stdout is untouched) naming the config
> error and the declared port it is probing against, then reports the fleet. The
> fallback port is your own declared `[server] port` for that profile, read from
> the same TOML + `.env.<profile>`/env layers the loader would have used, just
> without validation (or the framework default when no layer declares one) —
> never a guess.
>
> **`deploy check`, `deploy up` and `deploy rollback` still refuse**, and that
> asymmetry is deliberate: they grade and upload runtime *values* (the signing
> secret, the database URL), so an invalid config must stop them. `status` only
> reads. The caveat says so too.

### Fleet maintenance

```bash
autumn deploy maintenance on --message "Upgrading database schema"
autumn deploy maintenance on --readonly --allow-ips 10.0.0.0/8
autumn deploy maintenance off
```

`autumn deploy maintenance` turns [maintenance mode](maintenance-mode.md) on or
off on **every deploy-configured host over SSH**, writing the same JSON flag file
the local `autumn maintenance on` writes and taking the same flags
(`--message`, `--allow-ips`, `--readonly`, `--bypass-header`). The running apps
react within their 500 ms poll interval — no restart, no deploy.

The distinction from the top-level `autumn maintenance` matters: that command
writes **this machine's own working directory**, which is useless for a host you
deploy to over SSH. Deploy-managed hosts get their flag file at
`{app_dir}/shared/autumn-maintenance.json` — a path in the shared directory that
survives cutovers, rollbacks and pruning and is visible to *both* blue and green
slots — because `autumn deploy` stamps `AUTUMN_MAINTENANCE_FLAG_FILE` into every
slot unit it writes. The shared path is written **first**, because it is the
authoritative one: a host running a current slot unit reacts within its next
500 ms poll even if anything after it fails.

For a host still running a unit deployed *before* that override existed,
`maintenance on` then also writes the release-relative file that unit polls —
resolved from the host's **live slot unit**, the slot the proxy is actually
serving, and never from the `current` symlink (which is rewritten only after the
proxy flip, so the two disagree exactly when a flip landed and the marker commit
did not). `autumn deploy status` names those hosts as state drift — "polls a
release-local maintenance flag file, not the shared one" — because their flag is
orphaned by the next cutover until they are redeployed.

The fan-out is **best-effort-and-aggregate**, deliberately the opposite of the
rollout: every host is attempted, the per-host table names what changed, and the
command exits non-zero if any host failed. Hosts that *did* change are **not**
reversed automatically — reversing them would push users back into the very
window you are closing — so the summary names them explicitly (the
"Changed anyway: …" line, fully-changed hosts only), and reversing by hand is
your call.

Two per-host rows are worth recognising:

- `maintenance enabled (shared flag only — no release is promoted on this host,
  so no running unit polls a release-local flag)` — a **success**. Nothing is
  promoted, so no unit is running and the shared write was the whole job.
- `PARTIAL — shared flag written, but the file this host's RUNNING unit polls was
  NOT (failed at …), so this host may still be serving traffic` — a **failure**,
  and the command exits non-zero on it. The live slot unit could not be read (or
  the write to its file failed), so which file the app polls was never proved.
  `on` will not claim a host is maintained it could not prove, and `off` will not
  claim to have released one (there the row ends `so this host may still be in
  maintenance`).

Like `deploy status`, `deploy maintenance` **still runs when the app config does
not validate** — a maintenance window gets closed mid-incident, and an unrelated
invalid setting must not stand in the way. It prints the same caveat on stderr
naming the config error, and continues against the declared `[server] port` for
that profile read without validation; it uses that port only to identify which
slot unit each host is running. `autumn deploy check`/`up` still refuse until the
config is fixed.

> **Maintenance mode does not drain a host from your load balancer.** `/ready`
> stays `200` while maintenance is on, by design: gating it would eject every
> host from the pool the moment maintenance was enabled. A maintained host keeps
> taking traffic and answers it with `503` + `Retry-After`. Drain at the load
> balancer if you need a host out of rotation. This is why `deploy status`
> reports readiness and maintenance in separate columns.

### Inspect the plan without touching the server

```bash
autumn deploy plan
```

`plan` is a pure dry-run: it prints a **representative** systemd unit and the
ordered zero-downtime rollout steps for review, without connecting to the host.
The printed unit is illustrative — it mirrors the shape of what gets installed
(`User`, working directory, `EnvironmentFile`, restart policy) but is **not**
byte-for-byte what a real `deploy up` writes. Live deploys write **slot-specific**
units named `{service}-blue.service` / `{service}-green.service`, each pinned to
its own release directory and bound to a private `127.0.0.1` port, so the blue
and green slots can run side by side for the health-gated cutover while the proxy
owns the public port.

The step **order** is illustrative too: the list shows the overall rollout shape
for review, but its exact order can differ from a live `deploy up` — for example,
`plan` lists the migration step before starting the candidate, whereas a real
redeploy starts the candidate first and then runs the pre-cutover migration. Use
`plan` to review the shape and what happens, not as a byte-exact order or unit
reference.

With `[deploy] hosts` set, `plan` adds a **fleet rollout** section listing the
hosts in rollout order and stating the migrate-placement rule. That section is
descriptive for the same reason: `plan` contacts no host, so it cannot know which
hosts are first deploys and which are redeploys. It therefore renders the
migration as the *rule* `autumn deploy up` applies after probing every host —
"`[migrate]` runs once, on the first host in rollout order, before its
cutover — hosts 2..N skip it" — never as a named host. `deploy up` names the
actual host once it has probed the fleet.

### Where secrets live

The signing secret and database URL are written **only** to
`app_dir/shared/autumn.env` on the target, with mode `0600`, and are sourced by
the systemd unit via `EnvironmentFile`. They are never inlined into the
world-readable unit, never placed on a command line, and never printed to logs
or error messages.

### Where a SQLite data file lives

On the [SQLite tier](./sqlite-in-production.md) the database is a **file**, and a
file inside a release directory does not survive the next deploy: each release
gets its own directory, a slot unit's `WorkingDirectory` is that directory, and
release retention deletes old ones. So `autumn deploy` treats the data file as
persistent state:

| Configured `[database] url` | What the deploy does |
| --- | --- |
| Relative — `sqlite://app.db` | Keeps the real file at `app_dir/shared/data/app.db` and links each release at `app.db`. |
| Absolute outside the releases dir — `sqlite:///var/lib/myapp/app.db` | Leaves it exactly there; it is already release-independent. |
| Absolute inside the app dir but outside `shared/` (`releases/…`, `current/…`) | Refused at preflight — retention would delete it. |
| Relative but not a plain name — `sqlite://../x.db`, `sqlite://.` | Refused at preflight — it does not name a file inside the release dir. |
| In-memory — `sqlite::memory:` | Refused at preflight — it does not survive a restart, let alone a deploy. |

`shared/` is created by the deploy's `prepare-dirs` step, is never pruned, and is
seen by both blue/green slots, so the file a release writes is the file the next
release reads. SQLite follows the symlink when it names the `-wal`, `-shm` and
`-journal` sidecars, so those land beside the shared file too. The link is created
**before** the migration one-shot, so migrations and the app always open the same
database. `deploy rollback` re-links its target before it starts it, because a
release deployed before adoption holds no file at that path.

**Upgrading an app deployed before this contract existed.** If the data file is
still a real file in the currently serving release, the next `deploy up` **stops
and tells you what to run**. It does not move the file for you, and that is
deliberate: SQLite derives the `-wal` name from the path a connection resolved,
so a connection opened before a move and one opened after would use two different
write-ahead logs for one database — and there is no way to move a file and
create the link in its place atomically, so a pooled connection opening in that
window creates an empty database at the old path.

The one-time migration, on the host (the deploy prints these paths for you):

```sh
autumn db backup                      # first, from the project dir
systemctl stop myapp-blue.service myapp-green.service
mv /srv/autumn/myapp/current/app.db* /srv/autumn/myapp/shared/data/
```

Then re-run `autumn deploy up`. From that point on nothing is ever relocated: the
file stays in `shared/data` and each release is linked at it.

**If you already hand-symlinked the data file** at a database you keep elsewhere,
the deploy stops on that too, and prints a different one-time migration — `mv` on
a symlink moves the link, not the database behind it:

```sh
autumn db backup                      # first, from the project dir
systemctl stop myapp-blue.service myapp-green.service
mv "$(readlink -f /srv/autumn/myapp/current/app.db)"* /srv/autumn/myapp/shared/data/
rm -f /srv/autumn/myapp/current/app.db
```

The deploy refuses rather than link past your symlink: the shared file does not
exist yet, so the migration would create an empty database there and the cutover
would serve it while yours sat untouched at the old path.

The deploy never deletes a database file. A real file it finds at the link path
in some other release — a rollback target from before the migration — is set
aside as `shared/data/<file>.superseded`, out of retention's reach; a deploy that
would overwrite an existing one refuses instead.

Back it up with [`autumn db backup`](./daemon.md#database-backups), which takes an
online-safe snapshot of the file with no external tools.

### Troubleshooting

- **Preflight is failing.** Run `autumn deploy check` (or `autumn doctor --online`)
  — each failing grader prints the exact problem and a one-line fix (missing
  `[deploy] host`, unreachable SSH port, missing/weak signing secret, missing
  writable database URL, or an unsafe migration in your local `migrations/`
  directory). `autumn deploy check`
  always probes SSH reachability; plain `autumn doctor` skips network probes, so
  use `autumn doctor --online` to include the SSH-reachability grader.
- **`release binary not found …`** — run `autumn build --embed` first; `deploy
  up` uploads the pre-built binary, it does not build for you.
- **Nothing answers on the public port** — confirm the app's `server.port`
  matches the port you are curling, and that the host firewall allows it.
- **A fleet rollout halted part-way.** Read the `Fleet state:` table it printed —
  it describes where the fleet actually *is*, after any automatic compensation.
  Then run `autumn deploy status` for an independent read-only view. To converge,
  either re-run `autumn deploy up` (fix the cause first) or `autumn deploy
  rollback` to take the whole fleet back. Remember that a migration which already
  ran is still applied.
- **One host is stranded on the new release.** Roll just that host back with
  `autumn deploy rollback --only <host>`, then finish with a full `autumn deploy
  up` (no `--only`) so the fleet converges on one release. `autumn deploy status
  --strict` exits non-zero while the fleet is still mixed, which makes a good
  gate for the "am I done?" question.
- **A host says `NOT rolled back automatically`.** The fleet declined to touch it
  because its rollback *target* is in doubt — release markers left
  mid-transaction by `commit-markers`, a missing or unverifiable target release
  dir, or no previous release recorded at all. Deliberately, the guidance is
  **not** "run `deploy rollback --only <host>`": that command trusts the very
  target that is in question. Read the markers first — the deploy prints the exact
  read-only command for that host:

  ```bash
  ssh root@10.0.0.2 'cat /srv/autumn/myapp/shared/previous-release /srv/autumn/myapp/shared/live-slot; ls /srv/autumn/myapp/releases'
  ```

  The `previous-release` marker names the release dir, slot and port the host
  should return to. Restore it by hand before deploying the fleet again.
- **A host finished `DEGRADED`.** It is live and healthy on the new release; only
  post-cutover bookkeeping failed. Repair it before the next deploy — a redeploy
  of that host does — because a failed `record-proxy-options` makes the next
  deploy of that host fail closed.
- **`release directory … already exists`** — the one-second release id was reused
  by a fast re-run. Wait a second and re-run `autumn deploy up`, or remove that
  directory if you are certain it is stale.

### Limitations and known gaps

- **Only the signing secret, database URL, and profile selector are written to
  the host env file.** `autumn deploy` serializes just
  `AUTUMN_SECURITY__SIGNING_SECRET`, (for database-backed apps)
  `AUTUMN_DATABASE__URL`, and `AUTUMN_ENV` (the deploy profile, `prod` by
  default); any other runtime secret (OAuth/SMTP/Redis/storage/etc.) must be
  provisioned on the target separately. The file is rebuilt on every `deploy
  up`, so hand-added entries do not persist.
- **Your project's `autumn.toml` is deployed**
  ([#1952](https://github.com/autumn-foundation/autumn/issues/1952)). `autumn deploy up`
  uploads your `autumn.toml` (and the profile sibling `autumn-<profile>.toml`
  when present) into the **per-release directory** at mode `0600` (owner-only, so
  secrets are never exposed to other local accounts) and sets
  `AUTUMN_MANIFEST_DIR` to that release dir in the systemd unit, so the app loads
  the same non-secret configuration (auth, jobs and scheduler backends,
  health/telemetry paths, CORS, signing-secret rotation `previous_secrets`, etc.)
  you tested locally rather than falling back to built-in defaults. Because the
  manifest is coupled to the release, a **rollback loads the rolled-back
  release's own config** and **removing a local override then redeploying doesn't
  leave a stale one lingering**. The profile sibling is normalized and chosen
  exactly as the host runtime chooses it (e.g. `profile = "Production"` uploads
  `autumn-production.toml`). The manifest is re-uploaded on every `deploy up`;
  secrets never go in it — they stay in the `0600` host env file, which overrides
  `autumn.toml` at load time. When no `autumn.toml` is found locally the deploy
  prints a loud warning and the app runs built-in defaults.
- **One load balancer, many app hosts — but the load balancer is yours.**
  `[deploy] hosts` rolls a release across as many app servers as you list, but
  `autumn deploy` provisions no load balancer and performs no LB membership
  changes: kamal-proxy is per host and binds that host's public port. Run your
  load balancer on a separate host in front of the fleet, health-check `/ready`,
  and terminate TLS there (a fleet deploy refuses `[deploy.tls]` for exactly this
  reason). See the [fleet deploys guide](fleet-deploys.md) for the contract, and
  [Multi-replica setup](#multi-replica-setup) for the shared state every replica
  needs.
- **Fleet rollback restores binaries, never schema.** Neither the automatic
  compensation nor `autumn deploy rollback` runs a `migrate down` — an unattended
  down-migration mid-flip would run exactly the SQL nothing reviews. Use
  expand/contract migrations so a rolled-back binary still fits the migrated
  schema.
- **A failed *single-host* deploy never warns that the schema moved** (#2276) —
  including a failed *first* deploy, which since #1607 migrates before it starts
  the release, and so can leave a moved schema with nothing serving.
  A fleet ends every run with a `Fleet state:` summary that names the
  binaries-versus-schema state; the single-host path returns the per-host error
  directly and renders no summary, so a failure after `migrate` but before the
  cutover leaves the previous release serving against the migrated schema with
  nothing saying so. See
  [What fleet support changed for an existing single-host deploy](#what-fleet-support-changed-for-an-existing-single-host-deploy).
- **A compensated first deploy leaves its proxy route behind.** When the fleet
  removes a host's just-completed *first* deploy, that host's kamal-proxy still
  holds a route pointing at the (now stopped) slot, so its public port answers
  `502` instead of refusing the connection until the host is deployed again. The
  state table names the host so this is never a surprise.
- **Host identity is compared literally.** Duplicate `[deploy] hosts` entries are
  refused after trimming, but two DNS names for the same machine are not detected
  — the same limitation `autumn migrate` has for duplicate target URLs.
- **Remote state is updated step-by-step, not transactionally** (#1938). The
  `current` symlink and the live/previous-release markers are written by
  individual SSH commands, so an interrupted deploy can leave state mid-flight;
  a subsequent `autumn deploy up` or `autumn deploy rollback` is the intended
  recovery. To keep the most damaging drift self-correcting, each `autumn deploy
  up` **reconciles the `live-slot` marker against the live proxy at deploy-start**:
  the same probe that decides first-vs-redeploy also reads `kamal-proxy list`, and
  when the proxy is unambiguously serving a slot the marker disagrees with (a
  stale marker from an interrupted previous run), the deploy treats the proxy as
  authoritative — it plans the cutover onto the genuinely idle slot (so it never
  restarts the live one), warns loudly about the disagreement, and repairs the
  marker on disk. If the proxy signal is absent or unclear it falls back to the
  marker exactly as before, so the reconcile never changes a healthy deploy.

### MediaMTX host provisioning (`[media]`)

An app that uses the [autumn-media](../../autumn-media-plugin/README.md) plugin
(RTMP/WHIP ingest, HLS/WebRTC playback, recording) can have `autumn deploy`
provision **MediaMTX** as a host **systemd unit** on the same box — exactly as it
already provisions kamal-proxy (#2051). It is **opt-in and disabled by default**,
so a non-media project is byte-for-byte unaffected.

Enable it in `autumn.toml` (or a profile / `autumn-<profile>.toml` layer):

```toml
[media.mediamtx]
enabled = true                 # off by default; the controller is a no-op when false
# recordings_dir = "/var/lib/mediamtx/recordings"
# record_delete_after = "72h"  # MediaMTX recordDeleteAfter retention window
# webrtc_additional_hosts = ["my-app-mediamtx.example.com"]  # extra WebRTC ICE hosts
# The listen ports below default to MediaMTX's standard values; override only if
# you also change the app-side *_base URLs to match.
# api_port = 9997        # control API
# rtmp_port = 1935       # RTMP ingest only (OBS / RTMP encoders)
# hls_port = 8888        # HLS playback
# webrtc_port = 8889     # WebRTC: WHIP publish (browser + room) AND WHEP/WebRTC playback
# playback_port = 9996   # recording playback
# webrtc_local_udp = 8189
# config_path = "/etc/mediamtx/mediamtx.yml"   # where the rendered config is written
# binary_path = "/usr/local/bin/mediamtx"      # host bootstrap installs it; deploy does not download it
# unit_name = "mediamtx"                        # systemd unit name (no .service suffix)

[media.ffmpeg]
# bin = "/usr/bin/ffmpeg"  # concrete path; verified by the deploy-time FFmpeg preflight
```

When `enabled = true`, `autumn deploy up`:

1. Runs **four fail-closed host preflight checks before touching the host** —
   FFmpeg resolves (the concrete `[media.ffmpeg] bin`), the MediaMTX binary is
   executable, the recordings directory is writable, and the MediaMTX ports are
   free — plus a pure-config precheck that the configured MediaMTX listener ports
   are distinct, and **aborts the deploy** if the host cannot serve media, rather
   than shipping a half-provisioned box. One caveat on the FFmpeg check: only a
   **concrete literal** `[media.ffmpeg] bin` is probed and fail-closed here; an
   env/interpolation-indirected path (an empty value, or one carrying a `${...}`
   placeholder such as `${AUTUMN_MEDIA__FFMPEG__BIN}`) is resolved by the deployed
   service from its own environment, so it is **deferred to runtime** — surfaced as
   a non-blocking warning that does **not** abort the deploy. These checks require a
   live host executor and run **only at `deploy up`**.
2. After the app cutover succeeds, renders `mediamtx.yml` (LL-HLS window, fmp4
   recording under `recordings_dir`, WebRTC config, and a `~^room/.+$` path
   matcher for autumn-media Rooms) plus the systemd unit, then runs
   `daemon-reload && enable --now && restart`.

`autumn deploy plan` is a pure dry-run: it surfaces the media unit, its
provisioning steps, the names of the host preflight checks that **will** run at
`deploy up`, and the CSP origins your app must allow — but it holds no host
executor, so it **does not** probe MediaMTX/FFmpeg or validate ports remotely. The three browser-facing MediaMTX origins
(WebRTC `:8889`, HLS `:8888`, playback `:9996`) must appear in your
`connect-src` / `media-src` (and `frame-src` for WebRTC) CSP; in production they
collapse to your public MediaMTX origin, and your object-store origin must also
be allowed in `media-src` for recorded playback.

> **`strict_config` interaction.** The `[media]` table is **plugin-owned** — it
> is not part of autumn-web's `AutumnConfig` schema. `autumn deploy` reads the
> `[media.mediamtx]` / `[media.ffmpeg]` **subtree** straight from the merged
> `autumn.toml` (base ← inline `[profile.<name>]` ← `autumn-<profile>.toml`), so
> that media-subtree read never itself routes through the strict schema. But that
> does **not** make strict config deploy-safe. Before it ever reads the raw
> `[media]` subtree, `deploy::run` calls `AutumnConfig::load()` — the strict
> loader (`autumn-cli/src/deploy.rs`, ahead of `load_media_host_config`) — for
> **every** subcommand. So if you turn on autumn-web's strict config validation
> (`[server] strict_config = true`, or `AUTUMN_SERVER__STRICT_CONFIG=1`) **and**
> keep a top-level `[media]` table in that strict-loaded config, the `[media]`
> table is flagged as an **unknown top-level key** and **hard-fails** during that
> load (unknown top-level keys were already strict pre-#1890, so this fails even
> without `strict_config_enforce_all`). That means **both**:
>
> - the **app runtime fails to boot**, *and*
> - **`autumn deploy plan` / `deploy up` also exit during config load** on the
>   unknown `[media]` key — they never reach `load_media_host_config` and never
>   provision MediaMTX, because the strict `AutumnConfig::load()` runs first.
>
> **Workaround:** treat `strict_config` and a top-level `[media]` table as
> mutually exclusive today — don't enable `strict_config` while the strict-loaded
> config carries a top-level `[media]` table (the validator has no knowledge of
> the plugin's `[media]` section, on either the app-boot or the deploy path).

**Deferred (host-bootstrap prerequisites, not done by `autumn deploy`):**
installing/pinning the MediaMTX binary itself (like the kamal-proxy binary, it is
a host-bootstrap step); **creating and permissioning the `recordings_dir`**
(default `/recordings`) so the media user can write to it — `autumn deploy` only
`mkdir`s the MediaMTX config file's parent directory, so the fail-closed
recordings-dir preflight (`test -d && test -w`) aborts `deploy up` when the
directory is missing or not writable; and wiring the four host preflight checks
into the offline `autumn doctor` CLI (they run only in the executor-holding
`deploy up` path today; `deploy plan` names them but never executes them).

### How the deploy path is validated in CI

Two layers exercise the real `autumn deploy` lifecycle over real ssh/scp +
systemd + kamal-proxy — nothing is mocked:

- **Container e2e (every CI run):** `autumn-cli/tests/deploy_e2e.rs` drives first
  deploy, zero-downtime redeploy, on-demand rollback, and forced-failure
  auto-rollback against a privileged systemd+sshd container. The same binary also
  drives a **two-host fleet lifecycle** across two containers — rolling both
  hosts onto one release, counting the migrate one-shot on each host to prove it
  ran on exactly one, halting a rollout mid-fleet (the untouched host stays on
  its old release), auto-rolling the already-cut-over host back, and asserting
  `deploy status --json` / `--strict` — so the multi-host claims on this page are
  covered by the same real ssh/scp + systemd + kamal-proxy harness, not by mocks.
  A container cannot power-cycle, prep a stock host from scratch, or reproduce a
  real pam_systemd session, so those are deferred to the real-VPS job below.
- **Real-VPS validation (opt-in):** the
  [`Deploy real-VPS validation`](../../.github/workflows/deploy-real-vps.yml)
  GitHub Actions workflow provisions a throwaway Hetzner Cloud VM from a **stock
  Ubuntu image** and runs the same lifecycle assertions plus the four
  VM-only checks: a **real kernel reboot** (app + kamal-proxy come back serving),
  **bare-host prep from scratch**, the **`<15 min` onboarding wall-clock metric**,
  and **kamal-proxy control-socket fidelity under real pam_systemd**
  (`XDG_RUNTIME_DIR=/run/user/0`). It is **manually triggered
  (`workflow_dispatch`) or nightly only** — it **never** runs on pull requests or
  pushes, because it costs a real VM and must never block or bill routine CI. It
  always destroys the VM on exit (even on failure), so nothing lingers.

  The only credential it needs is `HCLOUD_TOKEN`. The workflow reads it from the
  environment (supply it however you configure Actions env — e.g. a repository
  secret under Settings → Secrets and variables → Actions); the job's first step
  fails with a clear message if it is unset, and no credential is ever hardcoded:

  | Env var | What it is |
  |---|---|
  | `HCLOUD_TOKEN` | A Hetzner Cloud API token (Read & Write) from the Hetzner console → project → Security → API Tokens. Used to provision and destroy the throwaway VM. **Required.** |
  | `AUTUMN_DEPLOY_SIGNING_SECRET` | Optional. The app signing secret (`AUTUMN_SECURITY__SIGNING_SECRET`, 64 hex chars) so the deployed app passes production preflight. Taken from the environment when provided; otherwise a throwaway secret is generated per run — the VM is destroyed on exit, so it never needs a persistent value. |

  The lifecycle is a self-contained shell script
  (`scripts/deploy-real-vps-validate.sh`) that mirrors the container
  harness's curl/ssh assertions while driving the real `autumn` binary; the
  Hetzner-specific provisioning is isolated in the workflow so another provider
  can be swapped in without touching the script. The in-container half of the
  pam_systemd socket check runs as part of the standard
  `cargo test -p autumn-cli --test deploy_e2e -- --ignored` Docker sweep (no
  separate feature needed).

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
{ "status": "ok", "version": "0.7.0" }
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
                       /usr/share/autumn/sbom.cdx.json  ← CycloneDX inventory

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
- **Supply chain, on by default** -- the image carries a CycloneDX SBOM at
  `/usr/share/autumn/sbom.cdx.json` (advertised by the `io.autumn.sbom.path`
  label), the binary is compiled through `cargo-auditable` so it can report its
  own crate versions with no source tree, and Tailwind is fetched through
  `autumn setup`, whose download is SHA-256 verified. The generated cloud
  deploy workflows additionally attest each pushed image and its SBOM. See
  [Verify what you're running](supply-chain.md).
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

## Deploy to Azure Container Apps

Scaffold a Terraform configuration alongside the production Dockerfile:

```bash
autumn release init --force --target azure-container-apps
```

This generates:

| File | Purpose |
|---|---|
| `main.tf` | Resource group, Azure Container Registry, Log Analytics workspace, Container Apps environment + the Container App itself, a one-shot migration job, Azure Database for PostgreSQL Flexible Server, and a Key Vault that feeds secrets into the app via a user-assigned managed identity. An optional Redis Cache is gated behind `enable_redis_cache` — **infrastructure only**, see the callout below. |
| `variables.tf` | `app_name`, `subscription_id` (required — AzureRM v4 needs it explicitly, even under `az login`), `location`, `image_tag`, `db_sku`, `bootstrap_image`, `min_replicas`/`max_replicas` (default 0/10), `enable_redis_cache`, and `sensitive`, no-default secret variables (`database_admin_password`, `signing_secret`). |
| `outputs.tf` | `app_fqdn`, `acr_login_server`, `resource_group_name`, `migrate_job_name`, and `app_name`. |
| `terraform.tfvars.example` | Non-secret defaults only — secrets are documented as `TF_VAR_*` exports, never committed. |
| `.github/workflows/azure-deploy.yml` | Opt-in CI/CD: builds the release image, pushes it to ACR, runs the migration job to completion, and runs `az containerapp update` on a `v*` tag push (or manual dispatch). |

**Redis cache is infrastructure only.** `enable_redis_cache = true`
provisions an Azure Redis Cache and wires `AUTUMN_CACHE__BACKEND=redis` /
`AUTUMN_CACHE__REDIS__URL` into the Container App, but Autumn's cache
subsystem has no built-in Redis implementation — unlike sessions, channels,
and jobs, which activate purely from config once compiled with the `redis`
Cargo feature. Setting these env vars alone does nothing: your application
must *also* depend on the `autumn-cache-redis` crate and register
`.plugin(RedisCachePlugin::new())` in `main.rs`, or the config is parsed and
silently never read — you'd pay for a Redis instance the app never talks to.
See [Shared Cache](cloud-native.md#shared-cache) for the three steps.

The generated cache sets `non_ssl_port_enabled = false`, so the URL it hands
the app is always `rediss://`. That works out of the box — see [Redis over
TLS](cloud-native.md#redis-over-tls-rediss) — and applies to any other
subsystem you point at the same instance.

**Resource names are sanitized, not verbatim.** A Cargo package name may
contain underscores or uppercase letters (both invalid in Container App
names), so every Container Apps-family resource name (the app, its
environment, Log Analytics, the migration job) is lowercased, any other
character is mapped to a hyphen, runs of hyphens are collapsed to one, and a
leading/trailing hyphen is trimmed — `my_app`/`My App`/`my--app` all become
`my-app`. The base name is also padded up to Azure's 2-character minimum and
capped at 24 characters, leaving headroom for the longest suffix appended to
any of these resources (`-migrate`, 8 characters) so the full name never
exceeds Azure's 32-character maximum. ACR, Key Vault, Postgres, and Redis use
a stricter sanitization (no hyphens at all, since ACR forbids them).
Sanitization happens once, in Terraform (`local.app_name_safe`) — the
generated workflow never hardcodes a name; it reads the result back via
`terraform output app_name` (as the `AZURE_APP_NAME` repository variable),
so editing `app_name` in `terraform.tfvars` after scaffolding is picked up
automatically instead of
silently deploying under a stale name.

Why Container Apps and not App Service or AKS: it is the closest managed
analog to Fly.io — scale-to-zero capable, managed ingress with automatic TLS,
and no cluster to operate — while Azure-heavy shops already have the
IAM/RBAC patterns in place.

Provision the infrastructure and set secrets via Terraform variables (never
as literals in `terraform.tfvars`). A single apply is enough — there is no
`database_url` variable to pre-compute: main.tf derives the connection
string from the Postgres server this same apply creates, from its FQDN plus
`database_admin_password`.

```bash
cp terraform.tfvars.example terraform.tfvars   # edit app_name/location/subscription_id/etc.
# Azure's Postgres complexity policy needs 3 of {upper, lower, digit,
# symbol} — `openssl rand -hex` is lowercase-only, and even -base64 alone
# only samples its alphabet randomly (a small fraction of outputs could
# still miss a category). Appending "Aa1!" guarantees all 4 regardless.
export TF_VAR_database_admin_password="$(openssl rand -base64 18)Aa1!"
export TF_VAR_signing_secret="$(openssl rand -hex 32)"

terraform init
terraform apply
```

The Container App and migration job both start from a public placeholder
image (`bootstrap_image` — Container Apps must pull *some* image to create a
first revision, and a brand-new ACR has none yet). The generated
`min_replicas = 0` default is intentional: keep it at zero for the initial
apply so the placeholder app container is not started with production secret
refs or the app's Key Vault-capable managed identity. Build and push your
real image, run migrations, then cut the app over:

```bash
APP_NAME="$(terraform output -raw app_name)"           # sanitized — may differ from your Cargo package name
ACR="$(terraform output -raw acr_login_server)"
RG="$(terraform output -raw resource_group_name)"
# Must be unique per BUILD, not just per commit: the commit SHA alone
# collides if you re-run this block at the same HEAD (uncommitted local
# changes, or merely a fresh AUTUMN_BUILD_TIMESTAMP baked in below) — same
# tag, different bytes pushed to ACR. Azure isn't guaranteed to treat a
# re-pushed tag it already has configured on the app as a revision-scope
# change (see the automated workflow's identical reasoning: it folds in
# GITHUB_RUN_ID/GITHUB_RUN_ATTEMPT for the same reason), so the old binary
# could keep serving even after these migrations complete.
TAG="$(git rev-parse --short=12 HEAD)-$(date -u +%Y%m%d%H%M%S)"

az acr login --name "${ACR%%.azurecr.io}"
# The Dockerfile's AUTUMN_BUILD_* ARGs default to empty unless passed here —
# .dockerignore excludes .git from the build context, so this is the only
# way for /actuator/info to report real git provenance (see the Dockerfile's
# own header comment for the full --build-arg list this mirrors).
docker build \
  --build-arg AUTUMN_BUILD_GIT_SHA="$(git rev-parse HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_SHA_SHORT="$(git rev-parse --short HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_BRANCH="$(git rev-parse --abbrev-ref HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_DIRTY="$([ -z "$(git status --porcelain)" ] && echo false || echo true)" \
  --build-arg AUTUMN_BUILD_TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  -t "$ACR/$APP_NAME:$TAG" .
docker push "$ACR/$APP_NAME:$TAG"

# Run migrations to completion BEFORE updating the app — the generated
# production config sets auto_migrate_in_production = false, so nothing
# else does this for you. `az containerapp job start` only starts the
# execution and returns immediately; it does NOT wait for it to finish, so
# the loop below is required — proceeding straight to `az containerapp
# update` after `job start` returns would update the app before migrations
# have actually completed.
#
# `job start --image` sends an execution-TEMPLATE OVERRIDE, which Azure
# treats as a full replacement rather than a merge: an override containing
# only --image drops the `command` (autumn migrate) and the
# AUTUMN_DATABASE__PRIMARY_URL secret env Terraform configured on the job,
# so the execution would run the container's default command with no DB URL
# instead of applying migrations. `job update --image` persists just the
# image onto the job's STORED template, leaving command/env untouched; the
# bare `job start` that follows then runs that complete, up-to-date
# template.
MIGRATE_JOB="$(terraform output -raw migrate_job_name)"
az containerapp job update \
  --name "$MIGRATE_JOB" \
  --resource-group "$RG" \
  --image "$ACR/$APP_NAME:$TAG" \
  --output none
EXECUTION=$(az containerapp job start \
  --name "$MIGRATE_JOB" \
  --resource-group "$RG" \
  --query name -o tsv)
for _ in $(seq 1 66); do   # 660s — must exceed the job's own 600s replica_timeout_in_seconds
  STATUS=$(az containerapp job execution list \
    --name "$MIGRATE_JOB" \
    --resource-group "$RG" \
    --query "[?name=='$EXECUTION'].properties.status" -o tsv)
  case "$STATUS" in
    Succeeded) break ;;
    Failed|Stopped) echo "migration failed: $STATUS" >&2; exit 1 ;;
  esac
  sleep 10
done
[ "$STATUS" = "Succeeded" ] || { echo "migration did not finish within the time budget" >&2; exit 1; }

az containerapp update \
  --name "$APP_NAME" \
  --resource-group "$RG" \
  --image "$ACR/$APP_NAME:$TAG"
```

Terraform is told to ignore both resources' image afterward
(`lifecycle.ignore_changes`), so a later `terraform apply` won't revert a
live deploy back to the bootstrap placeholder.

**Automated deploys on tag push:** `.github/workflows/azure-deploy.yml` only
runs once you add the required repository secrets and variables it documents
in its header comment — secrets `AZURE_CLIENT_ID`/`AZURE_TENANT_ID`/
`AZURE_SUBSCRIPTION_ID` for OIDC login (no client secret needed: the
workflow's `id-token: write` permission plus a federated credential on the
app registration is enough), and variables (not secrets — they're just
config) `ACR_LOGIN_SERVER`/`AZURE_RESOURCE_GROUP`/`AZURE_MIGRATE_JOB_NAME`/
`AZURE_APP_NAME` (all four are `terraform output` values — never hand-typed)
— until then it stays dormant. Once configured, pushing a `v*` tag builds,
pushes to ACR, runs the migration job to completion (aborting before any
deploy if it fails), and runs `az containerapp update` automatically.

**Grant the service principal Contributor at the resource-group scope**, not
just on the Container App: the migration job is a separate resource in the
same group, and Azure RBAC granted on one resource does not inherit to a
sibling — a principal scoped only to the app 403s the moment the workflow
tries to start the migration job.

**The image tag is unique per execution, not just per commit**, e.g.
`v1.2.3-a1b2c3d4e5f6-4821903-1` — the sanitized ref, the commit SHA, the
GitHub Actions run ID, and the run attempt number. All four matter: the
sanitized ref alone can't be used as a Docker tag verbatim (a branch or tag
name may contain characters Docker tags reject, like `/` in `feature/login`
or `+` in a SemVer tag); the run ID and attempt matter beyond the commit SHA
because re-running `workflow_dispatch` on the same branch, or clicking
"Re-run jobs" on an existing run, reuses the identical ref *and* commit —
yet still produces a genuinely different build (a fresh
`AUTUMN_BUILD_TIMESTAMP`, and possibly different bytes entirely if base
image packages updated in between). Pushing different bytes under a tag
Azure Container Apps already has configured on the app isn't guaranteed to
register as a revision-scope change, so without a truly unique tag the old
binary could keep serving even after a newer run's migrations.

**Overlapping runs are serialized, never interleaved.** The job sets a
`concurrency` group per repository with `cancel-in-progress: false`, so a
second tag push or dispatch while one is still running queues behind it
instead of racing it — without this, the older run's `az containerapp
update` could land after the newer one and silently roll production back.
GitHub doesn't document strict FIFO ordering for which queued run goes next,
though, and two *different* immutable tags (e.g. `v1` then `v2`) each
trigger their own run against their own ref, so a same-ref check alone can't
see a newer release land under a different tag. The workflow instead checks
— immediately before migrating, as late as practical — whether any other run
of this same workflow with a higher `run_number` (GitHub's own counter,
monotonic in trigger order regardless of which ref triggered it) exists at
all, regardless of its status or conclusion, and aborts if so. That last
part is deliberate: filtering to only "still active" or "completed
successfully" runs isn't enough, because a newer run can migrate — the
actual point of no return, since the schema is advanced at that point — and
then fail on the deploy step *after* migrating, reporting an overall
conclusion of `failure`. There's no cheap way to tell "failed before
migrating" apart from "failed after migrating" from a run's top-level
status, so the guard treats the mere existence of any newer run as
disqualifying: if a newer run failed for an unrelated CI reason before ever
reaching migration, the fix is to re-trigger it, not to let an older run
sneak through in the meantime.

**The app's own hostname is a trusted host, automatically.** Autumn's
`prod` profile fails fast at startup — the process never binds — when
[`security.trusted_hosts.hosts`](#trusted-hosts-host-header-allow-list) is
empty, and `main.tf` sets `AUTUMN_PROFILE=prod`. The Container App's default
ingress hostname (`<app_name>.<environment default domain>`) is derived in
Terraform (`local.app_fqdn`) and passed in as
`AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS` so the first `az containerapp
update` actually serves traffic instead of crash-looping — the same value
`terraform output app_fqdn` prints, deliberately the *stable* ingress
hostname rather than a revision-specific one (which would send a `Host`
header the app doesn't trust, and would go stale the moment CI creates a new
revision outside Terraform). Add a comma-separated custom domain to the env
var once you bind one.

**State file security.** `terraform apply` writes `database_admin_password`,
the derived database connection string, and `signing_secret` into
`terraform.tfstate` **in plaintext** — Terraform's `sensitive = true` only
redacts CLI plan/apply
output, never the state file itself. Add `*.tfstate*`, `.terraform/`, and
`terraform.tfvars` to `.gitignore` before running `terraform init`
(`autumn release init --target azure-container-apps` does this for you,
merging into an existing `.gitignore` without touching unrelated lines), and
use a remote backend (e.g. an Azure Storage container with encryption at
rest) instead of local state for any real deployment.

**Production hardening.** Two scaffold defaults trade convenience for
recoverability and are worth revisiting before a real deploy: the Postgres
firewall rule (`AllowAzureServices` — Azure's sentinel for "allow
Azure-internal traffic", not the whole internet, but still broader than a
VNet-scoped private endpoint) and the Key Vault's `purge_protection_enabled
= false` (lets `terraform destroy` fully clean up during setup, but also
means a purge is unrecoverable either way — flip to `true` once the vault
holds real secrets).

---

## Deploy to AWS App Runner

Scaffold a Terraform configuration alongside the production Dockerfile:

```bash
autumn release init --force --target aws-app-runner
```

This generates:

| File | Purpose |
|---|---|
| `main.tf` | A VPC (private subnets for RDS + the App Runner VPC connector, one public subnet + NAT gateway for the app's own outbound traffic), an ECR repository, RDS PostgreSQL, Secrets Manager entries for the database URL and signing secret, the App Runner service itself, and a minimal one-shot ECS Fargate task+cluster whose only job is running `autumn migrate` against private RDS (App Runner has no release-phase hook of its own). |
| `variables.tf` | `app_name`, `region`, `image_tag`, `bootstrap_image`, `instance_cpu`/`instance_memory`, `db_instance_class`, `vpc_cidr`, `min_size`/`max_size` (default 1/10), and `sensitive`, no-default secret variables (`database_admin_password`, `signing_secret`). |
| `outputs.tf` | `app_url`, `service_arn`, `service_name`, `ecr_repository_url`, `apprunner_access_role_arn`, plus `migrate_cluster_name`/`migrate_task_family`/`private_subnet_ids`/`vpc_connector_security_group_id` for the one-shot migration task. |
| `terraform.tfvars.example` | Non-secret defaults only — secrets are documented as `TF_VAR_*` exports, never committed. |

There is no CI workflow for this target — it's the fast/minimal path; wire
up your own deploy automation once you outgrow the manual walkthrough
below, or move to [`--target aws-ecs`](#deploy-to-aws-ecs-fargate).

**Why App Runner and not ECS/EKS**: it is the closest managed analog to
Fly.io — auto-TLS, auto-scale, no ALB or app-hosting cluster to manage
(the one-shot migration task above is deliberately minimal — no service, no
ALB, no autoscaling — and isn't in the way day to day) — good for teams
new to AWS or doing a quick migration.

**Egress routes through the VPC.** Setting
`network_configuration.egress_configuration.egress_type = "VPC"` (required
so the app can reach RDS privately through the VPC connector) routes ALL of
the app's own outbound traffic through the private subnets, not just
RDS-bound traffic — `main.tf` provisions a NAT gateway so the app still has
general internet egress (a third-party API call, outbound mail, a webhook)
rather than silently hanging.

**Resource names are sanitized, not verbatim** — the same lowercasing/
hyphen-collapsing scheme as Azure Container Apps (see the callout in that
section above), capped at 20 characters here to leave headroom under the
tightest limit this scaffold touches.

Provision the infrastructure and set secrets via Terraform variables:

```bash
cp terraform.tfvars.example terraform.tfvars   # edit app_name/region/etc.
export TF_VAR_database_admin_password="$(openssl rand -hex 24)"
export TF_VAR_signing_secret="$(openssl rand -hex 32)"

terraform init
terraform apply
```

**Generate these two values once, then persist and reuse them** — save
them in a password manager or your CI's secret store the same way you
would any other production secret, rather than regenerating fresh values
on every `terraform apply`. Re-running these `openssl rand` commands in a
later shell session and re-applying changes the live RDS password and
Secrets Manager signing secret in place, but the App Runner service's
`source_configuration` is `lifecycle`-ignored (see `main.tf`) — it never
redeploys to pick up the change, so already-running containers keep using
the OLD values and lose database access as connections recycle.

The App Runner service starts from a public ECR Public Gallery placeholder
image (`bootstrap_image`) — App Runner must pull *some* image to create a
first revision, and a brand-new private ECR repository has none yet. Build
and push your real image, then cut the service over. Unlike Azure Container
Apps (whose ingress hostname is derivable before the app exists) or ECS
behind your own domain, **App Runner assigns its subdomain only after the
service is created**, so the trusted-hosts env var can only be set on this
same follow-up call, not on the first `terraform apply`:

```bash
# Every `aws` call below must target the same region `terraform apply` used
# — not whatever the operator's ambient AWS CLI config happens to point at.
# AWS_REGION (not AWS_DEFAULT_REGION) is the one to set: the AWS CLI
# documents AWS_REGION as taking precedence over AWS_DEFAULT_REGION when
# both are set, so an operator with AWS_REGION already exported would
# otherwise still have their ambient region win.
export AWS_REGION="$(terraform output -raw region)"

ECR="$(terraform output -raw ecr_repository_url)"
SERVICE_ARN="$(terraform output -raw service_arn)"
ACCESS_ROLE="$(terraform output -raw apprunner_access_role_arn)"
INSTANCE_ROLE="$(terraform output -raw apprunner_instance_role_arn)"
DATABASE_URL_ARN="$(terraform output -raw database_url_secret_arn)"
SIGNING_SECRET_ARN="$(terraform output -raw signing_secret_secret_arn)"
TAG="$(git rev-parse --short=12 HEAD)-$(date -u +%Y%m%d%H%M%S)"

# docker login takes a registry hostname, not a repository path — $ECR
# includes the repository suffix (e.g. "<account>.dkr.ecr.<region>.amazonaws.com/my-app"),
# so strip everything from the first "/" onward.
aws ecr get-login-password | docker login --username AWS --password-stdin "${ECR%%/*}"
docker build \
  --build-arg AUTUMN_BUILD_GIT_SHA="$(git rev-parse HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_SHA_SHORT="$(git rev-parse --short HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_BRANCH="$(git rev-parse --abbrev-ref HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_DIRTY="$([ -z "$(git status --porcelain)" ] && echo false || echo true)" \
  --build-arg AUTUMN_BUILD_TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  -t "$ECR:$TAG" .
docker push "$ECR:$TAG"
```

**Run `autumn migrate` against the database now, before the cutover below** — the generated production config sets `auto_migrate_in_production = false`, so nothing else runs it, and App Runner has no separate release-phase hook the way ECS's one-shot migration task or Azure's migration job does. RDS is private (not publicly accessible), so `main.tf` provisions a dedicated one-shot ECS Fargate task purely to reach it — using the same private subnets and VPC-connector security group App Runner's own traffic already uses, so there's no second security group or RDS rule to keep in sync. Only proceed to the `update-service` call below once migrations have actually completed; running it first serves the new, schema-dependent code against the old schema.

```bash
MIGRATE_CLUSTER="$(terraform output -raw migrate_cluster_name)"
MIGRATE_FAMILY="$(terraform output -raw migrate_task_family)"
SUBNETS="$(terraform output -json private_subnet_ids | jq -c .)"
SG="$(terraform output -raw vpc_connector_security_group_id)"

# Task definitions are immutable per revision — register a new one with the
# real image, keeping every other setting Terraform declared (env, secrets,
# logging) untouched.
NEW_DEF=$(aws ecs describe-task-definition --task-definition "$MIGRATE_FAMILY" --query 'taskDefinition' | \
  jq --arg IMAGE "$ECR:$TAG" ".containerDefinitions[0].image = \$IMAGE | \
    del(.taskDefinitionArn, .revision, .status, .requiresAttributes, .compatibilities, \
        .registeredAt, .registeredBy, .deregisteredAt)")
MIGRATE_ARN=$(echo "$NEW_DEF" | aws ecs register-task-definition --cli-input-json file:///dev/stdin \
  --query 'taskDefinition.taskDefinitionArn' --output text)

# run-task only starts the task and returns immediately, so poll for it to
# stop, then check its exit code. `aws ecs wait tasks-stopped` has its own
# fixed ~10-minute budget and exits nonzero once exhausted regardless of
# whether the task is still running — and ECS tasks have no runtime limit of
# their own — so poll manually and explicitly stop the task on timeout
# rather than leaving it running in the background.
TASK_ARN=$(aws ecs run-task --cluster "$MIGRATE_CLUSTER" --task-definition "$MIGRATE_ARN" \
  --launch-type FARGATE \
  --network-configuration "{\"awsvpcConfiguration\":{\"subnets\":$SUBNETS,\"securityGroups\":[\"$SG\"],\"assignPublicIp\":\"DISABLED\"}}" \
  --query 'tasks[0].taskArn' --output text)
[ -n "$TASK_ARN" ] && [ "$TASK_ARN" != "None" ] || { echo "failed to start the migration task"; exit 1; }

EXIT_CODE=""
for _ in $(seq 1 60); do   # 10 minutes (60 x 10s)
  STATUS=$(aws ecs describe-tasks --cluster "$MIGRATE_CLUSTER" --tasks "$TASK_ARN" \
    --query 'tasks[0].lastStatus' --output text)
  if [ "$STATUS" = "STOPPED" ]; then
    EXIT_CODE=$(aws ecs describe-tasks --cluster "$MIGRATE_CLUSTER" --tasks "$TASK_ARN" \
      --query 'tasks[0].containers[0].exitCode' --output text)
    break
  fi
  sleep 10
done
if [ -z "$EXIT_CODE" ]; then
  echo "migration did not finish within the time budget — stopping it"
  aws ecs stop-task --cluster "$MIGRATE_CLUSTER" --task "$TASK_ARN" \
    --reason "manual deploy: migration exceeded its polling budget" >/dev/null
  aws ecs wait tasks-stopped --cluster "$MIGRATE_CLUSTER" --tasks "$TASK_ARN"
  exit 1
fi
[ "$EXIT_CODE" = "0" ] || { echo "migration failed"; exit 1; }
```

```bash
APP_URL="$(terraform output -raw app_url)"   # known only after the FIRST apply created the service
# --source-configuration REPLACES the image configuration wholesale, not
# merges it — RuntimeEnvironmentSecrets must be re-supplied here alongside
# the real image, or the cutover silently drops
# AUTUMN_DATABASE__PRIMARY_URL/AUTUMN_SECURITY__SIGNING_SECRET and the real
# app can't boot. HealthCheckConfiguration restores the real "/health" path
# — main.tf's bootstrap revision used "/" (nginx's own default response)
# since the bootstrap placeholder doesn't serve /health.
OPERATION_ID=$(aws apprunner update-service --service-arn "$SERVICE_ARN" \
  --instance-configuration "{\"InstanceRoleArn\": \"$INSTANCE_ROLE\"}" \
  --health-check-configuration "{\"Protocol\": \"HTTP\", \"Path\": \"/health\"}" \
  --source-configuration "{
  \"ImageRepository\": {
    \"ImageIdentifier\": \"$ECR:$TAG\",
    \"ImageRepositoryType\": \"ECR\",
    \"ImageConfiguration\": {
      \"Port\": \"3000\",
      \"RuntimeEnvironmentVariables\": {
        \"AUTUMN_PROFILE\": \"prod\",
        \"AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS\": \"${APP_URL#https://}\"
      },
      \"RuntimeEnvironmentSecrets\": {
        \"AUTUMN_DATABASE__PRIMARY_URL\": \"$DATABASE_URL_ARN\",
        \"AUTUMN_SECURITY__SIGNING_SECRET\": \"$SIGNING_SECRET_ARN\"
      }
    }
  },
  \"AuthenticationConfiguration\": { \"AccessRoleArn\": \"$ACCESS_ROLE\" },
  \"AutoDeploymentsEnabled\": false
}" --query 'OperationId' --output text)

# update-service only STARTS an asynchronous operation — a bare successful
# exit here just means App Runner accepted the request, not that the real
# image actually booted. If it fails health checks, App Runner rolls the
# service back on its own; poll list-operations for this specific
# OperationId's terminal status rather than trusting the call's exit code.
OP_STATUS=""
for _ in $(seq 1 60); do   # 10 minutes (60 x 10s)
  OP_STATUS=$(aws apprunner list-operations --service-arn "$SERVICE_ARN" \
    --query "OperationSummaryList[?Id=='$OPERATION_ID'].Status | [0]" --output text)
  [ "$OP_STATUS" = "SUCCEEDED" ] && break
  case "$OP_STATUS" in
    FAILED|ROLLBACK_FAILED|ROLLBACK_SUCCEEDED)
      echo "cutover failed (status=$OP_STATUS) — App Runner may have rolled back to the bootstrap image"
      exit 1
      ;;
  esac
  sleep 10
done
[ "$OP_STATUS" = "SUCCEEDED" ] || { echo "cutover did not complete within the time budget (status=$OP_STATUS)"; exit 1; }
```

**State file security and `.gitignore`.** Same caveats as Azure: Terraform
state holds every secret in plaintext regardless of `sensitive = true`;
`autumn release init --target aws-app-runner` merges `.gitignore` entries
for `.terraform/`, `*.tfstate*`, and `terraform.tfvars` for you.

---

## Deploy to AWS ECS Fargate

Scaffold a Terraform configuration alongside the production Dockerfile:

```bash
autumn release init --force --target aws-ecs
```

This generates:

| File | Purpose |
|---|---|
| `main.tf` | A VPC with public/private subnets across 2 AZs, an internet-facing ALB (HTTP→HTTPS redirect, ACM certificate via Route 53 DNS validation), an ECR repository, an ECS cluster + Fargate task definition + service (deployment circuit breaker with automatic rollback), Application Auto Scaling on CPU/memory, RDS PostgreSQL in private subnets, Secrets Manager entries for the database URL and signing secret, and a one-shot migration task definition. An optional ElastiCache Redis replication group is gated behind `enable_redis_cache` — **infrastructure only**, see the callout below. |
| `variables.tf` | `app_name`, `region`, `vpc_cidr`, `domain_name`, `route53_zone_id`, `image_tag`, `bootstrap_image`, `cpu`/`memory`, `db_instance_class`, `desired_count`/`min_count`/`max_count` (default 2/1/10), `enable_redis_cache`, and `sensitive`, no-default secret variables (`database_admin_password`, `signing_secret`). |
| `outputs.tf` | `app_url`, `alb_dns_name`, `ecr_repository_url`, `ecs_cluster_name`, `ecs_service_name`, `app_task_family`, `migrate_task_family`, `private_subnet_ids`, `ecs_tasks_security_group_id`. |
| `terraform.tfvars.example` | Non-secret defaults only — secrets are documented as `TF_VAR_*` exports, never committed. |
| `.github/workflows/aws-deploy.yml` | Opt-in CI/CD: builds the release image, pushes it to ECR, registers new "app" and "migrate" task definition revisions, runs the migration task to completion, then updates the ECS service and waits for it to stabilize — on a `v*` tag push (or manual dispatch). |

**Why ECS Fargate and not App Runner/EKS**: it maps to patterns
AWS-experienced infra teams already have runbooks for, with full control
over networking, scaling, and rollout behavior — the production path once
you outgrow [`--target aws-app-runner`](#deploy-to-aws-app-runner)'s
minimal footprint.

**A domain and an existing Route 53 hosted zone are prerequisites**, not
optional — `domain_name`/`route53_zone_id` are required variables with no
default. The zone must already be the domain's live DNS (delegated at your
registrar) *before* `terraform apply`, or ACM's DNS certificate validation
will hang until Terraform's apply times out. Unlike App Runner (whose
subdomain is only assigned once the service exists), ECS's trusted-hosts
env var is set correctly from the very first apply, since the ALB serves
under a domain you already own.

**Redis cache is infrastructure only** — same caveat as Azure's Redis Cache
and for the same reason: Autumn's cache subsystem has no built-in Redis
implementation. Setting `enable_redis_cache = true` provisions ElastiCache
and wires `AUTUMN_CACHE__BACKEND=redis`/`AUTUMN_CACHE__REDIS__URL` into the
task, but your application must *also* depend on the `autumn-cache-redis`
crate and register `.plugin(RedisCachePlugin::new())` in `main.rs`, or
you'd pay for a Redis instance the app never talks to. See
[Shared Cache](cloud-native.md#shared-cache) for the three steps.

ElastiCache is provisioned with transit encryption on, so the URL is a
`rediss://` one — see [Redis over
TLS](cloud-native.md#redis-over-tls-rediss).

**Resource names are sanitized, not verbatim** — the same scheme as Azure
and App Runner, capped at 20 characters here so the longest suffixed name
(the `-migrate-tg`-style target group name, if it existed) would still fit
under the ALB/target-group family's 32-character AWS limit — the tightest
this scaffold touches.

Provision the infrastructure and set secrets via Terraform variables:

```bash
cp terraform.tfvars.example terraform.tfvars   # edit app_name/domain_name/route53_zone_id/etc.
export TF_VAR_database_admin_password="$(openssl rand -hex 24)"
export TF_VAR_signing_secret="$(openssl rand -hex 32)"

terraform init
terraform apply
```

**Generate these two values once, then persist and reuse them** — save
them in a password manager or your CI's secret store the same way you
would any other production secret, rather than regenerating fresh values
on every `terraform apply`. Re-running these `openssl rand` commands in a
later shell session and re-applying changes the live RDS password and
Secrets Manager signing secret in place, but the ECS service's
`task_definition` is `lifecycle`-ignored (see `main.tf`) — it never
redeploys to pick up the change, so already-running tasks keep using the
OLD values and lose database access as connections recycle.

The "app" and "migrate" ECS task definitions both start from a public
placeholder image (`bootstrap_image`) — Fargate must pull *some* image to
register a task definition's first revision, and a brand-new ECR repository
has none yet. Push your real image, migrate, then cut over — this is
exactly what `.github/workflows/aws-deploy.yml` automates once you add its
required repository secret (`AWS_ROLE_ARN`, an OIDC-federated IAM role — no
long-lived access keys) and variables (all `terraform output` values —
`AWS_REGION`, `ECR_REPOSITORY_URL`, `ECS_CLUSTER_NAME`, `ECS_SERVICE_NAME`,
`APP_TASK_FAMILY`, `MIGRATE_TASK_FAMILY`, `ECS_TASKS_SECURITY_GROUP_ID`,
`ECS_PRIVATE_SUBNET_IDS`). Manually, the same sequence looks like:

```bash
# Every `aws` call below must target the same region `terraform apply` used
# — not whatever the operator's ambient AWS CLI config happens to point at.
# AWS_REGION (not AWS_DEFAULT_REGION) is the one to set: the AWS CLI
# documents AWS_REGION as taking precedence over AWS_DEFAULT_REGION when
# both are set, so an operator with AWS_REGION already exported would
# otherwise still have their ambient region win.
export AWS_REGION="$(terraform output -raw region)"

ECR="$(terraform output -raw ecr_repository_url)"
CLUSTER="$(terraform output -raw ecs_cluster_name)"
SERVICE="$(terraform output -raw ecs_service_name)"
SUBNETS="$(terraform output -json private_subnet_ids | jq -c .)"
SG="$(terraform output -raw ecs_tasks_security_group_id)"
TAG="$(git rev-parse --short=12 HEAD)-$(date -u +%Y%m%d%H%M%S)"

# docker login takes a registry hostname, not a repository path — $ECR
# includes the repository suffix (e.g. "<account>.dkr.ecr.<region>.amazonaws.com/my-app"),
# so strip everything from the first "/" onward.
aws ecr get-login-password | docker login --username AWS --password-stdin "${ECR%%/*}"
docker build \
  --build-arg AUTUMN_BUILD_GIT_SHA="$(git rev-parse HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_SHA_SHORT="$(git rev-parse --short HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_BRANCH="$(git rev-parse --abbrev-ref HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_DIRTY="$([ -z "$(git status --porcelain)" ] && echo false || echo true)" \
  --build-arg AUTUMN_BUILD_TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  -t "$ECR:$TAG" .
docker push "$ECR:$TAG"

# Task definitions are immutable per revision — register a NEW one for each
# family with the real image, keeping every other setting Terraform
# declared (env, secrets, logging) untouched. The "app" family also strips
# entryPoint/command: Terraform's bootstrap revision overrides both to make
# the placeholder nginx image satisfy the ALB health check before any real
# image exists (main.tf) — carrying that override onto the REAL image would
# make it try to run the bootstrap's nginx script instead of its own
# Dockerfile ENTRYPOINT/CMD (tini + the compiled binary), and the container
# would exit immediately. The "migrate" family's own command (`autumn
# migrate`) is intentional and permanent, not bootstrap-specific, so it's
# left untouched.
APP_SECRETS=$(aws ecs describe-task-definition \
  --task-definition "$(terraform output -raw migrate_task_family)" \
  --query 'taskDefinition.containerDefinitions[0].secrets' --output json)
for FAMILY_OUT in app_task_family:APP migrate_task_family:MIGRATE; do
  FAMILY="$(terraform output -raw "${FAMILY_OUT%%:*}")"
  STRIP_ENTRYPOINT="del(.containerDefinitions[0].entryPoint, .containerDefinitions[0].command) | "
  [ "${FAMILY_OUT##*:}" = "MIGRATE" ] && STRIP_ENTRYPOINT=""
  NEW_DEF=$(aws ecs describe-task-definition --task-definition "$FAMILY" --query 'taskDefinition' | \
    jq --arg IMAGE "$ECR:$TAG" --argjson SECRETS "$APP_SECRETS" ".containerDefinitions[0].image = \$IMAGE |
      (if \"${FAMILY_OUT##*:}\" == \"APP\" then .containerDefinitions[0].secrets = \$SECRETS else . end) | ${STRIP_ENTRYPOINT}
      del(.taskDefinitionArn, .revision, .status, .requiresAttributes, .compatibilities,
          .registeredAt, .registeredBy, .deregisteredAt)")
  ARN=$(echo "$NEW_DEF" | aws ecs register-task-definition --cli-input-json file:///dev/stdin \
    --query 'taskDefinition.taskDefinitionArn' --output text)
  eval "${FAMILY_OUT##*:}_ARN=$ARN"
done

# Run migrations to completion BEFORE updating the service — the generated
# production config sets auto_migrate_in_production = false, so nothing
# else does this for you. run-task only starts the task and returns
# immediately, so poll for it to stop, then check its exit code.
#
# `aws ecs wait tasks-stopped` has its own fixed ~10-minute budget (100
# checks x 6s) and exits nonzero once that's exhausted regardless of
# whether the task is still running — and ECS tasks have no runtime limit
# of their own, so a migration that blows this budget keeps running in the
# background even after this script gives up. Poll manually instead so a
# timeout can explicitly stop the task before failing; otherwise retrying
# this walkthrough could start a second migration while the first is still
# mutating the schema.
TASK_ARN=$(aws ecs run-task --cluster "$CLUSTER" --task-definition "$MIGRATE_ARN" \
  --launch-type FARGATE \
  --network-configuration "{\"awsvpcConfiguration\":{\"subnets\":$SUBNETS,\"securityGroups\":[\"$SG\"],\"assignPublicIp\":\"DISABLED\"}}" \
  --query 'tasks[0].taskArn' --output text)
[ -n "$TASK_ARN" ] && [ "$TASK_ARN" != "None" ] || { echo "failed to start the migration task"; exit 1; }

EXIT_CODE=""
for _ in $(seq 1 60); do   # 10 minutes (60 x 10s)
  STATUS=$(aws ecs describe-tasks --cluster "$CLUSTER" --tasks "$TASK_ARN" \
    --query 'tasks[0].lastStatus' --output text)
  if [ "$STATUS" = "STOPPED" ]; then
    EXIT_CODE=$(aws ecs describe-tasks --cluster "$CLUSTER" --tasks "$TASK_ARN" \
      --query 'tasks[0].containers[0].exitCode' --output text)
    break
  fi
  sleep 10
done
if [ -z "$EXIT_CODE" ]; then
  echo "migration did not finish within the time budget — stopping it"
  aws ecs stop-task --cluster "$CLUSTER" --task "$TASK_ARN" \
    --reason "manual deploy: migration exceeded its polling budget" >/dev/null
  aws ecs wait tasks-stopped --cluster "$CLUSTER" --tasks "$TASK_ARN"
  exit 1
fi
[ "$EXIT_CODE" = "0" ] || { echo "migration failed"; exit 1; }

aws ecs update-service --cluster "$CLUSTER" --service "$SERVICE" \
  --task-definition "$APP_ARN" --force-new-deployment >/dev/null

# services-stable exits 255 once its own ~10-minute polling budget (100
# checks x 6s) is exhausted, regardless of whether the deployment is still
# genuinely in progress — without checking that explicitly, the script
# would fall through to the PRIMARY-deployment comparison below, which can
# still match $APP_ARN mid-rollout and report false success.
aws ecs wait services-stable --cluster "$CLUSTER" --services "$SERVICE" || {
  echo "deployment did not stabilize within the waiter's time budget"
  exit 1
}

# services-stable's predicate only requires ONE deployment to have
# runningCount == desiredCount — if the new revision failed its health
# checks, the deployment circuit breaker rolls the service back to the
# PREVIOUS revision, and that older deployment satisfies the waiter just as
# well. Without this check you could see "success" even though $APP_ARN
# was never actually running.
DEPLOYED_TASK_DEF=$(aws ecs describe-services --cluster "$CLUSTER" --services "$SERVICE" \
  --query "services[0].deployments[?status=='PRIMARY'].taskDefinition | [0]" --output text)
[ "$DEPLOYED_TASK_DEF" = "$APP_ARN" ] || { echo "deployment rolled back to $DEPLOYED_TASK_DEF"; exit 1; }
```

Terraform is told to ignore both the service's `task_definition` and each
task definition's `container_definitions` afterward
(`lifecycle.ignore_changes`), so a later `terraform apply` won't revert a
live deploy back to the bootstrap placeholder.

**Automated deploys on tag push**, **overlapping-run serialization**, and
**the unique-per-execution image tag** all follow the identical reasoning
documented for [Azure's workflow](#deploy-to-azure-container-apps) above —
`aws-deploy.yml` checks for a newer superseding run before migrating, uses
a repository-scoped `concurrency` group, and folds `GITHUB_RUN_ID`/
`GITHUB_RUN_ATTEMPT` into the image tag for the same re-run-collision
reasons.

**State file security and `.gitignore`.** Same caveats as Azure: Terraform
state holds `database_admin_password`, the derived database connection
string, and `signing_secret` in plaintext regardless of `sensitive = true`
— `autumn release init --target aws-ecs` merges `.gitignore` entries for
`.terraform/`, `*.tfstate*`, and `terraform.tfvars` for you. Use a remote
backend (e.g. an S3 bucket with a DynamoDB lock table, or S3-native
locking) instead of local state for any real deployment.

**Production hardening.** One scaffold default trades cost for
availability and is worth revisiting before a real deploy: `main.tf`
provisions a single NAT gateway rather than one per AZ, so an AZ outage
taking that gateway's own AZ down with it can interrupt egress for tasks
in the *other* AZ too. Add a second NAT gateway (one per AZ, each private
route table pointed at the one in its own AZ) before a real multi-AZ
production cutover.

---

## Deploy to GCP Cloud Run

Scaffold a Terraform configuration alongside the production Dockerfile:

```bash
autumn release init --force --target gcp-cloud-run
```

This generates:

| File | Purpose |
|---|---|
| `main.tf` | An Artifact Registry repository, a VPC with a Serverless VPC Access connector, Cloud SQL for PostgreSQL on a private IP (no public exposure), a dedicated runtime service account scoped to `roles/cloudsql.client` plus per-secret `secretAccessor` grants, Secret Manager entries for the database URL and signing secret, the Cloud Run service itself, and a one-shot Cloud Run Job that runs `autumn migrate`. An optional Memorystore Redis instance is gated behind `enable_redis_cache` — **infrastructure only**, see the callout below. |
| `variables.tf` | `project_id` (required — no default), `app_name`, `region`, `image_tag`, `db_tier`, `vpc_connector_cidr`, `bootstrap_image`, `min_instances`/`max_instances` (default 1/10), `enable_redis_cache`, and `sensitive`, no-default secret variables (`database_admin_password`, `signing_secret`). |
| `outputs.tf` | `service_url`, `service_name`, `artifact_registry_repository_url`, `migrate_job_name`, `service_account_email`, `sql_instance_connection_name`, `region`, and `project_id`. |
| `terraform.tfvars.example` | Non-secret defaults only — secrets are documented as `TF_VAR_*` exports, never committed. |
| `.github/workflows/gcp-deploy.yml` | Opt-in CI/CD: builds the release image, pushes it to Artifact Registry, updates and executes the migration job to completion, then updates the Cloud Run service — on a `v*` tag push (or manual dispatch). |

**Redis cache is infrastructure only.** `enable_redis_cache = true`
provisions a Memorystore Redis instance and wires
`AUTUMN_CACHE__BACKEND=redis` / `AUTUMN_CACHE__REDIS__URL` into the Cloud
Run service, but Autumn's cache subsystem has no built-in Redis
implementation — unlike sessions, channels, and jobs, which activate purely
from config once compiled with the `redis` Cargo feature. Setting these env
vars alone does nothing: your application must *also* depend on the
`autumn-cache-redis` crate and register `.plugin(RedisCachePlugin::new())`
in `main.rs`, or the config is parsed and silently never read — you'd pay
for a Redis instance the app never talks to. See
[Shared Cache](cloud-native.md#shared-cache) for the three steps.

Unlike the Azure and AWS targets, this one sets
`transit_encryption_mode = "DISABLED"` and emits a plaintext `redis://` URL.
That is deliberate: Memorystore presents a Google-internal CA that Autumn's
Redis TLS — which trusts the public CA store — will not validate, so
`SERVER_AUTHENTICATION` would hand the app a URL it can never connect to.
The connection stays inside your VPC.

**Secret access is scoped per secret, not project-wide.** The runtime
service account is granted `roles/secretmanager.secretAccessor` on exactly
the two (or three, with Redis) secrets it needs via
`google_secret_manager_secret_iam_member` — never a project-wide
`secretAccessor` binding, which would let a compromised container read
every secret in the project.

**The default `db_tier` is sized for dev, not for `max_instances` at full
scale.** `db-f1-micro`'s Postgres `max_connections` ceiling is small (around
25 — run `SHOW max_connections;` against your instance to confirm), while
each Cloud Run instance opens up to `pool_size` connections
(`autumn.production.toml.example` defaults to 10). At the default
`max_instances` of 10, scaling out under real load can exhaust that budget
well before hitting the instance ceiling. Before relying on autoscaling in
production, size `db_tier` so its `max_connections` comfortably exceeds
`max_instances * pool_size` (e.g. `db-custom-2-7680` supports roughly 200),
or lower `pool_size`/`max_instances` to fit the tier you're on.

**Rotating `database_admin_password` replaces the sole live credential in
place.** `google_sql_user.this`'s password update is ordered before the
`database_url` secret version (so a new Cloud Run revision never starts
with a password Cloud SQL hasn't accepted yet), but any already-running
revision still holds the OLD password in its resolved env until it's
replaced by the new one — a brief reconnect window during rollout, not a
zero-downtime rotation. A fully staged handoff would need a second
database user, which this scaffold doesn't provision; if you need
zero-downtime credential rotation, add one and cut over manually.

**Resource names are sanitized, not verbatim.** A Cargo package name may
contain underscores or uppercase letters (both invalid in Cloud Run/RFC
1035-style GCP resource names), so every resource name this scaffold
touches (the Cloud Run service and job, the Artifact Registry repository,
the VPC/connector, the Cloud SQL instance, the service account) is
lowercased, any other character is mapped to a hyphen, runs of hyphens are
collapsed to one, and a leading/trailing hyphen is trimmed — `my_app`/`My
App`/`my--app` all become `my-app`. Sanitization happens once, in Terraform
(`local.app_name_safe`) — the generated workflow never *hardcodes* a name,
reading `GCP_SERVICE_NAME` etc. as a repository variable instead of baking
one into the YAML. But that variable is a snapshot you set from
`terraform output`, not a live link to Terraform state — see the note on
resyncing it below if you edit `app_name` after scaffolding.

Why Cloud Run and not GKE or App Engine: it is the closest managed analog
to Fly.io — fully managed, auto-TLS, managed ingress, scales to zero,
pay-per-request — while GKE is operationally heavy and App Engine Standard
doesn't support arbitrary binaries.

Provision the infrastructure and set secrets via Terraform variables (never
as literals in `terraform.tfvars`). A single apply is enough — there is no
`database_url` variable to pre-compute: main.tf derives the connection
string from the Cloud SQL instance this same apply creates, from its
private IP plus `database_admin_password`.

```bash
cp terraform.tfvars.example terraform.tfvars   # edit app_name/region/project_id/etc.
export TF_VAR_database_admin_password="$(openssl rand -hex 24)"
export TF_VAR_signing_secret="$(openssl rand -hex 32)"

terraform init
terraform apply
```

The Cloud Run service and migration job both start from Google's public
Cloud Run "hello" quickstart image (`bootstrap_image` — Cloud Run must have
*some* image to create a first revision, and a brand-new Artifact Registry
repository has none yet; unlike App Runner's nginx placeholder, this image
already honors the `PORT` env var Cloud Run injects, so no bootstrap-port
workaround is needed). Build and push your real image, run migrations, then
cut the service over:

```bash
# gcloud's "current project" is whatever `gcloud config get-value project`
# happens to be set to — not necessarily this one. Pass --project to every
# call below explicitly, or a stale/different ambient project setting can
# silently target the wrong project (the image push would still succeed,
# since $AR_URL embeds the target project on its own — only for migration
# and deployment to then fail to find the resources this apply just
# created, or worse, update same-named resources elsewhere).
PROJECT_ID="$(terraform output -raw project_id)"
REGION="$(terraform output -raw region)"
SERVICE_NAME="$(terraform output -raw service_name)"   # sanitized — may differ from your Cargo package name
AR_URL="$(terraform output -raw artifact_registry_repository_url)"
MIGRATE_JOB="$(terraform output -raw migrate_job_name)"
TAG="$(git rev-parse --short=12 HEAD)-$(date -u +%Y%m%d%H%M%S)"

gcloud auth configure-docker "$(echo "$AR_URL" | cut -d/ -f1)" --quiet
docker build \
  --build-arg AUTUMN_BUILD_GIT_SHA="$(git rev-parse HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_SHA_SHORT="$(git rev-parse --short HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_BRANCH="$(git rev-parse --abbrev-ref HEAD)" \
  --build-arg AUTUMN_BUILD_GIT_DIRTY="$([ -z "$(git status --porcelain)" ] && echo false || echo true)" \
  --build-arg AUTUMN_BUILD_TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  -t "$AR_URL/$SERVICE_NAME:$TAG" .
docker push "$AR_URL/$SERVICE_NAME:$TAG"

# Run migrations to completion BEFORE updating the service — the generated
# production config sets auto_migrate_in_production = false, so nothing
# else does this for you. `gcloud run jobs update --image` persists the new
# image onto the job's stored template (leaving its command/env untouched);
# `gcloud run jobs execute --wait` then blocks until the execution actually
# finishes and exits non-zero on failure — no manual poll loop needed here,
# unlike the Azure/AWS walkthroughs above, since gcloud provides a
# synchronous wait natively.
gcloud run jobs update "$MIGRATE_JOB" --project "$PROJECT_ID" --region "$REGION" \
  --image "$AR_URL/$SERVICE_NAME:$TAG" --quiet
gcloud run jobs execute "$MIGRATE_JOB" --project "$PROJECT_ID" --region "$REGION" --wait

gcloud run services update "$SERVICE_NAME" --project "$PROJECT_ID" --region "$REGION" \
  --image "$AR_URL/$SERVICE_NAME:$TAG" --quiet
```

Terraform is told to ignore both resources' image afterward
(`lifecycle.ignore_changes`), so a later `terraform apply` won't revert a
live deploy back to the bootstrap placeholder.

**Automated deploys on tag push:** `.github/workflows/gcp-deploy.yml` only
runs once you add the required repository secrets and variables it
documents in its header comment — secrets `GCP_WORKLOAD_IDENTITY_PROVIDER`/
`GCP_DEPLOYER_SERVICE_ACCOUNT` for OIDC login via
`google-github-actions/auth` (no service account key needed: the workflow's
`id-token: write` permission plus a Workload Identity Federation provider
trusting GitHub's OIDC issuer is enough), and variables (not secrets —
they're just config) `GCP_PROJECT_ID`/`GCP_REGION`/
`GCP_ARTIFACT_REGISTRY_URL`/`GCP_SERVICE_NAME`/`GCP_MIGRATE_JOB_NAME` (all
five are `terraform output` values — never hand-typed) — until then it
stays dormant:

```bash
gh variable set GCP_PROJECT_ID --body "$(terraform output -raw project_id)"
gh variable set GCP_REGION --body "$(terraform output -raw region)"
gh variable set GCP_ARTIFACT_REGISTRY_URL --body "$(terraform output -raw artifact_registry_repository_url)"
gh variable set GCP_SERVICE_NAME --body "$(terraform output -raw service_name)"
gh variable set GCP_MIGRATE_JOB_NAME --body "$(terraform output -raw migrate_job_name)"
```

Once configured, pushing a `v*` tag builds, pushes to Artifact Registry,
updates and executes the migration job to completion (aborting before any
deploy if it fails), and runs `gcloud run services update` automatically.

**These repository variables are a one-time snapshot, not a live link to
Terraform state.** If you edit `app_name` (or `region`) in
`terraform.tfvars` after the workflow is already configured, `terraform
apply` renames the underlying Cloud Run service/job/Artifact Registry
repository, but GitHub has no way to know that happened — the variables
above keep pointing at the OLD names until you re-run the same
`gh variable set` commands with fresh `terraform output` values. Skipping
this after an `app_name` change means the next tag push builds and
deploys against resources that may no longer exist.

**Grant the deployer service account `iam.serviceAccountUser` on the
runtime service account**, not just `run.developer` on the service: Cloud
Run requires whoever deploys a revision to be able to act as whatever
service account that revision runs as — a deployer scoped only to
`run.developer` 403s the moment the workflow tries to update the service or
execute the migration job.

**The image tag is unique per execution, not just per commit** and
**overlapping runs are serialized, never interleaved** — for the identical
reasons documented for [Azure's workflow](#deploy-to-azure-container-apps)
above: `gcp-deploy.yml` folds `GITHUB_RUN_ID`/`GITHUB_RUN_ATTEMPT` into the
image tag, uses a repository-scoped `concurrency` group with
`cancel-in-progress: false`, and checks for a newer superseding run
(`run_number`) right before migrating.

**The app's own hostname is a trusted host, automatically — with no second
apply.** Autumn's `prod` profile fails fast at startup — the process never
binds — when [`security.trusted_hosts.hosts`](#trusted-hosts-host-header-allow-list)
is empty, and `main.tf` sets `AUTUMN_PROFILE=prod`. Cloud Run's default URL
format is `https://<service>-<project number>.<region>.run.app` (Google
switched to including the project *number* in 2022, specifically so a
deleted-and-recreated service can't be squatted at the old URL) — fully
derivable at plan time from a `google_project` data source, unlike App
Runner's subdomain (only assigned once the service exists) or unlike
needing to wait for `google_cloud_run_v2_service.this.uri` to be known
after creation. `main.tf` computes it as `local.service_url_host` and
passes it in as `AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS`, so the very first
`terraform apply` — before any CI has ever run — already serves traffic
instead of crash-looping.

**State file security.** `terraform apply` writes `database_admin_password`,
the derived database connection string, and `signing_secret` into
`terraform.tfstate` **in plaintext** — Terraform's `sensitive = true` only
redacts CLI plan/apply output, never the state file itself. Add
`*.tfstate*`, `.terraform/`, and `terraform.tfvars` to `.gitignore` before
running `terraform init` (`autumn release init --target gcp-cloud-run` does
this for you, merging into an existing `.gitignore` without touching
unrelated lines), and use a remote backend (e.g. a Google Cloud Storage
bucket with versioning and encryption at rest) instead of local state for
any real deployment.

**Production hardening.** Two scaffold defaults trade convenience for
recoverability and are worth revisiting before a real deploy: Cloud SQL's
`deletion_protection = false` and the Cloud Run service/job's own
`deletion_protection = false` (both let `terraform destroy`/recreate cycles
run without a manual override first, but also mean an accidental `destroy`
isn't blocked — flip both to `true` once real data/traffic depends on
them).

---

## Prometheus metrics for platform scraping

Autumn exposes a Prometheus text endpoint at `/actuator/prometheus`. It is
controlled by `actuator.prometheus` (default **`true`**) and is **independent of
`actuator.sensitive`**. That separation is the whole point: a production app can
let Fly.io (or any scraper) collect metrics while keeping `actuator.sensitive =
false`, so `/actuator/env`, `/actuator/configprops`, `/actuator/loggers`,
`/actuator/tasks`, `/actuator/jobs`, `/actuator/shadow`, and the actuator task
UI stay off the public surface. `/actuator/shadow` (see the [shadow deploys
guide](staged-deploys.md#shadow-differential-deploys)) is the most sensitive of
these: it publishes redacted excerpts of real production responses.

```toml
# autumn.toml — metrics on, sensitive surfaces off (the safe production shape)
[actuator]
sensitive  = false   # env/configprops/loggers/tasks/jobs/shadow NOT mounted
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

## Refreshing staging from production

Copying a production database into staging or onto a laptop is the fastest way
to reproduce a bug — and the fastest way to leak customer PII. Autumn ships the
whole loop:

```sh
AUTUMN_ENV=prod    autumn db backup --keep 7                       # 1. dump
#                  ... move the run directory to the staging host ...
AUTUMN_ENV=staging autumn db scrub --artifact backups/prod/<run> --force
```

The scrub restores the artifact into the staging database and rewrites every
PII-classified column with constraint-valid fake values. Classification is
fail-closed — a column that is neither classified nor explicitly declared safe
aborts the scrub — so a newly added column can never silently carry real data
into staging. Add `autumn db scrub --check` to CI to keep the declaration
honest.

See the [Data Scrubbing guide](data-scrubbing.md) for the classification
workflow, the replacement strategies, and the full drill.

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
