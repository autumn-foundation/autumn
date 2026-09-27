### Security

- **`cache_fragment`/`cache_fragment_global` (and their `_in` namespaced
  variants) now fold the ambient resolved tenant into their cache key:** the
  key was built exclusively from the caller-supplied `identity`/`version`
  pair against the **process-global** cache backend, and never consulted the
  `CURRENT_TENANT` task-local every other tenant-scoped Autumn primitive
  resolves ambiently (`tenant_scoped` repository finders, `save()`, preload,
  retention sweeps, and — since the 2026-09-05 fix — `#[cached]` itself). An
  app that follows `docs/guide/fragment-caching.md`'s own Quick Start
  (`format_args!("post_card:{}", post.id)` as the identity) on a sharded,
  `tenant_scoped` deployment could have two different tenants' rows collide
  on `(identity, version)`: `docs/guide/sharding.md`'s own resharding runbook
  documents that a sharded table's primary key is a **shard-local**
  `BIGSERIAL` — every shard hands out its own `1, 2, 3, …` independently —
  and `docs/guide/conditional-get.md`'s idiomatic `#[lock_version]` version
  token starts at the same initial value for every fresh row regardless of
  tenant. Two tenants' first row of the same model can therefore land on the
  exact same `(identity, version)` deterministically, the instant both exist,
  purely from following the framework's own documented patterns: tenant B
  would receive **tenant A's cached fragment**. `cache_fragment`/
  `cache_fragment_global` now read `CURRENT_TENANT` and fold it into the key
  unconditionally, ahead of `identity`/`version`. Apps without tenancy
  enabled, or calling these helpers outside a request (a background job, a
  scheduled sweep, a non-tenant-scoped app), compute the same key as before —
  `CURRENT_TENANT` resolves to `None` in both cases.
  **Compatibility note:** a fragment that intentionally serves one shared,
  cross-tenant value (a genuinely global computation, not tenant-varying
  content) now partitions its cache per resolved tenant too when rendered
  from within a tenant's request — a harmless drop in hit rate, not a
  correctness change, since the rendered markup does not vary by tenant. See
  `docs/security/2026-09-21-fragment-cache-tenant-key/`.
