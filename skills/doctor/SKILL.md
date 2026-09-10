---
name: doctor
description: >
  Use when the user runs /autumn:doctor, asks to audit their Autumn app's
  configuration, check for deployment readiness, or diagnose config problems,
  missing secrets, or stale migrations.
argument-hint: "[--strict] [--json]"
allowed-tools:
  - Bash
  - Read
---

# autumn:doctor

Run `autumn doctor` to audit the project configuration and surface unsafe
defaults, missing secrets, stale migrations, and other deployment blockers.

## Execution

Run from the project root (directory containing `autumn.toml`):

```bash
autumn doctor --json
```

If `--strict` is passed as an argument, append it:

```bash
autumn doctor --strict --json
```

Capture stdout, stderr, and exit code. A nonzero exit code means problems
were found — treat it as signal, not as a hard stop.

## Output handling

The `--json` flag emits a structured result. Parse it and present a summary
grouped by severity:

- **FAIL** items: deployment blockers — list each one with the `detail` field
  and the one-line `hint` field if present.
- **WARN** items: non-blocking but worth flagging — list the most important ones.
- **PASS** items: do not enumerate unless asked; just report the total count.

Format:
```
Doctor results (exit <N>):
  ✗ FAIL (N): <description> — <remedy>
  ⚠ WARN (N): <description>
  ✓ PASS (N checks)
```

If the command is not found, tell the user to install:
```bash
cargo install autumn-cli --version 0.5.0
```

## Common FAIL remedies

These names match what `autumn doctor --json` actually emits in the `name` field:

