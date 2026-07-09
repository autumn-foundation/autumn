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

## Secrets redaction

Before displaying any output, redact values that look like secrets:
URLs containing passwords, signing secret values, API keys, and tokens.
Replace with `[REDACTED]`.
