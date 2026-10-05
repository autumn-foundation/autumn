### Changed

- **release:** release templates now use `/ready` for traffic checks and
  `/live` for liveness, not the `/health` alias (issue #3066). The ECS target
  group and the App Runner cutover check `/ready`, so a drain stops traffic
  first. The image `HEALTHCHECK` checks `/live`, so a database outage does not
  restart the container. Cloud Run gets a `/startup` startup probe and a
  `/live` liveness probe. Azure Container Apps gets a `/ready` readiness probe
  and a `/live` liveness probe. Files that you generated before this change
  keep the old paths. Change them by hand.

### Documentation

- **config:** `server.timeouts.request_timeout_ms` returns `503`, not `408`.
  The doc now says this.
- **jobs:** Postgres job workers poll every 200ms. They do not use
  `LISTEN`/`NOTIFY`. The jobs guide and the code comments now say this.

### Testing

- **verification:** a new `Verus proofs` workflow runs every spec in
  `verification/`. It is not a required check yet.
