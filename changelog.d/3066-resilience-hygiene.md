### Changed

- **release:** release templates now use `/ready` for traffic checks and
  `/live` for liveness, not the `/health` alias (issue #3066). The ECS target
  group and the App Runner cutover now check `/ready`. `/health` is only an
  alias of `/ready`. The image `HEALTHCHECK` checks `/startup`: it waits for
  startup to complete, and Docker Swarm does not replace containers during a
  database outage. Cloud Run gets a `/ready` startup probe (its traffic gate)
  and a `/live` liveness probe. Azure Container Apps gets a
  `/startup` startup probe, a `/ready` readiness probe and a `/live` liveness
  probe. Files that you generated before this change
  keep the old paths. Change them by hand.

### Documentation

- **config:** a request that exceeds `server.timeouts.request_timeout_ms` gets
  `503`, not `408`. The `RequestTimeoutsConfig` doc now says this.
- **jobs:** an idle Postgres job worker polls every 200ms. Workers do not use
  `LISTEN`/`NOTIFY`. The jobs guide and the code comments now say this.

### Testing

- **verification:** a new `Verus proofs` workflow runs every spec in
  `verification/`. It is not a required check yet.
