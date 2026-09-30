### Changed

- **Tenant cells:** evicting a `TenantCell` (`TenantCellRegistry::evict`, or
  automatic `max_cells` / `idle_ttl_secs` eviction) no longer splits a live
  tenant's accounting domain. While a request still holds the evicted cell, the
  next `get_or_create` for that tenant rejoins the same quota counter and
  scratch store instead of minting a fresh, zero-usage cell — so eviction can
  no longer let one tenant hold two quota-sized generations at once. A fresh
  domain starts only after every handle has dropped. Dead lifecycle records are
  swept in bounded batches, so one-off tenant ids cannot grow the registry
  without bound. See `docs/guide/tenant-cells.md`.
