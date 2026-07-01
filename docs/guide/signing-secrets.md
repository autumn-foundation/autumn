# Signing Secrets

Every production Autumn app that uses framework-owned signed state must have a
stable, private signing secret provisioned before the server starts. This guide
defines the contract, explains the dev/test defaults, and walks you through
provisioning, multi-replica setup, and rotation.

---

## What the signing secret covers

The signing secret is one shared key that protects every HMAC-signed surface
the framework manages:

| Surface | Why it needs the secret |
|---|---|
| **Session cookies** | The session backend signs the session ID embedded in the cookie so it cannot be forged or replayed |
| **CSRF tokens** | Per-request CSRF tokens are bound to the session and verified with the same key |
| **Flash / signed-cookie state** | Short-lived cross-request state rides the same signed cookie mechanism |
| **Local-storage signed URLs** | Blob presigned URLs (served by the local storage backend) are HMAC-SHA256 signed with this key and include an expiry |
| **Any future framework-owned signed token** | New framework surfaces will share this secret rather than introduce new config knobs |

---

## Development and test (zero-config)

In `dev` and `test` profiles Autumn generates an **ephemeral, per-process random
key** at startup. You do not need to set anything.

**What breaks with an ephemeral key:**

| Scenario | Consequence |
|---|---|
| Process restart | All existing sessions are invalidated immediately |
| Signed URLs from a previous process | Return `403 Forbidden` — the signature cannot be verified |
| Multiple dev replicas | Sessions started on one replica are not readable by another |
| `database.read_your_writes = "session"` | The ephemeral key is deliberately **not** threaded into the RYW middleware (see `router.rs`), so the `autumn.ryw` cookie is never issued — cross-request pinning silently does nothing until a secret is configured. A warning is logged at startup. `request` mode is unaffected (it needs no cookie). |

This is intentional: ephemeral keys keep local development zero-config and
ensure you never accidentally use a development secret in production. The
`autumn doctor` command reports this state as a **warning** so you know what
to expect.

---

## Production requirements

Before the server binds in the `prod` profile, Autumn validates the secret:

| Condition | Startup result |
|---|---|
| Secret not configured | Process exits with a clear error message |
| Secret shorter than 32 bytes | Process exits with a clear error message |
| Secret matches a known demo value (e.g. `"changeme"`, `"secret"`) | Process exits with a clear error message |

Generate a secret:

```bash
openssl rand -hex 32
```

This produces a 64-character hex string (32 bytes / 256 bits of entropy).

Set it as an environment variable — **never commit the value to source control**:

```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
```

Confirm it is accepted:

```bash
AUTUMN_ENV=prod autumn doctor
```

The `signing_secret` check should show ✅.

---

## `autumn.toml` configuration reference

```toml
# [security.signing_secret]
# secret and previous_secrets are intentionally omitted from this file.
# Set AUTUMN_SECURITY__SIGNING_SECRET in your deployment environment instead.
# Committing secrets to source control is a critical security vulnerability.
```

For rotation only — add previous secrets in the toml if you cannot set multiple
env vars in your platform:

```toml
[security.signing_secret]
previous_secrets = ["old-hex-secret-value"]
# current secret still comes from AUTUMN_SECURITY__SIGNING_SECRET env var
```

---

## Multi-replica deployments

Every replica **must use the same signing secret**. If replicas use different
keys, a user whose session was established on replica A will be rejected by
replica B (the session cookie signature will not verify).

Provision the secret once and supply it identically to all replicas:

```bash
# Generate once
SECRET=$(openssl rand -hex 32)

# Start replica 1
AUTUMN_ENV=prod \
AUTUMN_SECURITY__SIGNING_SECRET="$SECRET" \
AUTUMN_SESSION__BACKEND=redis \
AUTUMN_SESSION__REDIS__URL="redis://redis:6379" \
./myapp

# Start replica 2 — same secret, same Redis
AUTUMN_ENV=prod \
AUTUMN_SECURITY__SIGNING_SECRET="$SECRET" \
AUTUMN_SESSION__BACKEND=redis \
AUTUMN_SESSION__REDIS__URL="redis://redis:6379" \
./myapp
```

