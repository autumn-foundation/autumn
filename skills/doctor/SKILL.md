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
| `jobs_queue_coverage` **(trunk-dev)** | Add the uncovered queue(s) to a tier's `jobs.pin` in `[jobs.fleet].tiers`, or run an unpinned tier that drains everything (only fails under `--strict` once `[jobs.fleet]` is declared) |
| `offsite_backup` **(trunk-dev)** | Set `backup.offsite.s3.bucket`, or set `backup.offsite.allow_shared_bucket = true` if intentionally reusing the app `[storage.s3]` bucket |

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

On trunk-dev, `autumn doctor --strict` can hard-fail on a genuine job-queue
coverage gap once the operator declares the fleet topology. Declare every worker
tier's pin under `[jobs.fleet]`:

```toml
[jobs.fleet]
tiers = [["critical"], ["bulk", "default"]]  # each inner array = one worker
                                             # tier's jobs.pin; [] = unpinned
                                             # (drains every queue)
manifest = "target/jobs-manifest.toml"       # OR: declared_queues = ["…"]
```

The `jobs_queue_coverage` check then reasons about the **union** of all tier
pins: it FAILS (exit 1 under `--strict`) only when a needed queue — the
configured `[jobs.queues]` unioned with the compiled `#[job(queue = "…")]`-declared
set — is drained by no tier anywhere, so a valid multi-tier subset split no
longer false-positives. The declared set comes from a `[jobs.fleet] manifest`
emitted by `autumn jobs manifest` (a TOML `queues = [...]` document) or an inline
`[jobs.fleet] declared_queues` list. Without `[jobs.fleet]` the check stays
informational-only (Pass), exactly as before (issue #1756).

## Secrets redaction

Before displaying any output, redact values that look like secrets:
URLs containing passwords, signing secret values, API keys, and tokens.
Replace with `[REDACTED]`.
