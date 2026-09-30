### Fixed

- **`autumn db retention --dataset custom_domains`:** the one-shot retention
  path now installs the same custom-domain pruner the server installs, so it
  actually reports and purges eligible records instead of always saying
  "custom domains are not enabled" (issue #2652). The extracted
  `PruneOnlyCustomDomainPruner` carries no ACME issuer — pruning only touches
  the registry and the certificate store, so no order is placed and no CA is
  contacted — and keeps every safeguard the task had: it refuses to prune when
  the registry failed to hydrate, re-asserts each stale `pending_dns` record
  inside the registry's per-hostname gate, enumerates the certificate store
  before snapshotting the registry, deletes keys before chains, and never
  deletes the deployment's own certificate.
