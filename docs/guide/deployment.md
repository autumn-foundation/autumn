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

## Push-button deploy to your own server (`autumn deploy`)

`autumn deploy` takes a fresh project to a live, zero-downtime service on a
single Linux VPS you control — no Dockerfile, no container registry, no PaaS
account. It uploads a single embedded binary, supervises it with systemd behind
a reverse proxy, migrates before cutover, health-gates on `/ready`, and flips
traffic atomically. Re-running it is a zero-downtime redeploy; one command rolls
back.

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

Everything except two host prerequisites is automated by `autumn deploy up`:

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
- **The `kamal-proxy` binary present at `/usr/local/bin/kamal-proxy`.** The
  deploy writes and supervises the proxy's systemd unit, but does **not**
  download the binary itself — install it once as part of host bootstrap. (See
  [Limitations](#limitations-and-known-gaps).)
- **A local Rust toolchain** to build the release binary (`autumn build
  --embed`).

The reverse proxy, install directories, systemd units, release layout, and the
secret env file are all created for you.

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
# user = "root"           # SSH user (default: root)
# ssh_port = 22           # SSH port (default: 22)
# app_dir = "/srv/autumn/myapp"   # remote install dir (default: /srv/autumn/<app_name>)
# readiness_timeout_secs = 60     # /ready window before rollback (default: 60)
# keep_releases = 3               # prior releases retained for rollback (default: 3)
```

Every key also has an environment override (`AUTUMN_DEPLOY__HOST`,
`AUTUMN_DEPLOY__USER`, `AUTUMN_DEPLOY__SSH_PORT`, …) if you prefer to keep the
host out of the file.

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
> for all non-secret settings.
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
until it is green:

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
5. writes the app's systemd unit (bound to a private `127.0.0.1` port),
   points `current` at the release, and starts it,
6. health-gates on `GET /ready` within `readiness_timeout_secs`, then
7. routes the proxy at the freshly-ready release.

On success it prints:

```
✅ Deploy complete. Roll back with `autumn deploy rollback`.
```

Verify it is serving (the public port is your configured `server.port`):

```bash
curl http://203.0.113.10:3000/health   # -> {"status":"ok", ...}
curl http://203.0.113.10:3000/ready    # readiness probe used during cutover
```

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

### Rollback

Roll back to the previous release on demand:

```bash
autumn deploy rollback
```

This resolves the previous release on the host, brings its slot back up, flips
the proxy back to it (health-gated on `/ready`), repoints `current`, and
re-probes `/ready`. It fails loudly and non-zero when there is no previous
release to return to.

> **Rollback runs the same local preflight as `deploy up` first.** Before it
> makes any remote call — before it even resolves the previous release on the
> host — `autumn deploy rollback` runs the identical local preflight graders
> (signing secret, database URL, migrate-safety) and aborts non-zero if any fail.
> So it needs the same local inputs as a deploy — your project's `.env`/signing
> secret and database URL, and the `migrations/` dir — available **wherever you
> invoke it**: an emergency rollback from a bare CI checkout or a machine without
> the project's secrets fails preflight before it ever reaches the host. Keep the
> deploy inputs available where you would run a rollback.

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

### Where secrets live

The signing secret and database URL are written **only** to
`app_dir/shared/autumn.env` on the target, with mode `0600`, and are sourced by
the systemd unit via `EnvironmentFile`. They are never inlined into the
world-readable unit, never placed on a command line, and never printed to logs
or error messages.

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

### Limitations and known gaps

- **The `kamal-proxy` binary must be installed on the host** (at
  `/usr/local/bin/kamal-proxy`) before the first deploy. `autumn deploy up`
  configures and supervises the proxy but does not download its binary —
  provision it as part of host bootstrap.
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
- **Migrations run on redeploys, not on the very first deploy.** The pre-cutover
  migration one-shot is part of the zero-downtime redeploy path; the initial
  `deploy up` stands the release up and health-gates it. For a database-backed
  app, ensure the schema is applied (e.g. a follow-up `autumn deploy up`, or an
  out-of-band `autumn migrate` against the primary) before relying on DB routes.
- **Single host.** `[deploy] host` targets one server; there is no multi-host
  fan-out. For horizontally scaled setups behind a shared load balancer, see
  [Multi-replica setup](#multi-replica-setup).
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
  auto-rollback against a privileged systemd+sshd container. A container cannot
  power-cycle, prep a stock host from scratch, or reproduce a real
  pam_systemd session, so those are deferred to the real-VPS job below.
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

## Deploy to Azure Container Apps

Scaffold a Terraform configuration alongside the production Dockerfile:

```bash
autumn release init --force --target azure-container-apps
```

This generates:

| File | Purpose |
|---|---|
| `main.tf` | Resource group, Azure Container Registry, Log Analytics workspace, Container Apps environment + the Container App itself, a one-shot migration job, Azure Database for PostgreSQL Flexible Server, and a Key Vault that feeds secrets into the app via a user-assigned managed identity. An optional Redis Cache is gated behind `enable_redis_cache` — **infrastructure only**, see the callout below. |
| `variables.tf` | `app_name`, `subscription_id` (required — AzureRM v4 needs it explicitly, even under `az login`), `location`, `image_tag`, `db_sku`, `bootstrap_image`, `min_replicas`/`max_replicas` (default 1/10), `enable_redis_cache`, and `sensitive`, no-default secret variables (`database_admin_password`, `signing_secret`). |
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
first revision, and a brand-new ACR has none yet). Build and push your real
image, run migrations, then cut the app over:

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
for FAMILY_OUT in app_task_family:APP migrate_task_family:MIGRATE; do
  FAMILY="$(terraform output -raw "${FAMILY_OUT%%:*}")"
  STRIP_ENTRYPOINT="del(.containerDefinitions[0].entryPoint, .containerDefinitions[0].command) | "
  [ "${FAMILY_OUT##*:}" = "MIGRATE" ] && STRIP_ENTRYPOINT=""
  NEW_DEF=$(aws ecs describe-task-definition --task-definition "$FAMILY" --query 'taskDefinition' | \
    jq --arg IMAGE "$ECR:$TAG" ".containerDefinitions[0].image = \$IMAGE | ${STRIP_ENTRYPOINT}
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
