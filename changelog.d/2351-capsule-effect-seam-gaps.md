### Added

- **capsule:** `capsule::guard_egress`, `capsule::UnrecordedEgress` and
  `capsule::spawn` (issue #2351). `guard_egress` checks a call that does not go
  through the framework HTTP client. A replay refuses the call, and capture
  marks the capsule incomplete. CAPTCHA, `OAuth2`, SES inbound mail,
  `autumn-storage-s3` and the media plugin call it. `spawn` replaces
  `tokio::spawn` for detached work. Capture marks the capsule incomplete. A
  replay logs a divergence and gives the task the tape.
- **capsule:** `autumn replay` keeps startup code off live services. Cache
  backends stay offline, a mail send with no tape is refused, and a task that
  startup code spawns gets a tape that refuses every call.
- **capsule:** `autumn replay` installs a mailer. A mail-sending route reaches
  the mail seam and replays from the capsule. Nothing is delivered.
- **capsule:** a capsule records whether the app builder installed a cache
  (`with_cache_backend`). `autumn replay` then installs a cache that stores
  nothing in the same places, so a cache call takes the production path and
  the capsule answers it.
- **capsule:** `CacheInvalidationError`, `CacheEffect::is_read`,
  `CacheEffect::pending`, `OLDEST_READABLE_FORMAT_VERSION`,
  `ReplayEffects::for_format_version` and `ReplayEntropy::width_mismatches`.

### Changed

- **capsule:** the capsule format version is 4. This build still reads a
  version 3 capsule, and replays it with the version 3 rules.
- **capsule:** capture marks a capsule incomplete, so replay refuses it, when
  the run starts detached work (`db::register_after_commit`, a
  stale-while-revalidate refresh), runs a tracked job, makes egress outside
  the HTTP seam, reads or writes an untyped cache value, or has a cache fill
  that a distributed fill lock or the shared fill fence stops.

### Fixed

- **capsule:** cache removals (`invalidate`, `invalidate_namespace`, `clear`)
  and the untyped `cache::get` / `cache::insert` are on the capsule seam. A
  replay does not reach the installed cache backend. A coherence
  `invalidate_namespace` records its combined answer, so a registered store
  that failed replays as the `false` production returned.
- **capsule:** a replay now diverges when it reads a cache key that the
  recording only wrote, makes a tenant lookup the recording did not make, or
  draws random bytes of a different width.
- **capsule:** a plain `enqueue` no longer consumes a recorded delayed entry.
  An `enqueue_at` with a past deadline now replays clean.
- **capsule:** capture records a failed `*_after_commit` registration as a
  failure. With no open transaction, capture records it once, not twice.
- **capsule:** a masked cache write no longer refuses the capsule. Replay now
  refuses a capsule when the echo mask changed a response, a cache hit or the
  job's own payload.
- **capsule:** replay refuses a capsule when compared data held the literal
  text `[FILTERED]`. Replay reads that text as a wildcard.
- **capsule:** replay masks a secret that reached the outcome through an
  effect. Unchanged code then reproduces, and the verdict does not show the
  secret.
- **capsule:** a replayed job runs in the event, transaction-timeout and job
  contexts of a production run.
- **capsule:** the alternate mail body, every mail header and an enqueue error
  count against the capsule size limits.
- **jobs:** a job id is no longer drawn from the capsule's random tape. The
  job capsule filter is built once per app, not per execution.

### Breaking Changes

- **Breaking:** `capsule::CacheEffect` has new variants `Invalidate`,
  `InvalidateNamespace` and `Clear`; `capsule::EffectSeam` has new variants
  `Random` and `Detached`; `capsule::schema::MailErrorKind` has a new variant
  `NoDurableQueueInProduction`; `capsule::JobEffect` has new public fields
  `requested_due_at` and `error_status`; `capsule::CapsuleEffects` has a new public field
  `state_cache`
  ([migration guide](docs/migrations/next.md)).
