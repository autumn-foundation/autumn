### Breaking Changes

- **Breaking:** `jsonwebtoken` and `p256` are optional, behind the default-on
  `jwt` and `push` features. `jwt` covers the `jwt` tenancy source and the
  OIDC `id_token` check, and `oauth2` turns it on. `push` covers
  `autumn_web::push`, `[push]`, `with_web_push` and
  `with_push_subscription_store`. A default build does not change. An app with
  `default-features = false` that uses JWT tenancy, OIDC or Web Push must name
  the feature ([migration guide](docs/migrations/next.md)).

### Security

- **deps:** an app that leaves out `jwt` (and `oauth2`) no longer compiles
  `rsa` 0.9 (RUSTSEC-2023-0071) or `crypto-bigint` 0.5 (RUSTSEC-2026-0387), and
  can drop both waivers from its `deny.toml`. A build without a feature refuses
  `[push]`, any `AUTUMN_PUSH__*` variable, or `tenancy.source = "jwt"` at boot,
  and a router built without `run` fails closed on `jwt` tenancy with a 500.
