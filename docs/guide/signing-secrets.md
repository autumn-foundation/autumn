# Signing Secrets

Every production Autumn app that uses framework-owned signed state must have a
stable, private signing secret provisioned before the server starts. This guide
defines the contract, explains the dev/test defaults, and walks you through
provisioning, multi-replica setup, and rotation.

---

## What the signing secret covers

The signing secret is one shared key that protects every HMAC-signed surface
the framework manages:

| Surface | Why it needs the secret | Signed with no `secret` set? |
|---|---|---|
| **Session cookies** | Signs the session ID in the cookie. Stops forgery and replay. | No. Outside `prod`, the cookie is unsigned until you set `secret`. |
| **CSRF tokens** | Binds each CSRF token to the session. Uses the same key. | No. Same rule as session cookies. |
| **Flash / signed-cookie state** | Rides inside the session cookie. | No. It follows the session cookie's state. |
| **Local-storage signed URLs** | Signs presigned blob URLs from the local storage backend. Each URL carries an expiry. | Yes. Uses a random per-process key when `secret` is unset. |
| **`database.read_your_writes = "session"`** | Signs the `autumn.ryw` cross-request pinning cookie. | No. The cookie is not issued at all when `secret` is unset. |

Outside `prod`, only local-storage signed URLs get a real key with no `secret`
set. Every other row needs `secret` set — or the `prod`/`production` profile,
which fails startup without one (see below) — before the framework signs
anything for it.

---

## Development and test (zero-config)

In `dev` and `test` profiles, an unset `secret` does **not** turn on signing
for sessions, CSRF tokens, or the RYW cookie. Those stay unsigned. The RYW
cookie is not even issued. Set `security.signing_secret.secret` to turn
signing on for them.

Only local-storage signed URLs always get a key: Autumn generates a random,
per-process key for them at startup. You do not need to set anything for
local development to work.

**What that means day to day:**

| Scenario | Consequence |
|---|---|
| Process restart, default in-memory session store | Sessions are gone. The store is process-local memory. No key rotated — there was no key. |
| Process restart, session store backed by Redis or another persistent store | Old session cookies keep working. No key exists to rotate. |
| Local-storage signed URLs from a previous process | Return `403 Forbidden`. The per-process key rotated. |
| Multiple dev replicas, local-storage signed URLs | A URL signed by one replica fails on another. Each replica has its own key. |
| Multiple dev replicas, sessions on a shared store (e.g. Redis) | Sessions work across replicas. With no key, any replica holding the store can read the same unsigned ID. (The default in-memory store still keeps replicas apart — because it is process-local, not because of signing.) |
| `database.read_your_writes = "session"` | No `autumn.ryw` cookie is issued. Cross-request pinning silently does nothing until you set a secret. A warning is logged at startup. `request` mode is unaffected — it needs no cookie. |

This is intentional. An unset secret keeps local development zero-config and
stops a development secret from reaching production by accident. The `autumn
doctor` command reports this state as a **warning** so you know what to
expect.

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
| Dev, no secret configured | ⚠️ Warn | Sessions and CSRF tokens are unsigned; fine for local dev |
| Production, missing | ❌ Fail | Set `AUTUMN_SECURITY__SIGNING_SECRET` |
| Production, too short | ❌ Fail | Generate a new secret with `openssl rand -hex 32` |
| Production, demo value | ❌ Fail | Generate a new secret with `openssl rand -hex 32` |

`--strict` promotes the dev warning to a failure, which is useful in staging
environments that mirror production.
