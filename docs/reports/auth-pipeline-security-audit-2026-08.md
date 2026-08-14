# Authentication Pipeline Security Audit — 2026-08-14

Scope: the full authentication pipeline in `autumn-web` and the code emitted by
`autumn generate auth` —

- `autumn/src/auth.rs` (password hashing, `RequireAuth`, `RequireApiToken`,
  OAuth2/OIDC, API token stores), `auth/password.rs`, `auth/remember.rs`
- `autumn/src/session.rs`, `session_redis.rs`, `entropy.rs`
- `autumn/src/security/` (`csrf.rs`, `config.rs` signing keys, `headers.rs`,
  `trusted_proxies.rs`, `submit_token.rs`, `rate_limit.rs` config surface,
  `captcha.rs`), `autumn/src/step_up.rs`
- `autumn-cli/src/generate/auth.rs` (generated login / logout / lockout /
  forgot- & reset-password / magic-link / TOTP / remember-me / session-tracking
  handlers), `autumn-cli/src/routes_audit.rs`

Method: manual line-by-line review of the above, checked against OWASP ASVS
session-management / authentication requirements and known attack classes
(fixation, enumeration, timing, algorithm confusion, CSRF variants, replay,
TOCTOU).

## Verdict

The pipeline is in very good shape. No critical or high-severity issues were
found. The design repeatedly makes the hard-but-right choice: tokens hashed at
rest everywhere, constant-time comparison at every secret check but one,
session-id rotation at every privilege boundary, atomic single-use token
consumption, JWT algorithm pinning derived from trusted key material, TOTP
step replay guards, and non-enumerating responses with timing equalization on
the email-based flows. The findings below are two medium-severity gaps and a
set of low-severity hardening items.

---

## Medium

### M1. CSRF cookie is never set with `Secure` (and the token is not bound to the session)

`security/csrf.rs` builds the cookie as:

```text
{name}={token}; Path=/; SameSite=Lax; HttpOnly
```

unconditionally — there is no `Secure` attribute, in any profile. The session
cookie defaults to `Secure` (and prod smart-defaults force it), the remember
cookie sets it conditionally, but the CSRF cookie can always be set or
overwritten over plaintext HTTP by an active network attacker.

Because the scheme is double-submit (cookie value must equal the
header/form/query value), a planted cookie plus a matching form value passes
validation. HMAC signing raises the bar but does not close it: tokens are not
bound to a session, so *any* legitimately-signed token (e.g. one the attacker
minted by visiting the site) verifies for any victim it can be planted on.

Recommendations, in increasing strength:
1. Add `Secure` when the deployment is HTTPS (mirror `session.secure` /
   the prod smart default).
2. Use the `__Host-` cookie prefix by default so no subdomain or non-secure
   context can plant it.
3. Bind the token to the session: sign `{uuid}:{session_id}` rather than the
   bare uuid, and verify against the current session id. This converts
   double-submit into a session-bound synchronizer-equivalent and defeats
   planted-cookie attacks entirely.

### M2. Operator-unlock endpoint compares the admin secret non-constant-time

Generated `POST /auth/admin/unlock` (`generate/auth.rs`):

```rust
if admin_secret.is_empty() || provided != admin_secret {
```

This is the only secret comparison in the whole pipeline that uses `!=`
instead of a constant-time compare — everywhere else uses `subtle` or the
`constant_time_eq` helpers. `AUTUMN_ADMIN_SECRET` also has no minimum-length
validation, unlike the signing secret (32-byte minimum + weak-value denylist).

Recommendation: compare via `autumn_web::auth::constant_time_eq`, and consider
running the value through the same `validate_signing_secret`-style checks at
startup. (The endpoint is otherwise well-designed: non-enumerating response,
empty-secret fail-closed, documented CSRF-exemption requirement.)

---

## Low / hardening

### L1. `remember_secure()` reads `X-Forwarded-Proto` directly, bypassing `ProxyResolver`

