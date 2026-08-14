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
the email-based flows. The findings below are three medium-severity gaps and
a set of low-severity hardening items.

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

This is the only plain `!=` on a long-lived secret in the pipeline — token,
state, nonce, and cookie comparisons all go through `subtle` or the
`constant_time_eq` helpers. (The generated TOTP code compare is the other
non-constant-time comparison, on a short-lived code — see L14.)
`AUTUMN_ADMIN_SECRET` also has no minimum-length
validation, unlike the signing secret (32-byte minimum + weak-value denylist).

Recommendation: compare via `autumn_web::auth::constant_time_eq`, and consider
running the value through the same `validate_signing_secret`-style checks at
startup. (The endpoint is otherwise well-designed: non-enumerating response,
empty-secret fail-closed, documented CSRF-exemption requirement.)

### M3. The TOTP second-factor endpoint is brute-forceable (no throttle, no attempt limit)

Generated `POST /login/verify` has neither a `#[throttle]` attribute nor any
failed-attempt counter, and a failed guess returns "Invalid code." while
leaving `totp_pending_id` in the session — so the pending login survives
unlimited retries. An attacker who has the password (precisely the scenario
2FA defends against) can therefore machine-guess the 6-digit code: ~10⁶
values, with the ±1-step window accepting three codes per attempt, and no
rate limit in the way. The account-lockout counters do not apply here (they
are only touched in `login`).

Each miss additionally loads **every** unused recovery code and runs a
cost-12 bcrypt verify against each, so the same endpoint is a CPU
amplification vector (~1 s of server CPU per guess with a default code set)
— painful under load even when the guessing itself fails.

The `reauth` TOTP branch shares the same shape (unlimited tries, full
recovery-code bcrypt sweep per miss), though it at least demands the password
on every attempt.

Fix: throttle `/login/verify` like the magic-link routes
(`#[throttle(limit = 5, per = "1m", key = "ip")]`), add a small
per-pending-session attempt counter (e.g. 5 tries, then clear
`totp_pending_id` and force re-login), and only fall through to the
recovery-code sweep when the submitted value does not look like a 6-digit
code. Apply the same attempt bound to the `reauth` branch. *(Credit: flagged
by automated review on the audit PR and verified against the code.)*

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
per-email cooldown on forgot-password. The unthrottled TOTP verify endpoint
is the sharper instance of this gap and is tracked separately as M3.

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
entry wins — so a *direct* client can spoof the identity seen by
resolver-backed consumers (the global rate limiter's IP keying, `ClientAddr` /
`ClientHost` / `ClientScheme` extractors, access logs) by sending its own
`X-Forwarded-For`. Account lockout is unaffected: its counters are
account-keyed and its IP telemetry reads unspoofable `ConnectInfo` (see L2).
The dev default (loopback-only) and prod
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

### L11. Reset tokens are not atomically consumed

Generated `reset_password` SELECTs the user by `reset_token_digest`, but the
subsequent transaction UPDATEs the row by `find(user_id)` only — it never
re-checks the digest. Two concurrent POSTs presenting the same valid token can
both pass the SELECT and both commit, with the second password overwriting the
first (each also rotating/revoking sessions). Exploitability is low — both
requests must already hold the secret token — but it breaks the single-use
invariant the magic-link and confirm-email flows enforce. Fix: add
`.filter(reset_token_digest.eq(&token_digest))` to the in-transaction UPDATE
(or consume via the same `UPDATE … RETURNING` pattern) and treat zero affected
rows as an invalid link. *(Credit: flagged by automated review on the audit PR
and verified against the code.)*

### L12. `confirm_email` consumes its token — and grants a session — on a bare GET

`GET /auth/confirm/{token}` atomically consumes the token (good) but does so
directly on the GET, unlike magic-link's scanner-safe non-consuming GET →
confirming POST. Consequences: (a) an email link-scanner or preview bot that
follows the URL burns the single-use token before the human clicks, leaving
them at the "invalid or expired" page and forcing a resend; (b) a
session-granting, CSRF-exempt-by-method GET is login-CSRF-shaped — a victim
who top-level-navigates an attacker's own confirm link is silently logged into
the attacker's account. Recommendation: adopt the magic-link pattern (GET
renders a confirm form, POST consumes), which fixes both. The email-change
variant (`confirm_email_change`) has the same GET-consumption shape and should
move with it. *(Credit: flagged by automated review on the audit PR and
verified against the code.)*

### L13. TOTP/passkey/email-change credential flows do not revoke remember chains

`change_password` and `reset_password` delete the user's remember-token rows
inside their transactions ("the old password is compromised"), but the other
credential-changing flows do not: `two_factor_confirm`, `two_factor_disable`,
`passkey_register_finish`, `passkey_revoke`, and `confirm_email_change` all
revoke tracked *session* rows (under `revoke_on_credential_change`) yet leave
the remember-token table untouched. A stolen remember cookie therefore
survives 2FA enrollment/disable and passkey add/remove and can re-establish a
login afterwards — inconsistent with the `[auth.sessions]` documentation,
which frames exactly these events as credential changes, and with
`change_password`'s own "chains are ALWAYS revoked on a credential change"
comment. Fix: add the same `{rem_table}` user-scoped delete to those five
transactions (optionally sparing the current device's chain, mirroring the
`token_digest.ne(current)` session carve-out). *(Credit: flagged by automated
review on the audit PR and verified against the code.)*