| Check name | Remedy |
|---|---|
| `signing_secret` | `export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"` |
| `pending_migrations` | Run `autumn migrate` before deployment |
| `db_connectivity` | Verify `AUTUMN_DATABASE__PRIMARY_URL` and that Postgres is reachable |
| `trusted_hosts` | Set explicit `[security] trusted_hosts` in `autumn.toml` for production |
| `rate_limit_key_strategy` | Set `rate_limit.key_strategy` to `"ip"`, `"api_token"`, or `"authenticated_principal"` |
| `version_compat` | Upgrade `autumn-cli` to match the framework version: `cargo install autumn-cli --version 0.5.0` |
| `dotenv` | Copy `.env.example` to `.env` and fill in local values, or add every present secret dotenv file (`.env`, `.env.local`, and any `.env.<profile>.local`) to `.gitignore` if it is not already ignored — committable `.env.<profile>` files are left alone (warns only; never fails) |
| `jobs_queue_coverage` **(trunk-dev)** | Add the uncovered queue(s) to a tier's `jobs.pin` in `[jobs.fleet].tiers`, or run an unpinned tier that drains everything (fails once `[jobs.fleet]` is declared and a needed queue is uncovered; informational when `[jobs.fleet]` is absent) |
| `offsite_backup` **(trunk-dev)** | Set `backup.offsite.s3.bucket`, or set `backup.offsite.allow_shared_bucket = true` if intentionally reusing the app `[storage.s3]` bucket |
| `edge_target` **(trunk-dev)** | Run `rustup target add wasm32-wasip1` — the project has `#[edge]` routes, so `autumn build` needs that target to compile the edge capsule (passes with "no `#[edge]` routes" when the project has none) |
| `deploy_host` **(trunk-dev)** | Set a deploy target in `autumn.toml`: `[deploy] host = "<address>"` (one server) **or** `[deploy] hosts = ["<a>", "<b>"]` (a fleet, in rollout order) — never both. Blank or duplicate `hosts` entries are refused (issue #1621) |
| `plugin_residue` **(trunk-dev)** | Orphaned plugin wiring (issue #1631). Warns when a plugin is declared in `[dependencies]` but never mounted (`autumn plugin add <name>` finishes the install, `autumn plugin remove <name>` takes the dependency back — for a community `autumn-plugin-*` crate, add the `.plugin(...)` call from its README, since `add` never writes one). Fails when a plugin's fully-qualified type is mounted but the crate is not declared, which does not compile. Warns when migrations a plugin declares are still recorded as applied and the plugin is gone (`autumn plugin remove <name> --drop-data` reverts them); that last finding needs a configured database and the `diesel` CLI, and is skipped otherwise |
| `platform_support` **(trunk-dev)** | Never fails — reports this platform's Windows support tier (issue #1616). On Windows its `detail` names the Tier 1 (native) and Tier 2 (WSL2) commands and the vcpkg/`VCPKG_ROOT` prerequisite for `generate auth --passkeys` |
| `edge_routes` **(trunk-dev)** | Fails when an `#[edge]` handler also carries `#[secured]`/`#[authorize]`/`#[step_up]`/`#[throttle]`/`#[intercept]`: remove one of the two — edge routes are unauthenticated read-path routes served without origin middleware. Warns when a marked handler is missing from `edge_routes![]`, or when `src/bin/edge-capsule.rs` is absent |
| `dependencies` **(trunk-dev)** | Dependency policy findings (issue #1633). Fix or waive in `deny.toml` — a waiver is an `[advisories] ignore` entry, the same store the CI gate reads. Never fails for a missing cargo-deny or advisory database: those **pass** with a `not evaluated` detail, since warning would make `autumn doctor --strict` exit 1 on every machine without an optional tool. Warns with no verdict when `deny.toml` is not valid TOML, or declares a section in a spelling the generated CI workflow's grep cannot see |

## Platform support check (unreleased — trunk-dev, issue #1616)

`autumn doctor` runs a `platform_support` check first, reporting the platform's
Windows support tier. It **always passes** — the tier is information, not a
defect, and a warning would make `autumn doctor --strict` exit 1 on every
Windows machine. Read its `detail` rather than its status:

- On Linux/macOS: "every autumn journey runs natively on this platform".
- On Windows: the Tier 1 (native) commands, the Tier 2 (WSL2) commands, and the
  Windows prerequisites — notably that `autumn generate auth --passkeys` needs
  OpenSSL via `vcpkg` with `VCPKG_ROOT` set.

When a user reports that `autumn serve --daemon` (or `stop`/`status`/`restart`),
`autumn deploy up` (or `rollback`/`status`/`maintenance`), or a `scripts/*.sh`
gate fails on Windows, that is the **documented Tier 2 refusal, not a bug**:
those are built on Unix domain sockets, POSIX signals, `ssh`, and bash. Tell
them to run it from a WSL2 shell. `autumn deploy check` and `autumn deploy plan`
are the exception and Tier 1 — they are local-only, so a Windows developer can
validate a deploy config natively before switching to WSL2 to run it.
Note `autumn serve --bundled-pg` implies `--daemon`, so it is Tier 2 too — on
Windows use `autumn dev` for a managed-Postgres app. See
`docs/guide/platform-support.md`.

## Operator alert checks (unreleased — trunk-dev)

On trunk-dev, `autumn doctor` adds production-only **operator-alert** warnings
(issue #1610). It warns when `[alerts]` configures no destination (no `email`
and no `webhook_url`), when an `email` destination is paired with a disabled
`[mail]` transport or an SMTP transport missing/invalid `[mail] from`, when
`webhook_url` is set without a `webhook_secret` or is not an absolute `http(s)`
URL, when `error_rate_threshold` is outside `(0, 1]`, and when `[alerts]
enabled = false` despite a configured destination. If you register an
`AlertChannel` in code via `AppBuilder::with_alert_channel`, declare
`[alerts] custom_channel = true` so `--strict` passes without a `[alerts]`
destination. See `docs/guide/operator-alerts.md`.

### Native alert transports (unreleased — trunk-dev, issue #1630)

On trunk-dev, a configured native transport (a `[alerts] pagerduty_routing_key`,
`slack_webhook_url`, or `discord_webhook_url`) now counts as an alert
destination, so the production-only `alert_destination` no-destination warning
no longer fires when only a native transport is set (no `email`/`webhook_url`
needed). A separate production-only `alert_transports` check validates their
delivery-worthiness: it warns on a `pagerduty_routing_key` containing whitespace
(a malformed Events API v2 key), a set-but-non-absolute `pagerduty_url` override,
a `pagerduty_url` set with no routing key, and a `slack_webhook_url` /
`discord_webhook_url` that is not an absolute `https` URL (Slack and Discord only
expose `https` webhook endpoints). Each `AUTUMN_ALERTS__*` env override is
accepted in place of the TOML key (issue #1630). See
`docs/guide/operator-alerts.md`.

## Offsite-backup check (unreleased — trunk-dev, issue #1619)

`autumn doctor` adds an `offsite_backup` check for the offsite S3 backup
destination (`[backup.offsite]`). It never prints a credential value (only
env-var names / booleans), but it **can hard-fail** on an invalid configured
destination:

- **Pass** — no offsite destination is configured, or a configured destination
  is complete (bucket set + credential env vars ready).
- **Warn** — the bucket is set but the named credential env vars
  (`backup.offsite.s3.access_key_id_env` / `secret_access_key_env`) are not ready
  (unset name, or the variable is not exported).
- **Fail** — an invalid configured destination: `[backup.offsite]` is set without
  `backup.offsite.s3.bucket`, or the destination reuses the app's `[storage.s3]`
  bucket at the same endpoint without `backup.offsite.allow_shared_bucket = true`.

Remedy: add a `[backup.offsite]` section and run `autumn db backup --upload`, or
set the specific key the check names. See `docs/guide/daemon.md`.

## Topology-aware queue coverage (unreleased — trunk-dev, issue #1756)

On trunk-dev, `autumn doctor` hard-fails on a genuine job-queue coverage gap
once the operator declares the fleet topology. Declare every worker tier's pin
under `[jobs.fleet]`:

```toml
[jobs.fleet]
tiers = [["critical"], ["bulk", "default"]]  # each inner array = one worker
                                             # tier's jobs.pin; [] = unpinned
                                             # (drains every queue)
manifest = "target/jobs-manifest.toml"       # OR: declared_queues = ["…"]
```

The `jobs_queue_coverage` check then reasons about the **union** of all tier
pins: it returns a hard `Fail` when a needed queue — the configured
`[jobs.queues]` unioned with the compiled `#[job(queue = "…")]`-declared set —
is drained by no tier anywhere, so a valid multi-tier subset split no longer
false-positives. Because a `Fail` counts toward the exit code regardless of
mode, ANY `autumn doctor` run (not just `--strict`) exits non-zero on that gap;
`--strict` additionally escalates warnings to a non-zero exit. The declared set
comes from a `[jobs.fleet] manifest` emitted by `autumn jobs manifest` (a TOML
`queues = [...]` document) or an inline `[jobs.fleet] declared_queues` list.
Without `[jobs.fleet]` the check stays informational-only (Pass), exactly as
before (issue #1756).

## TLS certificate check (unreleased — trunk-dev, issue #1603)

On trunk-dev, `autumn doctor` adds a `tls` check that grades the `[server.tls]`
configuration:

- **Pass** — TLS is unconfigured (plain HTTP), or the configured leaf certificate
  is valid and expires more than 30 days out.
- **Warn** — the leaf certificate expires within 30 days, or the CLI was built
  without the `tls` feature (so the cert cannot be fully graded).
- **Fail** — a missing or invalid cert or key, a cert/key mismatch, or an
  expired/not-yet-valid leaf or intermediate certificate.

See issue #1852.

## ACME preflight checks (unreleased — trunk-dev, issue #1608)

On trunk-dev, `autumn doctor --online` (alias `--preflight`) runs active network
probes, gated behind the CLI `acme` feature:

- `acme_ports` — grades reachability of `:80` (HTTP-01) and `:443`. Under
  DNS-01 an unreachable `:80` is only a **Warn**: the CA never connects to this
  host, so all that is lost is the HTTP→HTTPS redirect (#1620).
- `acme_dns` — grades whether the configured `[server.tls.acme]` domains resolve
  to this host (`Matches` = pass, `PartialMatch` = warn, `ResolvesElsewhere` =
  fail). A `*.` entry is probed as the base domain it covers — a wildcard has no
  address record of its own. Under DNS-01 a name that resolves elsewhere is a
  **Pass**: the CA reads a TXT record rather than connecting to this host, so
  fronting the app with a load balancer or CDN is the normal shape (#1620).
- `acme_dns_propagation` — whether public DNS can answer for
  `_acme-challenge.<domain>` at all. **Fail** on SERVFAIL/timeout: the CA reads
  the challenge record from public DNS, so a broken delegation defeats a
  correctly-written record. **Warn** on records left over from an interrupted
  run (#1620).

Two checks are graded **offline**, so they run without `--online`:

- `acme_stored_cert` — the cached leaf's expiry.
- `acme_dns_credential` — the `[server.tls.acme.dns]` provider credential is
  readable and carries the fields that provider needs, graded with the runtime's
  own `validate_credential` so doctor and the server cannot disagree. **Fail**
  when missing: without it every issuance and renewal fails, and nothing about
  the running app reveals that until the certificate is expiring (#1620).
- `acme_tenancy_domain` — `[tenancy] base_domain`'s subdomains are actually
  covered by `[server.tls.acme] domains`. **Fail** otherwise: every tenant host
  would serve a certificate name mismatch (#1620).
- `acme_ca_root` — the configured `[server.tls.acme] ca_root_path`, validated
  through the same `CertificateDer::from_pem_file` + `RootCertStore::add` pair
  the runtime uses. Fails on a blank, unreadable, or unusable file (that state
  can only produce failed orders); warns when the file is a bundle — only its
  first certificate becomes a trust anchor — or when it is set alongside a
  publicly-trusted Let's Encrypt directory.

See issues #1858 and #1620.

## Deploy preflight checks (unreleased — trunk-dev, issues #1607/#1621)

When a `[deploy]` section is present in `autumn.toml`, `autumn doctor` runs the
deploy preflight as a config-gated section, emitting deploy-namespaced checks:
`deploy_config` (the `[deploy]` section parses), `deploy_host` (a deploy target
is configured), `deploy_ssh_reachability` (`--online` only),
`deploy_signing_secret`, `deploy_database_url`, and `deploy_migrate_check`. The
same graders back the standalone `autumn deploy check`, so the offline `doctor`
and online `deploy check` surfaces report identically. The `[deploy]` profile
defaults to the **production** profile (`"prod"`). The full flow is
`autumn deploy {check | plan | up | rollback | status | maintenance on|off}`
(SSH: install proxy on first deploy, zero-downtime cutover with auto-rollback on
redeploy, on-demand `rollback`, and — with `[deploy] hosts` — a serial rolling
deploy across a fleet).

### Fleet grading (issue #1621)

The deploy target is a **list**: `[deploy] host` (one server) or `[deploy] hosts`
(a fleet, in rollout order). `doctor` enumerates it through the same validator
`autumn deploy check` uses, so a fleet config is graded rather than reported as
"no target host configured":

- `deploy_host` — **passes** naming every configured host in rollout order
  (`3 deploy target hosts configured, in rollout order: …`). It **fails** on a
  malformed spelling — `host` and `hosts` both set, a blank `hosts` entry, or a
  duplicate entry — with the same hard refusal `autumn deploy check` makes.
  Remedy: keep either `host` or `hosts`, never both, and make every `hosts` entry
  a distinct non-blank address.
- `deploy_ssh_reachability` (`--online`) — probes **every** host and fails if
  **any** is unreachable, naming the unreachable ones. A broken host spelling
  fails this check too (there is no list to probe), so the `--json` key set never
  changes shape.

`doctor` keeps **one check per grader name** even for a fleet — `CheckResult.name`
is a stable `--json` key — so the per-host detail rides in the `detail` text.
`autumn deploy check` is the surface that prints one `ssh_reachability` line per
host, scoped as `ssh_reachability (10.0.0.3)`.

For live per-host state (deployed release, live slot, `/ready`, maintenance flag,
proxy port, version/state drift) use `autumn deploy status [--json] [--strict]`
rather than `doctor` — it probes the hosts read-only and `--strict` exits
non-zero on drift. See `docs/guide/fleet-deploys.md`.

## SQLite backend awareness (unreleased — trunk-dev, issue #1614)

For an app whose resolved primary database is `sqlite://…`, `autumn doctor`
adapts Postgres-specific checks: the `pg_dump`/`pg_restore` client-tools check
reports informationally: those tools are not required for a SQLite app, whose
`autumn db backup` / `restore` work on the data file in-process. A
`sqlite://` URL is only accepted as the lone primary in a single-role,
single-host topology (SQLite is single-writer / no read-replica role).

## Dependency policy check (unreleased — trunk-dev, issue #1633)

`autumn doctor` grades the app's lockfile against its own `deny.toml`, using the
same auditor, policy file, waiver store and check list as the CI gate the
scaffold generates — so a local verdict predicts the CI verdict.

```
❌ dependencies — 1 finding, 1 blocking — cargo-deny 0.20.2; checks: advisories; advisory data 0 days old
   RUSTSEC-2020-0071 vulnerability (high) time 0.1.45 — Potential segfault in the time crate
```

Reading the status:

- **fail** — a finding the policy denies. Severity is consequence, not
  taxonomy: denied findings grade high or critical (a CVSS v3 base score
  separates the two), warned findings grade low or medium.
- **warn** — only warn-level findings (duplicate or yanked crates), stale
  advisory data (over 7 days), or **no verdict**: a `deny.toml` that is not
  valid TOML, a section spelling only doctor's parse can see, or an auditor
  error the diagnostic parse could not account for.
- **pass** — no live findings, *or* the policy was not evaluated because
  cargo-deny is not installed or no advisory database is present. A pass whose
  detail reads `not evaluated` is not a clean bill of health; read the detail.

Waivers go in `deny.toml` as `[advisories] ignore` entries and are read by
doctor, `autumn dev` and CI alike — there is no second waiver format. A waived
finding shows as waived and never fails.

Neither doctor nor `autumn dev` fetches the advisory database; both run
`cargo deny --offline`. Tell a user reporting a stale-data warning to run
`cargo deny fetch db`. Two parity differences with CI are reported rather than
hidden: CI pins its cargo-deny version while a local run uses whatever is
installed (doctor names the version it used), and CI fetches the database every
run (doctor names its data age). See `docs/guide/supply-chain.md`.

## Secrets redaction

Before displaying any output, redact values that look like secrets:
URLs containing passwords, signing secret values, API keys, and tokens.
Replace with `[REDACTED]`.
