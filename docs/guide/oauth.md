# Sign in with OAuth2 / OIDC

Autumn supports first-class OAuth2 and OpenID Connect (OIDC) login via the `--oauth` flag on `autumn generate auth`.

OAuth is one identity source among several. For the session, password, and
route-guard machinery every sign-in path shares — including what a successful
OAuth callback ultimately writes into the session — start from the
[authentication guide](./authentication.md).

## Quick start

```sh
autumn generate auth User --oauth github,google
```

This generates:

- `src/routes/oauth.rs` — redirect and callback handlers
- A migration for the `oauth_identities` table
- Login-page buttons for each listed provider
- `autumn.toml` stubs for `[auth.oauth2.github]` and `[auth.oauth2.google]`

## Provider presets

Autumn ships built-in presets for the three most common providers.  
Add the provider name to `autumn.toml` and fill in your credentials:

```toml
[auth.oauth2.github]
client_id     = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"   # use env var in production
authorize_url = "https://github.com/login/oauth/authorize"
token_url     = "https://github.com/login/oauth/access_token"
userinfo_url  = "https://api.github.com/user"
redirect_uri  = "https://yourapp.com/auth/github/callback"
scope         = "read:user user:email"

[auth.oauth2.google]
client_id     = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"
authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url     = "https://oauth2.googleapis.com/token"
userinfo_url  = "https://openidconnect.googleapis.com/v1/userinfo"
redirect_uri  = "https://yourapp.com/auth/google/callback"
scope         = "openid email profile"
issuer        = "https://accounts.google.com"
jwks_url      = "https://www.googleapis.com/oauth2/v3/certs"
discovery_url = "https://accounts.google.com"

# Microsoft — single-tenant: replace {YOUR_TENANT_ID} with your Directory (tenant) ID.
# Multi-tenant: see the note on issuer validation below.
[auth.oauth2.microsoft]
client_id     = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"
authorize_url = "https://login.microsoftonline.com/{YOUR_TENANT_ID}/oauth2/v2.0/authorize"
token_url     = "https://login.microsoftonline.com/{YOUR_TENANT_ID}/oauth2/v2.0/token"
redirect_uri  = "https://yourapp.com/auth/microsoft/callback"
scope         = "openid email profile"
issuer        = "https://login.microsoftonline.com/{YOUR_TENANT_ID}/v2.0"
jwks_url      = "https://login.microsoftonline.com/{YOUR_TENANT_ID}/discovery/v2.0/keys"
```

| Provider  | Protocol | Notes                        |
|-----------|----------|------------------------------|
| Google    | OIDC     | ID token validated via JWKS; all required endpoints are stable |
| GitHub    | OAuth2   | No ID token; uses `/user` userinfo endpoint |
| Microsoft | OIDC     | **Single-tenant**: use your tenant-specific endpoints and issuer. **Multi-tenant** (`/common`): the ID-token `iss` claim is tenant-specific and will not match the common issuer — issuer validation must be relaxed or performed after decoding the `tid` claim |

## Registering redirect URIs

Each provider requires you to allowlist your callback URL before login works.

**GitHub** — Settings → Developer settings → OAuth Apps → New OAuth App  
Callback URL: `https://yourapp.com/auth/github/callback`

**Google** — Google Cloud Console → APIs & Services → Credentials → OAuth 2.0 Client  
Authorized redirect URI: `https://yourapp.com/auth/google/callback`

**Microsoft** — Azure portal → App registrations → Redirect URIs  
Platform: Web, URI: `https://yourapp.com/auth/microsoft/callback`

For local development use `http://localhost:3000/auth/<provider>/callback`.

## Security properties

### PKCE (S256)

Every authorization request uses PKCE with the S256 challenge method.  
The `code_verifier` (32 random bytes, base64url-encoded) is stored in the session; the `code_challenge = BASE64URL(SHA256(code_verifier))` is sent in the authorization URL.  
This prevents authorization code interception even if the callback URL is compromised.

### State anti-CSRF

A random `state` token is generated per-request and stored in the session.  
The callback handler validates it with a constant-time comparison (`subtle::ConstantTimeEq`).  
A mismatch returns a generic error — the offending value is never logged.

### Nonce (for OIDC)

When using OIDC providers (Google, Microsoft) a random `nonce` is embedded in the ID token and verified on the callback, preventing token-replay attacks.

## Account linking

`oauth2_finish_login` validates the OAuth2 flow (PKCE, state, nonce, ID-token signature) and returns an `OAuthIdentity` containing the provider name and the provider's stable user identifier (`subject`).  
**Account creation is not automatic** — you must implement it in the callback handler after receiving the identity:

```rust
let identity = oauth2_finish_login(&session, &provider_name, &provider, &callback).await?;

// Look up or create the local user, then set the application session:
let user_id = upsert_oauth_user(&mut db, &identity).await?;
session.insert(&auth_cfg.session_key, user_id).await;
```

The `oauth_linking_policy` field in `autumn.toml` communicates intent to `autumn doctor`:

- `create_account` (default) — doctor expects the callback creates a new account on first sign-in
- `require_local_signup_first` — doctor expects the callback rejects unknown identities

```toml
[auth]
oauth_linking_policy = "require_local_signup_first"
```

## Database schema

The generator creates an `oauth_identities` migration:

```sql
CREATE TABLE oauth_identities (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider   TEXT      NOT NULL,
    subject    TEXT      NOT NULL,   -- provider's stable user identifier
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    UNIQUE (provider, subject)
);
```

One user can have multiple OAuth identities (e.g., sign in with both GitHub and Google).

## autumn doctor --strict

`autumn doctor --strict` fails when:

- `client_secret` is empty for any configured `[auth.oauth2.*]` provider **in production**

It warns (non-fatal) in development so the check still appears in the output.

Set secrets via environment variables to keep them out of `autumn.toml`:

```sh
export AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRET="ghp_..."
export AUTUMN_AUTH__OAUTH2__GOOGLE__CLIENT_SECRET="..."
```

## Middleware

Generated callback routes are automatically opted into the CSRF, session, and security-headers middleware stacks.  No manual wiring is required.

## Opting out

The entire OAuth2 subsystem is gated behind the `oauth2` Cargo feature flag.  
If you never generate `--oauth` handlers the feature is never enabled and no OAuth2 code is compiled into your binary.
