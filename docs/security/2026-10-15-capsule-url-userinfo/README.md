# Capsule: outbound URL userinfo reached disk

- **Surface:** `capsule::redact::redact_effects` -> `redact_url_query` (outbound `url` and `final_url`).
- **Threat model:** against an app with `[failure_capture]` on that calls a downstream service with a credential in URL userinfo (`https://user:secret@host/..`, which the client sends as Basic auth), anyone who later reads the capsule file (support, CI artifact, shared bug report) obtains that credential. The `Authorization` header was masked; the same credential in the authority was not.
- **Reproduction:** `cargo test -p autumn-web --features "test-support,reporting,http-client,cache-moka" --test integration_tests failure_capsule_url_userinfo`. Trunk output: `trunk-failure.txt`; fixed: `after.txt`.
- **Root cause:** `redact_url_query` masked only the query string.
- **Fix:** `mask_url_userinfo` blanks the whole userinfo, retains raw and decoded user/password in `RedactedValues`, and records `http[n].url:userinfo` in `redacted_keys`. Replay matches the `[FILTERED]` placeholder as for other masked URL text.
- **Variant sweep:** inbound request URI (absolute-form targets) not sent with userinfo by clients and not a documented path; mail/job/cache fields are free-form and covered by the echo pass once userinfo is retained. URL path segments remain an openly documented exclusion. The DB-URL tape was not examined here.
- **Compatibility:** capsule contents only; no API change. Changelog fragment added.