`generate/auth.rs` decides the remember cookie's `Secure` attribute from a raw
`x-forwarded-proto` header read. `security/trusted_proxies.rs` explicitly
forbids this ("Never read X-Forwarded-* directly"), and the consequence is
concrete: a deployment that terminates TLS in-process (the `acme` feature)
sends no forwarded headers, so the remember cookie is **never** marked
`Secure` there. Use `ClientScheme` / the resolver, or key off `session.secure`
config like the session cookie does.

### L2. Generated handlers use the TCP peer IP, not the resolved client IP

`MaybeClientIp` reads `ConnectInfo` directly. Behind a proxy/LB, the
active-sessions device list, remember-token rows, and lockout telemetry all
record the proxy's IP. This fails safe (unspoofable) but shows users wrong
"device" IPs and collapses lockout telemetry to one IP. Route it through the
framework's `ClientAddr`/`ProxyResolver` seam instead.

### L3. `/login` and `/forgot-password` have no request throttle

The magic-link routes carry `#[throttle(limit = 5, per = "1m", key = "ip")]`
plus a per-email re-mint cooldown; the password login and forgot-password
routes have neither (and global rate limiting is off by default). Account
lockout covers per-account brute force, but:
- one IP can spray one password across many accounts without friction;
- `/forgot-password` can be used to email-bomb a victim (each request re-mints
  and re-sends; the 1s timing pad is the only backpressure).

Recommendation: mirror the magic-link posture — `#[throttle]` on both, and a
per-email cooldown on forgot-password.

### L4. Small enumeration timing channel on login lockout bookkeeping

Login equalizes bcrypt timing with a dummy hash (good), but a failed attempt
on an *existing* account then performs 1–2 `UPDATE`s (failure counter /
lock stamp) that an unknown-email attempt skips. That re-opens a millisecond-
scale timing signal after the equalization. Forgot-password pads the whole
handler to a 1s floor; doing the same on the login failure path (or performing
a dummy write) would close it.

### L5. `MemoryStore` sessions have no server-side expiry

Cookie `Max-Age` is the only lifetime; the in-memory store never evicts, so a
leaked session id stays valid until restart and long-lived processes grow
unboundedly. It is dev-only (prod boot warns), but a periodic sweep with a
stored expiry would make the default backend safe even when the warning is
ignored (`allow_memory_in_production = true` exists, after all). The Redis
backend already applies `SET EX max_age_secs` correctly.

### L6. `trust_forwarded_headers = true` with no ranges/hops trusts every peer

`ProxyResolver`: when forwarding trust is enabled but neither CIDR ranges nor
`trusted_hops` are configured, every peer is trusted and the rightmost XFF
entry wins — so a *direct* client can spoof its rate-limit/lockout identity by
sending its own `X-Forwarded-For`. The dev default (loopback-only) and prod
default (no trust) are both safe; this is purely the misconfiguration corner.
A startup warning for "forwarding trusted but no boundary declared" would
close the footgun.

### L7. Custom profiles silently lose the prod security smart-defaults

`staging`, `qa`, etc. get no smart defaults, so CSRF stays disabled and HSTS
off unless configured — an operator who believes "staging is prod-like" ships
without CSRF. Consider a boot note when a non-`dev` profile runs with
`csrf.enabled = false`.

### L8. bcrypt's 72-byte truncation is silent

`hash_password` passes straight to bcrypt, which truncates input at 72 bytes.
Two passwords sharing the first 72 bytes verify identically. Standard bcrypt
behavior, but worth either documenting on `hash_password` or rejecting >72-byte
passwords in `validate_password` (a max-length knob would also satisfy the
ASVS "no silent truncation" requirement).

### L9. Generated `read_cookie` accepts duplicate cookie names

