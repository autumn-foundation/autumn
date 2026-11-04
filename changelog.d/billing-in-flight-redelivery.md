### Breaking Changes

- **Breaking:** `EventClaim::Duplicate` is split into `Applied` and `InFlight`
  (issue #3097). `EventClaim` is `#[non_exhaustive]`, and `ReconcileOutcome`
  has an `InFlight` variant ([migration guide](docs/migrations/next.md)).

### Fixed

- **billing:** a webhook redelivery that arrives while the first attempt is
  still being applied now gets `503`, not `200` (issue #3097). Before, a
  failed first attempt could lose the event.
