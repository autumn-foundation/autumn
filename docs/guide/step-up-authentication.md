# Step-Up Authentication (Sudo Mode)

`#[secured]` answers "is the user logged in?" — it ensures a valid session
exists but makes no claim about *how recently* the user proved their identity.
`#[step_up]` answers a stricter question: **"did the user re-prove their
identity within the last N minutes?"**

This guide covers:

- [Why step-up exists](#why-step-up-exists) — the threat model.
- [The `#[step_up]` attribute macro](#the-step_up-attribute-macro).
- [Configuration](#configuration) — global defaults and per-route overrides.
- [The reauth flow](#the-reauth-flow) — what happens on a stale session.
- [JSON / API clients](#json--api-clients) — `WWW-Authenticate: StepUp`.
- [Audit events](#audit-events) — SOC 2 evidence collection.
- [Admin plugin](#admin-plugin) — protecting mutating admin actions.
- [Step-up vs. session rotation](#step-up-vs-session-rotation-819) —
  knowing which primitive to reach for.
- [When to use each](#when-to-use-each).

---

## Why Step-Up Exists

**The threat**: A user logs in at 9 AM, leaves their laptop unlocked at 12 PM.
`#[secured]` routes are fully open to whoever sits down. Session hijacking via
XSS cookie exfiltration creates the same window.

**The fix**: Sensitive operations — account deletion, MFA removal, email
change, credential export — require a *fresh proof of identity* (password or
second factor) recorded in the session within the last N minutes. The window
is short enough to be meaningful and long enough not to be annoying.

Step-up is **not** session rotation (#819). Rotating the session ID prevents
fixation attacks at login; it does not re-verify identity after the fact.
Step-up and session rotation are complementary and address different threats.

---

## The `#[step_up]` Attribute Macro

Add `#[step_up]` (or `#[step_up(max_age = "5m")]`) above any `async fn` handler:

```rust,ignore
use autumn_web::prelude::*;

#[secured]
#[step_up]
#[post("/account/destroy")]
pub async fn account_destroy(session: Session, mut db: Db) -> AutumnResult<Response> {
    // Only runs if the user authenticated (via #[secured]) AND proved their
    // identity within the step-up window (via #[step_up]).
    // ...
}
```

The macro injects four hidden extractors (`Session`, `AppState`, `Uri`,
`HeaderMap`) to read the `last_strong_auth_at` session claim and decide
whether to redirect or return a problem-details response.

### Order of attributes

Put `#[step_up]` **after** `#[secured]` so the authentication check happens
first, then the freshness check. Putting them in reverse order is harmless but
less readable.

### Custom `max_age`

```rust,ignore
// Require re-auth within the last 5 minutes (default).
#[step_up]

// Require re-auth within the last 10 minutes.
#[step_up(max_age = "10m")]

// Require re-auth within the last 2 hours.
#[step_up(max_age = "2h")]
```

Supported units: `s` (seconds), `m` (minutes), `h` (hours).
The default is 5 minutes (300 seconds) unless overridden in `autumn.toml`.

---

## Configuration

Set the app-wide default in `autumn.toml`:

```toml
[auth.step_up]
default_max_age_secs = 300   # 5 minutes — the framework default
```

Any `#[step_up(max_age = "...")]` annotation on a specific route takes
precedence over this global default.

---

## The Reauth Flow

When a browser client hits a step-up-protected route with a missing or stale
`last_strong_auth_at` claim, they are redirected to:

```
/reauth?return_to=%2Faccount%2Fdestroy
```

The generated auth scaffold includes a `/reauth` endpoint that:

1. Presents a password (and TOTP/passkey, if enrolled) prompt.
2. Verifies the credential.
3. Writes `last_strong_auth_at` to the session with the current timestamp.
4. Redirects to `return_to` (validated to be same-origin — no open redirect).

### Same-origin validation

`return_to` is rejected if it:
- Does not start with `/`, or
- Starts with `//` (protocol-relative URL that would leave the origin).

---

## JSON / API Clients

Clients sending `Accept: application/json` receive a
[Problem Details](https://www.rfc-editor.org/rfc/rfc7807) response instead
of a redirect:

```
HTTP/1.1 401 Unauthorized
WWW-Authenticate: StepUp max-age=300
Content-Type: application/problem+json

{
  "type": "https://autumn.rs/probs/step-up-required",
  "title": "Step-Up Authentication Required",
  "status": 401,
  "detail": "This operation requires recent authentication. Please re-authenticate and retry.",
  "code": "step_up_required"
}
```

The `WWW-Authenticate: StepUp max-age=300` header signals the time window
to API clients and service-to-service consumers.

---

## Audit Events

Every step-up check emits a structured audit event through the framework's
`AuditSink`:

| Event name | When |
|---|---|
| `auth.step_up.success` | Claim present and fresh; handler allowed to proceed |
| `auth.step_up.failure` | Claim absent or stale; redirect / 401 issued |

These events include the authenticated user ID (`actor_id`) automatically
using the same session key as `#[secured]`. They are visible in any
`AuditSink` implementation (database logger, structured log sink, etc.),
making them straightforward evidence for SOC 2 access control criteria.

---

## Admin Plugin

The `autumn-admin-plugin` can protect every mutating admin action (create,
update, destroy) with step-up via the `with_step_up_mutations` builder flag:

```rust,ignore
AdminPlugin::new("Admin", "admin")
    .with_step_up_mutations()   // guards POST / PUT / PATCH / DELETE
    .build()
```

This wraps the entire admin router with a Tower middleware that checks the
`last_strong_auth_at` claim before any mutating request is dispatched.

---

## Step-Up vs. Session Rotation (#819)

| | `Session::rotate_id()` | `#[step_up]` |
|---|---|---|
| **Threat addressed** | Session fixation (attacker plants cookie before login) | Session hijacking / shoulder-surfing after login |
| **When it runs** | At login (and after password reset) | Before each sensitive handler |
| **What it checks** | Nothing — it just issues a new session ID | Age of `last_strong_auth_at` claim in session |
| **Requires user action** | No | Yes — re-enter password/second factor |
| **Configured via** | Hardcoded in login flow | `[auth.step_up]` in `autumn.toml` + per-route `max_age` |

Use **`rotate_id`** at login to invalidate any pre-session the attacker may
have planted. Use **`#[step_up]`** before destructive or privilege-changing
operations so a hijacked or abandoned session cannot exercise them without
re-proving identity.

---

## When to Use Each

| Operation | `#[secured]` | `#[step_up]` | Notes |
|---|---|---|---|
| View account profile | ✅ | — | Reading data; no step-up needed |
| Log out | ✅ | — | Any authenticated user can log out |
| Change email | ✅ | ✅ | Privilege change |
| Change password | ✅ | ✅ | Credential change |
| Delete account | ✅ | ✅ | Irreversible; generated scaffold adds it |
| Enable / disable MFA | ✅ | ✅ | Security-setting change |
| Export GDPR data | ✅ | ✅ | Sensitive data access |
| View credentials store | ✅ | ✅ | Encrypted secrets |
| Admin create / update / delete | ✅ | opt-in via `with_step_up_mutations()` | |

**Rule of thumb**: if the action is irreversible, changes credentials or
security settings, or exposes sensitive data, add `#[step_up]`.

## See also

- [Authentication](./authentication.md) — the session, password, and
  `#[secured]` layer that `#[step_up]` tightens, including the remember-me
  chains that make a "logged in" request only weakly authenticated.
- [Authorization](./authorization.md) — `#[authorize]` for "may this user touch
  this record?", the question orthogonal to freshness.