The framework's `session::get_cookie` and CSRF extractor reject duplicate
same-name cookies (cookie-tossing defense); the generated `read_cookie` used
for the remember cookie takes the first match. Theft-detection limits the
impact, but the hygiene should match.

### L10. CSRF token accepted from the URL query string

`?_csrf=...` is accepted as a token carrier. Tokens in URLs leak into access
logs, browser history, and (partially) Referer. The default Referrer-Policy
mitigates cross-origin leakage; still, consider dropping query-string
acceptance or gating it behind config.

---

## Notable strengths (verified, not assumed)

- **Password storage**: bcrypt cost 12 via `spawn_blocking`; dummy-hash timing
  equalization on both invalid-format hashes and unknown users (both dummy
  hashes are well-formed 60-char `$2b$12$` values, so the equalizing verify
  really runs).
- **OIDC**: PKCE S256 always on; state validated constant-time and *not*
  consumed on mismatch (attacker callbacks can't burn the pending login);
  nonce required for id_token logins; verification algorithms pinned from the
  JWK (never the token header) with HS*/`alg=none` rejected — with real
  algorithm-confusion attack tests.
- **Sessions**: server-side store, UUIDv4 ids from the OS CSPRNG, optional
  HMAC-signed cookies with key rotation, duplicate-cookie (tossing) rejection,
  id rotation at login/reset/magic-link/OAuth completion and logout, old id
  destroyed in the store on rotation, deadline-cancelled requests never persist
  partial session state, attacker-chosen cookie ids are never adopted (a miss
  mints a fresh id).
- **Signing secrets**: 32-byte production minimum, template-value denylist,
  fail-fast boot validation, previous-key rotation grace.
- **Reset/magic-link/confirm tokens**: 256-bit OS-random, digest-only at rest,
  bounded TTLs, atomic single-use consumption (`UPDATE ... WHERE consumed_at
  IS NULL RETURNING`), non-enumerating responses with 1s timing floors,
  link-scanner-safe GET-then-POST consumption, lockout re-checked *after*
  token consumption to close the TOCTOU race, magic-link deliberately denied
  step-up freshness (email possession ≠ password knowledge).
- **Remember-me**: Jaspan series/token scheme, hash-at-rest, constant-time
  verify, CAS rotation with race re-evaluation, theft detection nukes the
  chain, credential changes always revoke remember chains, `reauth_pw_ok`
  cleared so an email-only login can't shortcut a password reauth.
- **Lockout**: atomic counters, non-enumerating lock responses, cool-off with
  clean re-entry, success-path guarded against concurrent locking, salted
  digest + truncated IP prefix in telemetry.
- **TOTP**: per-step replay guard (`totp_last_used_step` CAS).
- **API tokens**: SHA-256 digest-only at rest (appropriate for 244-bit random
  tokens), scoped default-deny, atomic CTE rotation, blank-seed-token guard,
  scopes carried in request extensions rather than the session (no cookie
  leakage onto later requests).
- **CSRF layer mechanics**: normalized-path exemption matching (dot-segment
  tricks can't reach an exemption), bounded body scan with lossless streaming
  reconstruction, boundary parsing shared with the real multipart extractor.
- **Headers**: CSP without `unsafe-eval`, `frame-ancestors 'none'`,
  `X-Frame-Options: DENY`, nosniff, HSTS prod default.
- **Verifiability**: `autumn routes audit` emits a fail-closed security
  manifest — every route must be provably `framework`/`gated`/`public` or the
  build gate fails — with honest provenance tags (`provable` vs `declared` vs
  `runtime-only`). Session cookie parsing is additionally fuzzed
  (`fuzz/fuzz_targets/session.rs`).

## Suggested fix order

1. M1 (CSRF cookie `Secure` + `__Host-`; session binding as follow-up)
2. M2 (constant-time admin-secret compare) — one-line fix
3. L1/L2 (route generated code through the ProxyResolver seams)
4. L3 (throttle login/forgot-password)
5. Remaining L-items opportunistically.