With a shared Redis session backend and the same signing secret:

- Sessions established on replica 1 are readable by replica 2.
- Signed blob URLs generated on replica 1 are verifiable by replica 2.
- CSRF tokens validate correctly regardless of which replica handles a request.

In container orchestration, supply the secret via a secret store rather than
an inline environment variable:

```yaml
# docker-compose.yml (secrets pattern)
services:
  app:
    environment:
      AUTUMN_ENV: prod
      AUTUMN_SECURITY__SIGNING_SECRET: "${SIGNING_SECRET}"
      AUTUMN_SESSION__BACKEND: redis
      AUTUMN_SESSION__REDIS__URL: "redis://redis:6379"
    deploy:
      replicas: 2
```

---

## Secret rotation

Autumn supports a **rotation grace window**: the current secret signs new
tokens while previous secrets continue to validate existing ones. This means
active sessions remain valid during a rolling deployment.

### Step-by-step rotation

**Step 1 — Generate a new secret:**

```bash
NEW_SECRET=$(openssl rand -hex 32)
echo "$NEW_SECRET"   # copy this value
```

**Step 2 — Stage the rotation in config (or env):**

Move the old secret to `previous_secrets` and set the new one:

```toml
# autumn.toml  (previous_secrets only — current secret comes from env var)
[security.signing_secret]
previous_secrets = ["old-secret-hex-value"]
```

```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$NEW_SECRET"
```

**Step 3 — Deploy the new secret to all replicas.**

Use a rolling restart so at least one replica is always serving. Because
`previous_secrets` includes the old key, replicas running the new code will
validate tokens that were signed by replicas still running the old code.

**Step 4 — Verify.**

After all replicas are running the new secret, confirm with `autumn doctor`:

```bash
AUTUMN_ENV=prod autumn doctor
```

The `signing_secret` check must show ✅.

**Step 5 — Remove the old secret after the grace window.**

The grace window is the maximum lifetime of any token signed with the old
secret. This is typically `session.max_age_secs` (default: 86 400 s / 1 day).
After that window has elapsed, remove the old entry:

```toml
[security.signing_secret]
# previous_secrets = []  # empty — old tokens are now expired
```

Redeploy. The old secret is fully retired.

---

## Rollback

If the new secret causes problems and you need to revert:

1. Restore the old secret as `AUTUMN_SECURITY__SIGNING_SECRET`.
2. Remove the entry from `previous_secrets` (or leave it — it is harmless).
3. Deploy the rolled-back configuration.

Sessions signed with the old secret are immediately valid again. Sessions
signed with the new secret during the window it was active will be
invalidated — users on those sessions will be asked to log in again.

---

## `autumn doctor` and `--strict`

```bash
# Check in dev (shows a warning about ephemeral key):
autumn doctor

# Check production config (fails if secret is missing or weak):
AUTUMN_ENV=prod autumn doctor

# Treat the ephemeral-key warning as a failure (useful in CI):
autumn doctor --strict

# Machine-readable output for CI pipelines:
AUTUMN_ENV=prod autumn doctor --json
```

The `signing_secret` check reports:

| Outcome | Status | Meaning |
|---|---|---|
| Production, valid secret | ✅ Pass | Ready to deploy |
| Dev, no secret configured | ⚠️ Warn | Ephemeral key in use; fine for local dev |
| Production, missing | ❌ Fail | Set `AUTUMN_SECURITY__SIGNING_SECRET` |
| Production, too short | ❌ Fail | Generate a new secret with `openssl rand -hex 32` |
| Production, demo value | ❌ Fail | Generate a new secret with `openssl rand -hex 32` |

`--strict` promotes the dev warning to a failure, which is useful in staging
environments that mirror production.
