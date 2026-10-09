### Added

- **capsule:** `capsule::guard_egress` and `capsule::spawn` (issue #2351).
  `guard_egress` checks a call that does not go through the framework HTTP
  client: a replay refuses it, and capture marks the capsule incomplete.
  CAPTCHA, OAuth2, SES inbound mail, the S3 blob store and the `MediaMTX`
  client call it. `spawn` replaces `tokio::spawn` for detached work: capture
  marks the capsule incomplete, and a replay gives the task the tape.
- **capsule:** `autumn replay` installs a mailer. A mail-sending route reaches
  the mail seam and replays from the capsule; nothing is delivered.

### Fixed

- **capsule:** cache removals (`invalidate`, `invalidate_namespace`, `clear`)
  and the untyped `cache::get` / `cache::insert` are on the capsule seam. A
  replay never reaches the installed cache backend.
- **capsule:** a replay that reads a cache key the recording only wrote, makes
  a tenant lookup the recording did not make, or draws random bytes of a
  different width now diverges.
- **capsule:** a plain `enqueue` no longer consumes a recorded delayed entry,
  and an `enqueue_at` with a past deadline replays clean.
- **capsule:** a failed `*_after_commit` registration is recorded as the
  failure it was. With no open transaction it is recorded once, not twice.
- **capsule:** a masked cache write no longer refuses the capsule. An echo mask
  over a response, a cache hit or a job payload now refuses it.
- **capsule:** a capsule whose compared data held the literal text
  `[FILTERED]` is refused, as replay reads that text as a wildcard.
- **capsule:** a secret that reached the outcome through an effect is masked
  in the replayed outcome, so unchanged code reproduces and the value is not
  printed.
- **capsule:** a replayed job runs in the event, transaction-timeout and job
  contexts a production run gets. A tracked job's capsule is refused.
- **capsule:** the alternate mail body, every mail header and an enqueue error
  count against the capsule size limits.
- **jobs:** the job capsule filter is built once per app, not per execution.

### Breaking Changes

- **Breaking:** `capsule::CacheEffect` has new variants `Invalidate`,
  `InvalidateNamespace` and `Clear`; `capsule::EffectSeam` has a new variant
  `Random`; `capsule::JobEffect` has a new public field `requested_due_at`
  ([migration guide](docs/migrations/next.md)).
