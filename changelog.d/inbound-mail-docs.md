### Documentation

- **mail:** the mail guide now documents **receiving** email, not just sending.
  `docs/guide/mail.md` gains a "Receiving Mail (Inbound Email)" section covering
  the Mailgun/SES/generic webhook endpoints, the `InboundMailRouter`, recipient
  patterns and plus-address reply routing, sync vs background processing, and
  bounce/spam handlers. `autumn generate inbound-mail` joins
  `docs/guide/generators.md`. Eight reader questions — "inbound email", "email
  webhook", "reply by email", "mailgun", "ses" among them — landed on no page at
  all before this, except "inbound mail", which landed on the macro-expansion
  reference.
- **inbound-mail:** the new section also states two things no inbound-mail doc
  said before: Mailgun's 5-minute timestamp check is a freshness bound, not
  replay protection — nothing de-duplicates, so side-effecting handlers must be
  idempotent (unlike `SignedWebhook`, which does keep replay markers) — and a
  signing key applies only to Mailgun and generic endpoints, since `build_routes`
  never passes one to an SES route. `autumn generate inbound-mail` scaffolds a
  Mailgun endpoint only; there is no provider flag.

### Fixed

- **inbound-mail:** corrected three claims in the `InboundMailEndpointConfig`
  SES docs. `topic_arn` and `::ses()` said the SNS topic check was optional ("Leave
  `None` to skip the topic check", "For production use…"); an SES endpoint
  without `.with_topic_arn(...)` fails closed and answers `503` to every
  request, in every profile. `::ses()` also said SNS authenticity was "verified
  via the `X-Amz-Sns-Message-Type` header" — it is verified by the RSA
  `Signature` against the certificate at `SigningCertURL`, and that header is
  read nowhere in the crate.
- **inbound-mail:** `InboundMailEndpointConfig::generic()`'s docs described its
  HMAC signing as "optional" without saying that the constructor sets neither
  key field, so the endpoint it returns verifies nothing and dispatches every
  body it receives. Both the rustdoc and the new guide section now state the
  unauthenticated default outright.
- **cli:** `autumn generate inbound-mail --help` said it adds the `inbound-mail`
  Cargo feature; it adds `inbound-mailgun`.
