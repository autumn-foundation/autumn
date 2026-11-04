### Breaking Changes

- **Breaking:** 16 modules of `autumn-web` now sit behind default-on features
  (issue #2795, ADR 0027): `alerts`, `custom-domains`, `data-retention`,
  `experiments`, `feature-flags`, `gdpr`, `graph`, `idempotency`,
  `job-tracking`, `ledger`, `money`, `nested-form`, `slo`, `upgrade`,
  `version-history` and `wizard`. A default build does not change. An app with
  `default-features = false` must name the features it uses
  ([migration guide](docs/migrations/next.md)).

### Performance

- **build:** the no-DB starter no longer builds these modules, the `psl`
  crate, or the route graph descriptors that `#[get]` and `#[job]` emitted.
