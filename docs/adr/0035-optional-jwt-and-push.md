# ADR 0035: Put `jsonwebtoken` and `p256` Behind Default-On Features

- Status: Accepted
- Date: 2026-11-03
- Deciders: Autumn maintainers
- Tags: build-time, features, supply-chain

## Context

`jsonwebtoken` (with its RustCrypto backend) and `p256` were unconditional
dependencies of `autumn-web`. Every app compiled them, whether or not it
verified a JWT or sent a Web Push.

`jsonwebtoken` compiles `rsa` 0.9, which has RUSTSEC-2023-0071 and no fixed
release. It also compiles `p256`/`p384` 0.13 and so `elliptic-curve` 0.13,
which carries `crypto-bigint` 0.5. That has RUSTSEC-2026-0387, fixed only in
0.7.6, which `elliptic-curve` 0.13 cannot take. An app that used neither JWT
nor push still had to waive both advisories in its `cargo deny` policy, for
code it never runs.

Autumn's own `p256` (Web Push) is 0.14 and carries neither advisory. It is
gated for build size, and so that an app can leave out the whole curve stack.

## Decision

Add two features, on the pattern of ADR 0027. Both are in `default`, so a
default build does not change.

| Feature | Owns | Covers |
|---------|------|--------|
| `jwt` | `dep:jsonwebtoken` | The `jwt` tenancy source. The OIDC `id_token` check, through `oauth2 = ["http-client", "jwt"]`. |
| `push` | `dep:p256` | The `push` module, `WebPush` and `PushMessage` in the prelude, `AppBuilder::with_web_push`, `AppBuilder::with_push_subscription_store`, the matching `TestApp` methods, and the `PushError` status mapping. |

Unlike ADR 0027's `[alerts]` and `[[slo]]`, which log a warning when their
feature is off, these two refuse the config that needs them. A push section
that does nothing means an app that records subscriptions and delivers
nothing. JWT tenancy that does nothing means every request fails, or worse,
a tenant read from an unverified token. So:

- `[push]` still parses without `push`. It is kept as an opaque table whose
  `Debug` shows only key names. An `AUTUMN_PUSH__*` variable records its key
  in that table, without the value. `AutumnConfig::validate_push` then fails,
  and `run` exits with code 1 before it binds.
- `AutumnConfig::validate_tenancy_source` fails when tenancy is enabled with
  `source = "jwt"` and `jwt` is off, and `run` exits with code 1.
- Tenant resolution has its own arm for `jwt` without the feature. It returns
  a 500, so a router built without `run` fails closed too.

## Consequences

- A default build does not change.
- An app with `default-features = false` that uses JWT tenancy, OIDC or Web
  Push must name the feature. The migration guide has the entry.
- An app that leaves out `jwt` and `oauth2` compiles neither `rsa` (unless it
  turns on `webauthn` or `inbound-ses`) nor `crypto-bigint` 0.5, and can drop
  both waivers. The scaffold keeps default features, so its `deny.toml` keeps
  them.
- The CLI turns on `push` (for `autumn generate pwa`) and not `jwt`.
- The `jwt-push` lane of `test-features` checks each combination, checks the
  dependency graph of each, and runs the specs with both features off.
