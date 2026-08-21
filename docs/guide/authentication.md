# Authentication

This is the hub guide for "who is this request?" in Autumn: cookie-backed
sessions, password hashing and password policy, login/logout, route protection,
account lockout, remember-me, and active-session revocation.

Everything here is about **proving identity**. Deciding what an identified user
is allowed to *do* is [authorization](./authorization.md); proving identity
*again* before a dangerous action is
[step-up auth](./step-up-authentication.md).

This guide covers:

- [What's in the box](#whats-in-the-box) — the primitive behind each capability.
- [Quick start: `autumn generate auth`](#quick-start-autumn-generate-auth).
- [Sessions](#sessions) — the `Session` extractor, cookie, and stores.
- [Passwords](#passwords) — hashing, verification, and the password policy.
- [Login and logout](#login-and-logout) — session fixation and non-enumeration.
- [Protecting routes](#protecting-routes) — `#[secured]`, `RequireAuth`, `Auth<T>`.
- [Account lockout](#account-lockout) — credential-stuffing defence.
- [Remember me](#remember-me) — rotating tokens with theft detection.
- [Active sessions and revocation](#active-sessions-and-revocation).
- [Testing authenticated routes](#testing-authenticated-routes).
- [Production checklist](#production-checklist).
- [Where to go next](#where-to-go-next).

---

## What's in the box

Autumn ships the *primitives* — a session store, password hashing, a password
policy, a route guard — and the *generator* that assembles them into a complete,
app-owned login flow. Nothing here is a black box: `autumn generate auth` writes
ordinary handlers into your `src/routes/`, and you edit them like any other code.

| Capability | Primitive | Config block |
|---|---|---|
| Cookie-backed sessions | `autumn_web::session::{Session, SessionStore}` | `[session]` |
| Password hashing | `autumn_web::auth::{hash_password, verify_password}` | — |
| Password policy | `autumn_web::auth::validate_password` | `[auth.password]` |
| Route guard | `#[secured]`, `RequireAuth`, `Auth<T>` | `[auth].session_key` |
| Account lockout | generated login handler | `[auth.lockout]` |
| Remember me | `autumn_web::auth::remember` | `[auth.remember]` |
| Session tracking / revocation | generated `*_sessions` table | `[auth.sessions]` |
| Re-auth for sensitive actions | `#[step_up]` | `[auth.step_up]` |
| Social login | `autumn generate auth --oauth …` | `[auth.oauth2.*]` |

---

## Quick start: `autumn generate auth`

```sh
autumn generate auth User
```

One command scaffolds the whole browser flow — a `User` model and migration,
route handlers, request-level tests, and documentation — as app-owned code:

| Method | Path | Auth |
|---|---|---|
| `GET`/`POST` | `/signup` | Public |
| `GET`/`POST` | `/login` | Public |
| `POST` | `/logout` | Any |
| `GET` | `/account` | Required + email-confirmed |
| `POST` | `/account/destroy` | Required + [step-up](./step-up-authentication.md) |
| `GET`/`POST` | `/reauth` | Any |
| `GET`/`POST` | `/forgot-password` | Public |
| `GET`/`POST` | `/reset-password` | Public |
| `GET` | `/auth/confirm/{token}` | Public |
| `GET`/`POST` | `/auth/confirm/resend` | Public |
| `GET` | `/account/sessions` | Required |
| `POST` | `/auth/admin/unlock` | `X-Admin-Secret` header |

Optional factors compose on top, each off by default:

| Flag | Adds |
|---|---|
| `--oauth github,google` | Redirect + callback handlers, `oauth_identities` table, the `oauth2` feature — see [OAuth2 / OIDC](./oauth.md) |
| `--totp` | TOTP enrollment + login-verify, encrypted-at-rest secrets, single-use recovery codes |
| `--passkeys` | WebAuthn ceremony handlers, `webauthn_credentials` table, a passkey list/revoke surface |
| `--magic-link` | `/login/magic` request → email → verify, single-use digest tokens, per-email cooldown |

The generator also writes `docs/guide/authentication.md`,
`docs/guide/session-management.md`, and `docs/guide/gdpr-compliance.md` **into
your project**, describing the code it just wrote (that project-local file is
distinct from this framework guide). Re-run with `--force` to regenerate
templates after an upgrade.

Prefer to hand-roll? The rest of this guide is the primitive-by-primitive tour.

---

## Sessions

A session is a server-side key-value map keyed by an opaque id that travels in a
cookie. `SessionLayer` is installed for you by `AppBuilder`; handlers just ask
for the `Session` extractor (re-exported from the prelude):

```rust,ignore
use autumn_web::prelude::*;

#[get("/dashboard")]
async fn dashboard(session: Session) -> AutumnResult<String> {
    let user_id = session
        .get("user_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("log in first"))?;
    Ok(format!("Hello, {user_id}"))
}
```

### The `Session` API

| Method | What it does |
|---|---|
| `get(key)` / `insert(key, value)` / `remove(key)` | Read and write `String` values |
| `contains_key(key)` | Membership test without cloning the value |
| `clear()` | Drop all data, **keep** the id |
| `rotate_id()` | Mint a new id for the same data — call on every privilege change |
| `destroy()` | Drop the data and the server-side record, and expire the cookie |
| `id()` | The current session id |
| `touch()` | Force a save + `Set-Cookie` even if nothing was written |

Values are strings. Store a user id, a tenant id, a role — not a serialized
object graph.

The layer only writes back when the session is **dirty**, i.e. something was
inserted, removed, cleared, rotated, or destroyed. That matters in one case: a
handler that only reads `session.id()` and hands that id to something which will
later identify the session never dirties it, so a first-time visitor's browser
never receives the cookie. Call `session.touch()` in that situation.

`rotate_id()` and `destroy()` both invalidate the old server-side record — on
save, the layer deletes the previous id from the store — so a captured cookie
cannot be replayed afterwards.

### Cookie and backend configuration

```toml
[session]
backend      = "memory"       # "memory" | "redis"
cookie_name  = "autumn.sid"
max_age_secs = 86400          # 24 hours
secure       = true           # HTTPS-only
http_only    = true           # invisible to JavaScript
same_site    = "Lax"
path         = "/"

[session.redis]
url        = "redis://127.0.0.1:6379"
key_prefix = "autumn:sessions"
```

Every key has an `AUTUMN_SESSION__*` environment override
(`AUTUMN_SESSION__BACKEND`, `AUTUMN_SESSION__COOKIE_NAME`,
`AUTUMN_SESSION__MAX_AGE_SECS`, `AUTUMN_SESSION__REDIS__URL`, …). See
[runtime config](./runtime-config.md).

**Backends:**

- `memory` (default) — process-local, lost on restart, not shared between
  replicas. Fine for dev and tests. Booting the `prod` profile on it logs a
  warning; set `session.allow_memory_in_production = true` to acknowledge it
  deliberately (single-replica, restart-tolerant deployments).
- `redis` — survives restarts and is shared across replicas. Requires the
  `redis` Cargo feature; without it, selecting the backend is a startup error
  rather than a silent downgrade.
- Anything else — implement `SessionStore` (three async methods: `load`, `save`,
  `destroy`) and install it with `AppBuilder::with_session_store(…)`, which
  bypasses config-driven backend selection entirely.

If the store is unreachable mid-request the response is `503 Service
Unavailable` — sessions fail closed, they do not silently degrade to
"anonymous".

### Cookie integrity

When a [signing secret](./signing-secrets.md) is configured, the session id in
the cookie is HMAC-signed and a tampered cookie is rejected. The same is true in
production whether or not you configure one, because the production profile
fails fast without a secret.

**Without a configured secret in `dev`/`test`, session cookies are not signed at
all.** The router only threads signing keys into the session layer when a secret
is set or the profile is production; otherwise the cookie carries the raw
session id. In practice that is masked by the default memory store — its data
dies with the process, so a restart ends the session regardless. Point a dev
process at Redis or another persistent store, though, and an old cookie still
resolves across restarts, because there is no signature whose key changed. Set a
secret locally if you want dev to behave like production here.

---

## Passwords

### Hashing and verification

```rust,ignore
use autumn_web::auth::{hash_password, verify_password};

let digest = hash_password(&form.password).await?;      // bcrypt, cost 12
let ok = verify_password(&form.password, &digest).await?;
```

Both run the bcrypt work on a blocking thread pool, so a login storm does not
stall the async runtime. `verify_password` also verifies against a dummy hash
when the stored value fails a cheap shape check — 60 characters starting with
`$` — so an empty or obviously non-bcrypt column costs the same wall time as a
real mismatch instead of returning instantly and leaking that fact. The check is
shape-only: a 60-character `$`-prefixed string that is *not* a valid bcrypt
digest is passed straight to `bcrypt::verify`, which can reject it on parse
without doing cost-12 work. Corrupted or badly imported hashes can therefore
still be distinguished by timing; keep the uniform-timing guarantee for the
unknown-account path (below), which is the one an attacker probes.

`hash_password` hashes at a fixed cost of 12. `[auth].bcrypt_cost` is surfaced on
`AuthConfig` for applications that call bcrypt themselves; changing it does not
change what `hash_password` does.

### Password policy

`validate_password` accumulates **every** applicable failure so the form can show
all of them at once, rather than making the user fix one problem per round-trip:

```rust,ignore
use autumn_web::auth::{BreachCheck, validate_password};

// `config_arc` clones the `Arc`, not the config behind it, so reading policy
// on a per-request path costs a refcount bump rather than a deep clone.
let config = state.config_arc();
let cfg = &config.auth.password;
let mut policy = cfg.policy();
if cfg.breach_check != BreachCheck::Off {
    // The HIBP lookup needs an HTTP client; the default-off path never builds one.
    policy = policy.with_client(autumn_web::http_client::Client::new());
}

// `context` holds strings the password must not resemble — email, username.
let validation = validate_password(&form.password, &policy, &[email.as_str()]).await;
if !validation.is_valid() {
    return Ok(signup_page(cfg.min_length, &validation.messages().join("\n")));
}
```

Read config through `state.config_arc()` on anything that runs per request. It
hands back a shared `Arc<AutumnConfig>`, so the read is a refcount bump: borrow
the section you need off the handle (`&config.auth.password`), or clone just
that section if you need it owned.

`state.config()` is the per-boot accessor. It hands back an owned, independently
mutable snapshot, and paying for one means deep-cloning **every** config section
— 64 allocations and 1,384 bytes per call against a default config, more as your
config grows — to read a single field. On a request path that cost is repeated
on every request, and a handler that reads two or three sections pays it two or
three times over. Reach for it in `on_startup` hooks and one-shot setup, not in
handlers.

```toml
[auth.password]
min_length    = 8       # counted in Unicode scalar values
reject_common = true    # bundled 10k weak-password corpus, compiled into the binary
breach_check  = "off"   # "off" | "fail_open" | "fail_closed"
```

The similarity check tokenizes each context string on non-alphanumeric
boundaries (`john.doe@example.com` → `john`, `doe`, `example`, `com`) and rejects
a password that equals a context string or contains any token of length ≥ 4.

**Breach checking (HIBP)** uses k-anonymity: only the first five hex characters
of the password's SHA-1 are sent to `api.pwnedpasswords.com`. The password and
its full hash never leave the process and are never logged. The two "on" modes
differ only in what happens when HIBP is unreachable — `fail_open` allows the
password, `fail_closed` rejects it. Start with `fail_open`: a transient HIBP
outage should not block legitimate sign-ups.

Render the policy into the form (`minlength = state.config_arc().auth.password
.min_length`) so the client-side hint always matches what the handler enforces.

---

## Login and logout

A login handler does three things: verify the credential, **rotate the session
id**, then record the identity.

```rust,ignore
#[post("/login")]
pub async fn login(
    session: Session,
    mut db: Db,
    Form(form): Form<LoginForm>,
) -> AutumnResult<Response> {
    let email = form.email.trim().to_lowercase();

    // Reject over-long inputs before any DB query or bcrypt work — they can
    // never match a stored account and only waste CPU.
    if email.len() > 254 || form.password.len() > 128 {
        return Err(AutumnError::unauthorized_msg("Invalid email or password"));
    }

    let user: Option<User> = users::table
        .filter(users::email.eq(&email))
        .select(User::as_select())
        .first(&mut *db)
        .await
        .optional()?;

    let invalid = || AutumnError::unauthorized_msg("Invalid email or password");
    let user = match user {
        Some(u) => u,
        None => {
            // Always run a verification so response time is constant whether or
            // not the address is registered.
            let _ = verify_password(&form.password, DUMMY_HASH).await;
            return Err(invalid());
        }
    };
    if !verify_password(&form.password, &user.password_hash).await? {
        return Err(invalid());
    }

    session.rotate_id().await;                              // session fixation
    session.insert("user_id", user.id.to_string()).await;   // the identity claim
    session.insert("role", &user.role).await;

    Ok(Redirect::to("/dashboard").into_response())
}
```

Three properties are worth naming explicitly:

- **Session fixation.** An attacker who can plant a known session id in a
  victim's browser inherits their session the moment they log in — unless the id
  changes at that boundary. `rotate_id()` on login (and on password reset, and
  after any privilege elevation) closes that window; the old server-side record
  is deleted on save.
- **Non-enumeration.** Wrong password, unknown address, unconfirmed account, and
  locked account all return the *same* status and body. The dummy-hash
  verification removes the dominant timing signal — without it, an unknown
  address skips bcrypt entirely and returns in a fraction of the time, which is
  a free "this email is not registered" oracle.

  It narrows the gap rather than closing it. A wrong password against a *real*
  account still writes a `failed_attempts` update, and a successful or
  confirmed login writes a counter reset, where an unknown address returns
  straight after the dummy verify with no write at all. That residual
  difference is a DB round-trip, not a bcrypt cost, but it is measurable in
  aggregate — so pair it with [rate limiting](./rate-limiting.md) on `/login`
  rather than treating the timing as truly uniform.
- **Bounded work.** Cap input lengths before touching the database or bcrypt.

Logout tears the session down and, if you issue them, revokes the device's
[remember chain](#remember-me) first:

```rust,ignore
#[post("/logout")]
pub async fn logout(
    session: Session,
    State(state): State<AppState>,
    mut db: Db,
    headers: HeaderMap,
) -> AutumnResult<Response> {
    let config = state.config_arc();
    let remember_cfg = &config.auth.remember;

    // Drop this device's tracking row, then revoke its remember chain — before
    // the session teardown, and only a no-op when neither is in play. Skipping
    // this is the classic bug: the session dies, the remember cookie survives,
    // and the next request logs the browser straight back in.
    let _ = untrack_current_session(&mut db, &session).await;
    revoke_remember_from_cookie(&mut db, remember_cfg, &headers).await;

    session.clear().await;
    session.rotate_id().await;   // old cookie can no longer be replayed

    let mut response = Redirect::to("/").into_response();
    append_set_cookie(&mut response, &build_remember_clear_cookie(remember_cfg));
    Ok(response)
}
```

`clear()` + `rotate_id()` keeps a fresh session available to carry a one-shot
[flash message](./flash.md) to the next page while still destroying the old
record. Use `destroy()` when you also want the cookie expired outright.

If you issue neither remember cookies nor tracking rows, the two helper calls
and the `Set-Cookie` drop out and logout really is just `clear()` +
`rotate_id()`. Add them back the moment you turn remember-me on.

**Logout is best-effort against database failure.** Both generated helpers
swallow their delete errors (`let _ = …`) and return `()`, so the handler has
nothing to propagate: if the `DELETE` fails, the browser still gets a cleared
cookie and a destroyed session, but the remember chain survives server-side and
a copy of that cookie can still authenticate later. The session teardown itself
is unaffected. If your threat model does not tolerate that, delete the chain
yourself with a checked query and fail the logout — or queue a
[durable job](./jobs.md) to retry the deletion — instead of relying on the
best-effort helper.

Working code: [`examples/saas/src/routes/auth.rs`](../../examples/saas/src/routes/auth.rs)
(signup with policy enforcement, [submit tokens](./submit-tokens.md), and
remember-me) and
[`examples/reddit-clone/src/routes/auth.rs`](../../examples/reddit-clone/src/routes/auth.rs)
(signup + login inside a transaction, with events and flash).

---

## Protecting routes

### `#[secured]`

```rust,ignore
#[get("/dashboard")]
#[secured]
async fn dashboard() -> AutumnResult<Markup> { /* … */ }

#[get("/admin")]
#[secured("admin")]                    // 403 unless session["role"] == "admin"
async fn admin_panel() -> AutumnResult<Markup> { /* … */ }

#[post("/admin/purge")]
#[secured("admin", "moderator")]       // any of the listed roles
async fn purge() -> AutumnResult<Markup> { /* … */ }
```

The macro injects hidden `Session` and `AppState` extractors and a check at the
top of the handler body. It resolves the identity key through
`AppState::auth_session_key()`, so `[auth].session_key` (default `"user_id"`)
governs `#[secured]`, `#[authorize]`, `#[step_up]`, and the generated handlers
uniformly — change it in one place.

Missing key → `401 Unauthorized`. Present but wrong role → `403 Forbidden`.

Two side effects worth knowing: a passing check publishes the user as the
request's [current actor](./audit-logging.md) — so repository and audit writes
auto-attribute — and tags the request-scoped log context with `user_id`, so
every subsequent log line carries it. When a request is already authenticated by
a stronger principal (a bearer API token, via `RequireApiToken`), the token
principal wins and is not clobbered by the session user.

For token-authenticated routes, `#[secured(scopes = ["posts:write"])]` is
default-deny: every required scope must be present in the presenting token's
grants.

See [macro transparency](./macro-transparency.md#securedrole) for the exact
expansion.

### `RequireAuth` and `Auth<T>`

`#[secured]` is per-handler. To gate a whole subtree, layer `RequireAuth`, which
rejects requests lacking the session key before they reach any handler:

```rust,ignore
use autumn_web::auth::RequireAuth;

let admin = admin_router.layer(RequireAuth::new("user_id"));
```

`Auth<T>` extracts a fully-loaded user that middleware has already placed in
request extensions, and returns `401` when it is absent:

```rust,ignore
use autumn_web::auth::Auth;

#[get("/profile")]
async fn profile(Auth(user): Auth<CurrentUser>) -> String {
    format!("Hello, {}!", user.name)
}
```

`#[secured]` answers "is anyone logged in?"; `Auth<T>` hands you the row.
Neither decides whether that user may touch *this* record — that is
[`#[authorize]` and `Policy`](./authorization.md).

---

## Account lockout

IP rate limiting does not stop credential stuffing that rotates source IPs. The
generated login handler therefore also counts failures **per account**:

1. Each failed login increments `failed_attempts` on the account row.
2. At `threshold`, `locked_at` is stamped and a `tracing::warn!` fires with
   `event = "account_locked"`, a salted SHA-256 account digest truncated to 8
   bytes, and an IP prefix (IPv4 /24, IPv6 /64) — correlatable across log lines
   for incident response without putting a raw account id in the logs.

   **Set the salt.** The digest is salted from `SECRET_KEY_BASE`, falling back
   to `AUTUMN_ADMIN_SECRET`, falling back to a compiled-in constant. With
   neither variable set, the salt is public and account ids are small sequential
   integers, so anyone holding the logs can hash candidates and recover the id.
   Export one of the two in production — note this digest does not read
   `AUTUMN_SECURITY__SIGNING_SECRET`, so provisioning only the
   [signing secret](./signing-secrets.md) leaves the fallback in place.
3. While locked, *every* attempt — including the correct password — returns the
   same response as a wrong password, so the endpoint never reveals which
   accounts are locked.
4. A successful login clears the counter and the lock; the account auto-unlocks
   on the first successful login after the cool-off elapses.

```toml
[auth.lockout]
enabled      = true   # false → disable entirely (external policy in place)
threshold    = 10     # consecutive failures before lockout; 0 also disables
window_secs  = 60     # reserved for future sliding-window counting
cooloff_secs = 900    # 15 minutes
```

Each maps to an env override (`AUTUMN_AUTH__LOCKOUT__THRESHOLD`, …). Lockout
state is columns on the account row, so it lives in whichever database the app
is configured against and every replica sharing that database agrees. On a
[SQLite](./sqlite-in-production.md) deployment where each replica has its own
file, the counters are per-replica and the lockout threshold is effectively
multiplied by the replica count.

`window_secs` is not yet enforced — failures currently accumulate since the last
successful login rather than over a sliding window.

For operator recovery the generator emits `POST /auth/admin/unlock`, guarded by
an `X-Admin-Secret` header matched against `AUTUMN_ADMIN_SECRET`. Set that to a
strong random value and put network-level controls in front of the route; if the
variable is unset the endpoint always refuses.

Complementary, not redundant: pair lockout with
[`#[throttle]`](./rate-limiting.md) on `/login` and
[CAPTCHA](./bot-protection.md) for the volumetric side of the same attack.

---

## Remember me

A "keep me signed in" checkbox that outlives the session cookie is a long-lived
bearer credential. Autumn implements the Jaspan rotating-token scheme (the model
Spring Security uses) so a stolen cookie is detectable and self-limiting.

A credential is a **`(series, token)`** pair carried as `series:token` in a
separate cookie:

- `series` is stable for one device's login chain — it is the database lookup key.
- `token` **rotates on every use**. Only its SHA-256 hash is ever stored.

On each request presenting the cookie, look up the record by series and call the
pure decision function:

```rust,ignore
use autumn_web::auth::{RememberDecision, default_rotation_grace, evaluate_remember};

match evaluate_remember(&presented_token, record.as_ref(), now, default_rotation_grace()) {
    RememberDecision::Rotate => { /* mint a new token, store its hash, extend expiry, re-cookie, log in */ }
    RememberDecision::Accept => { /* previous token inside the grace window: log in, do not rotate */ }
    RememberDecision::Theft  => { /* delete the whole chain for this series; stay unauthenticated */ }
    RememberDecision::Reject => { /* unknown or expired series: clear the cookie */ }
}
```

`Theft` is the point of the scheme: replaying a rotated-out token for a *known*
series can only mean two parties hold the chain, so the whole series is deleted
and the cookie cleared — neither party can use remember-me to authenticate
again. The 60-second rotation grace (`DEFAULT_ROTATION_GRACE_SECS`) keeps
concurrent requests that raced a rotation from false-firing that alarm.

Scope that precisely: `Theft` revokes the **remember chain**, not existing
logins. The generated middleware deletes the series and clears the cookie on
that one unauthenticated request; it does not destroy session records or delete
[tracking rows](#active-sessions-and-revocation), and it skips remember
processing entirely for a request that already carries a session identity. A
victim and an attacker who each hold a live session cookie both stay logged in
until those sessions end. If a theft signal should log everyone out, pair it
with `revoke_all_sessions` on that account.

```toml
[auth.remember]
enabled       = true              # issue remember cookies on login
duration_secs = 2592000           # 30 days; also the sliding expiry on rotation
cookie_name   = "autumn.remember"
```

Because `evaluate_remember` is pure and deterministic, the security-critical
decision is unit-testable against fixed timestamps with no I/O.

Rules of thumb: mint a chain only on explicit opt-in; revoke this device's chain
in the logout handler **before** tearing down the session, or a stolen cookie
re-establishes the login; and treat a remember-authenticated request as *weakly*
authenticated — send it through [step-up](./step-up-authentication.md) before
anything sensitive.

End-to-end wiring: [`examples/saas/src/remember.rs`](../../examples/saas/src/remember.rs).

---

## Active sessions and revocation

Cookie expiry alone cannot answer "sign me out of that other laptop". The
generated auth stack persists one row per login, keyed by the **digest** of the
session id, holding the user, login IP, parsed user-agent, an optional device
label, `created_at`, and `last_seen_at`.

Revoking a device deletes its row. **That deletion is enforced only where a
handler looks the row up** — `require_tracked_session(&session, &mut db,
&state)`, which loads the row by digest and, when it is gone, destroys the
cookie session and rejects with `401`. The generated auth routes call it; so
must yours.

This is the part to get right, because the failure mode is silent:

```rust,ignore
// Enforces revocation — the row is checked on every request.
#[get("/dashboard")]
async fn dashboard(session: Session, mut db: Db, State(state): State<AppState>)
    -> AutumnResult<Markup>
{
    let user = require_tracked_session(&session, &mut db, &state).await?;
    // …
}

// Does NOT enforce revocation. `#[secured]` checks only that the session
// carries the auth key, so a revoked device keeps reaching this route until it
// hits a handler that does the lookup.
#[get("/dashboard")]
#[secured]
async fn dashboard() -> AutumnResult<Markup> { /* … */ }
```

`#[secured]` and `RequireAuth` answer "is there a session key?", not "is this
session still authorized to exist" — revocation deletes the tracking row, not
the record in the session store. If revocation must hold across your whole app,
call `require_tracked_session` in every authenticated handler, or wrap the check
in [middleware](./middleware.md) layered over the authenticated subtree so no
route can forget it.

`last_seen_at` is refreshed at most once per window, bounding a busy session to
one `UPDATE` per window rather than one per request. The raw session id never
reaches the database, so a database leak cannot be replayed as a cookie.

`GET /account/sessions` renders the device list with per-row revoke, a device
label form, and "sign out everywhere else" (htmx in-place swaps, with plain form
posts as the no-JavaScript fallback).

```toml
[auth.sessions]
revoke_on_credential_change = true  # password change/reset, TOTP enroll/disable, passkey add/remove
last_seen_update_secs       = 60
```

Leave `revoke_on_credential_change` on unless an external policy handles it: the
standard response to credential theft is to invalidate every *other* session.

The stored IP and user-agent are personal data. Delete rows on logout,
revocation, and account deletion; scrub stale rows on a schedule aligned with
`session.max_age_secs`; truncate IPs if full addresses are more than you need;
and include the table in your GDPR export.

---

## Testing authenticated routes

The test client can mint an authenticated session directly, so a test of a
protected route does not have to drive the login form:

```rust,ignore
let client = TestApp::new().routes(routes![dashboard]).build();

client.acting_as(42).await;               // alias: login_as(42)
client.get("/dashboard").send().await.assert_ok();

client.log_out();
client.get("/dashboard").send().await.assert_status(401);
```

`acting_as` sets **identity only** — authorization still runs, so a user who
lacks the required role or scope is still denied. It needs a session-store
handle, which means a client built via `TestApp::build()` on the default memory
backend (not `TestApp::from_router`, and not a Redis-backed one).

Test the real login handler too, at least once: assert that the session cookie
changes on login (fixation), that a wrong password and an unknown address are
indistinguishable, and that logout makes the old cookie unusable. See the
[testing guide](./testing.md).

---

## Production checklist

- [ ] A [signing secret](./signing-secrets.md) is provisioned. The production
      profile fails fast without one, so this is really a check on every
      *other* environment you care about: without it the router installs no
      signing keys and session cookies carry unsigned ids.
- [ ] `session.backend = "redis"` (with the `redis` feature) for any
      multi-replica or restart-sensitive deployment, or
      `allow_memory_in_production = true` set deliberately.
- [ ] `session.secure = true`, `http_only = true`, `same_site = "Lax"` (or
      `"Strict"`), and a `max_age_secs` you actually want.
- [ ] Login, signup, and password reset all call `rotate_id()`.
- [ ] Login responses are non-enumerating in body and status, with the
      dummy-hash verify in place so timing does not separate known from unknown
      addresses by a bcrypt's width.
- [ ] `[auth.password]` reviewed; consider `breach_check = "fail_open"`.
- [ ] `[auth.lockout]` left enabled, `AUTUMN_ADMIN_SECRET` set, and the unlock
      route network-restricted. That variable (or `SECRET_KEY_BASE`) also salts
      the `account_locked` log digest — without either, the digest is reversible.
- [ ] Logout revokes the remember chain and the tracking row, not just the
      session.
- [ ] Every authenticated route reaches `require_tracked_session` (directly or
      via middleware) if you rely on session revocation — `#[secured]` alone
      does not enforce it.
- [ ] `/login` and `/forgot-password` carry [`#[throttle]`](./rate-limiting.md).
- [ ] Sensitive routes carry [`#[step_up]`](./step-up-authentication.md).
- [ ] `autumn routes audit` shows no unintentionally public route — see the
      [security posture manifest](./security-posture-manifest.md).
- [ ] Secrets (OAuth client secrets, admin secret) come from the environment or
      the [credentials store](./credentials.md), not `autumn.toml`.

---

## Where to go next

- [OAuth2 / OIDC](./oauth.md) — social and enterprise sign-in, provider presets,
  and account-linking policy.
- [Step-up authentication](./step-up-authentication.md) — `#[step_up]` for
  sudo-mode re-verification before destructive actions.
- [Authorization](./authorization.md) — `Policy`, `Scope`, and `#[authorize]`
  for "may this user touch this record?".
- Multi-factor — `autumn generate auth --totp | --passkeys | --magic-link`
  writes the flows and their own project-local docs.
- [Rate limiting](./rate-limiting.md) and [bot protection](./bot-protection.md)
  — the volumetric half of credential-stuffing defence.
- [Submit tokens](./submit-tokens.md) — at-most-once signup and reset forms.
- [Signing secrets](./signing-secrets.md) — the key behind session cookies, CSRF
  tokens, and flash state.
- [Middleware](./middleware.md) — where the session, CSRF, and security-header
  layers sit in the stack.
- [Audit logging](./audit-logging.md) — the actor `#[secured]` publishes, and
  how writes get attributed to it.
- [Testing](./testing.md) — `acting_as` / `login_as` and the request-level
  assertions.
- [Coming from other frameworks](./coming-from-other-frameworks.md) — Devise,
  `authenticate_user!`, Spring Security, and `@login_required` mapped onto
  Autumn.
- Rustdoc: [`autumn_web::session`](../../autumn/src/session.rs),
  [`autumn_web::auth`](../../autumn/src/auth.rs),
  [`autumn_web::auth::password`](../../autumn/src/auth/password.rs),
  [`autumn_web::auth::remember`](../../autumn/src/auth/remember.rs).

## Sessions as authoritative edge identity

Edge authentication has separate verification, authoritative storage, and claim
projection stages ([ADR-0005](../adr/0005-edge-session-identity.md)). Redis is the
first-party shared path; DragonflyDB and Valkey use the same `[session.redis]`
URL/prefix configuration and `RedisStore`, not backend-specific store types. The
adapter uses only `GET`, TTL-bearing `SET`, and `DEL`.

Choose consistency deliberately: logout/revocation requires the authoritative
store's current answer. An opportunistic cache is not an authority—eviction is a
miss, and stale hits could retain revoked access. Cross-region authority adds
latency; colocate it or let identity-required requests fall through to origin.
Custom stores implement `SessionStore`; custom identity sources implement
`EdgeIdentityProvider`, keeping cookie and signing rules out of plugins.