### L14. Generated TOTP code comparison is not constant-time

`verify_totp_code` compares `expected == candidate` with a plain string
equality. Practical exploitability is marginal — the code is 6 digits, rotates
every 30 s, and a matched step is single-use via the `totp_last_used_step`
replay guard — but it is inconsistent with the pipeline's own standard
(constant-time comparison everywhere else, including for values with similar
threat profiles). Fix: run the candidate through
`autumn_web::auth::constant_time_eq` against each window's expected code.
*(Credit: flagged by automated review on the audit PR and verified against the
code.)*

### L15. Lockout counter collapses concurrent failures right after cool-off

In the generated `login` handler, once a lock's cool-off has expired the local
state is reset (`current_attempts = 0`) and a failed attempt takes the reset
branch, which unconditionally writes `failed_attempts = 1`. Every concurrent
request that read the same expired-lock row takes that same branch and writes
the same `1` — so a parallel burst of N wrong-password attempts fired at the
cool-off boundary counts as a single failure instead of N. An attacker can
thereby stretch each cool-off cycle to (burst size + threshold) attempts
rather than threshold. The normal path's `failed_attempts + 1` DB-side
increment *is* atomic; only the reset branch collapses. Fix: make the reset
branch a guarded atomic transition too, e.g. `UPDATE … SET failed_attempts =
failed_attempts + 1, locked_at = NULL WHERE id = … AND locked_at <= expiry`
after first zeroing the counter in the unlock path, or fold reset-and-count
into one conditional increment. *(Credit: flagged by automated review on the
audit PR and verified against the code.)*

### L16. Lockout telemetry digest falls back to a public hard-coded salt

The `account_locked` telemetry event salts its `account_id_digest` with
`SECRET_KEY_BASE` or `AUTUMN_ADMIN_SECRET` — env vars that Autumn's canonical
config path (`AUTUMN_SECURITY__SIGNING_SECRET` / `autumn.toml`) never sets —
and otherwise uses the hard-coded literal `"autumn-lockout-fallback-salt"`.
With a public salt, sequential integer account ids, and only 8 digest bytes
logged, anyone with log access can enumerate `sha256(salt:id)` and reverse the
pseudonymization cheaply. Fix: salt from the app's resolved signing key
(`ResolvedSigningKeys`), which production boot already guarantees exists, and
drop the static fallback. *(Credit: flagged by automated review on the audit
PR and verified against the code.)*

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
  bounded TTLs, non-enumerating responses with 1s timing floors. Magic-link
  and confirm-email consume atomically (`UPDATE … RETURNING` on the digest);
  reset does not — see L11. Magic-link additionally uses the link-scanner-safe
  non-consuming GET → confirming POST pattern (confirm-email does not — see
  L12), re-checks lockout *after* token consumption to close the TOCTOU race,
  and is deliberately denied step-up freshness (email possession ≠ password
  knowledge).
- **Remember-me**: Jaspan series/token scheme, hash-at-rest, constant-time
  verify, CAS rotation with race re-evaluation, theft detection nukes the
  chain, password change/reset revoke remember chains (the TOTP, passkey, and
  email-change flows do not — see L13), `reauth_pw_ok`
  cleared so an email-only login can't shortcut a password reauth.
- **Lockout**: DB-side atomic increment on the normal failure path (the
  post-cool-off reset branch is not — see L15), non-enumerating lock
  responses, cool-off with clean re-entry, success-path guarded against
  concurrent locking, truncated IP prefix in telemetry (the digest salt has a
  weak fallback — see L16).
- **TOTP**: per-step replay guard (`totp_last_used_step` CAS); code
  comparison is not constant-time — see L14.
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
  manifest — every *dump-enumerated* route must be provably
  `framework`/`gated`/`public` or the build gate fails — with honest
  provenance tags (`provable` vs `declared` vs `runtime-only`). Coverage
  caveat: the opt-in serve-path HTTP surfaces (MCP, inbound-mail, storage,
  SEO) are injected only on the serve path, after the dump early-exit, so the
  gate cannot classify or fail on them; the manifest discloses this honestly
  in its `excluded` list (`serve_path_routers`, eventual provenance
  `provable`) rather than silently omitting it, and closing that gap is the
  natural next increment of the gate. Session cookie parsing is additionally
  fuzzed (`fuzz/fuzz_targets/session.rs`).

## Suggested fix order

1. M3 (throttle + attempt-bound the TOTP verify endpoint) — the one finding
   that weakens a security guarantee outright
2. M1 (CSRF cookie `Secure` + `__Host-`; session binding as follow-up)
3. M2 (constant-time admin-secret compare) — one-line fix
4. L11/L12/L13 (atomic reset-token consumption; confirm-email GET→POST;
   remember-chain revocation in the TOTP/passkey/email-change transactions) —
   small, invariant-restoring changes to the generated handlers
5. L1/L2 (route generated code through the ProxyResolver seams)
6. L3 (throttle login/forgot-password)
7. Remaining L-items opportunistically.
