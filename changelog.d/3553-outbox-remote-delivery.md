### Added

- **outbox:** delivery to another service. The inbox of the receiver drops
  copies. `[outbox.destinations.<name>]` routes topics to the
  `/_autumn/inbox` of another service. The relay POSTs each message signed
  with `Autumn-Signature` and keeps its rules: commit with the data, at least
  once, order per aggregate, retry with jitter, dead letters and `requeue`.
  `[outbox.inbox]` serves the endpoint. It verifies the signature, records
  the message id with `Inbox::seen` and hands the message to each
  `#[listener]` of the topic in one transaction, so a copy has no second
  effect. The inbox returns `503` for a topic with no listener. The relay
  then sends the message again. A
  destination topic must be an `#[event(name = "...")]`, so
  `autumn contracts check` covers it. Delivery runs only on roles with the
  `outbox_relay` capability. See `docs/guide/outbox.md`.

### Breaking Changes

- **Breaking:** `OutboxConfig` has the new fields `destinations` and `inbox`.
  A struct literal with no `..` spread must add them
  ([migration guide](docs/migrations/next.md)).
